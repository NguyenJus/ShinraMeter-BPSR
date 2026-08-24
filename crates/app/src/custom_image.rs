//! Runtime-loaded user background images (issues #121 and #253).
//!
//! Every other texture the overlay paints is `include_bytes!`-compiled into
//! the binary and decoded once by [`crate::icons`], so nothing here existed
//! before these two issues: a user-supplied header background (#121) and a
//! user-supplied backdrop behind the row list (#253) are the first assets
//! that come off disk at runtime, at a size nobody knew at build time.
//!
//! The two features share one path deliberately (#253 asks for exactly
//! that): one decoder, one cache, one failure story, one set of geometry
//! helpers. Only the [`ImageSlot`] differs.
//!
//! ## Shape of the pipeline
//!
//! Read the file -> decode it (`image`, the crate `icons.rs` already uses)
//! -> centre-crop the source to the destination's aspect ratio
//! ([`cover_crop`]) -> resize that crop to the destination's exact pixel
//! size -> upload one `egui::TextureHandle`. Painting is then a plain
//! full-UV blit, because the crop and the scale were both baked into the
//! texture: no per-frame work at all beyond one cache-key comparison.
//!
//! ## Cache key
//!
//! `(path, destination size in physical pixels, bucketed)`. A changed path
//! obviously invalidates; a changed destination size does too, because the
//! texture was resized *to* that size. The bucketing ([`TARGET_BUCKET`]) is
//! what keeps a window-resize drag from re-decoding on every one of the ~10
//! frames a second it produces — without it a slow drag across 400pt would
//! re-read and re-scale the file hundreds of times.
//!
//! ## Failure
//!
//! Never panics and never propagates: a missing, unreadable, or
//! undecodable file is cached as an [`ImageError`] under the same key, so
//! the failure is attempted exactly once rather than once per frame. The
//! caller paints its default artwork instead (the compiled-in gradient for
//! the header, the bare panel fill for the row list) and the settings
//! dropdown surfaces the message next to the offending path, so a typo in
//! a hand-edited `settings.json` is visible rather than silent.

use std::fmt;
use std::path::{Path, PathBuf};

use eframe::egui;

/// Which of the two customizable regions an image belongs to. The whole
/// module is written once and parameterized by this rather than duplicated
/// per region — #253 is explicit that the two features share their loading
/// infrastructure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSlot {
    /// Issue #121: behind the header band, replacing `ui::draw_header_wash`'s
    /// compiled-in gradient and oversized emblem.
    Header,
    /// Issue #253: behind the player-row list, over the panel fill.
    Backdrop,
}

impl ImageSlot {
    /// Both slots, for callers that act on the whole cache (the settings
    /// dropdown builds one row per slot; "Reset to defaults" clears them
    /// all). An array rather than an iterator so adding a third region is
    /// a one-line change that the compiler then chases through every
    /// exhaustive `match` in this module.
    pub const ALL: [ImageSlot; 2] = [ImageSlot::Header, ImageSlot::Backdrop];

    /// The label the settings dropdown puts on this slot's row. Deliberately
    /// says "header"/"rows" rather than naming the issues, since it is user
    /// -facing text.
    pub fn label(self) -> &'static str {
        match self {
            ImageSlot::Header => "Header image",
            ImageSlot::Backdrop => "Row backdrop",
        }
    }

    /// The `egui` texture name. Distinct per slot so the two never collide
    /// in the texture manager, and prefixed like `icons.rs`'s own names.
    fn texture_name(self) -> &'static str {
        match self {
            ImageSlot::Header => "custom-image-header",
            ImageSlot::Backdrop => "custom-image-backdrop",
        }
    }
}

/// The file extensions the picker offers, in the order it offers them.
///
/// This is exactly what the `image` crate is compiled to decode for this
/// binary (`image`'s `png`/`jpeg`/`bmp`/`gif`/`webp` features, see
/// `Cargo.toml`) — all five are pure-Rust decoders, so they cross-compile
/// to `x86_64-pc-windows-gnu` with no system library. The filter is a
/// convenience, not the gate: the decoder sniffs the *content*
/// ([`decode_and_fit`] calls `image::load_from_memory`, which guesses the
/// format from magic bytes), so a mislabelled `.png` that is really a JPEG
/// still loads, and a `.png` that is really a text file still fails
/// cleanly.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp"];

/// Why a configured image is not on screen. Two variants because the two
/// causes need different words in front of the user: "I could not open
/// that file" (moved, renamed, on a disconnected drive, permissions) and "I
/// opened it and it is not an image I can read".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// `std::fs::read` failed.
    Unreadable(String),
    /// The bytes were read but are not a decodable image, or decode to a
    /// zero-sized one.
    Undecodable(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Unreadable(err) => write!(f, "could not be read ({err})"),
            ImageError::Undecodable(err) => write!(f, "is not a readable image ({err})"),
        }
    }
}

/// Quantum, in physical pixels, that a destination size is rounded *up* to
/// before it becomes part of the cache key. See the module doc: this is the
/// difference between a window-resize drag costing one re-decode per
/// 64px step and one per frame.
const TARGET_BUCKET: u32 = 64;

/// Ceiling on either side of the uploaded texture. The destination region
/// is a panel a few hundred points across, so this is never reached in
/// practice by the region — it exists so a pathological
/// `pixels_per_point`/region combination cannot ask the GPU for a texture
/// larger than the 4096 side that is universally supported.
const MAX_TEXTURE_SIDE: u32 = 4096;

/// Floor on either side. `image`'s `resize_exact` and `crop_imm` are not
/// defined for a zero dimension, and a region *can* legitimately measure
/// zero for a frame (a collapsed window, the frame before layout settles),
/// so every size that reaches the decoder is clamped up to this first.
const MIN_TEXTURE_SIDE: u32 = 1;

/// Turns a destination region measured in egui points into the physical
/// pixel size the texture is decoded to, bucketed for the cache key.
///
/// `points_per_pixel` is `egui::Context::pixels_per_point` — decoding at
/// logical size and letting the GPU upscale would visibly soften the image
/// on the 125%/150% displays this overlay is normally used on.
///
/// Pure, so the rounding and clamping are testable without a live context.
/// Non-finite or negative input (which `Rect::size` can produce for an
/// unset rect) collapses to [`MIN_TEXTURE_SIDE`] rather than panicking in
/// the `as u32` cast.
pub fn target_pixels(size: egui::Vec2, points_per_pixel: f32) -> [u32; 2] {
    let scale = if points_per_pixel.is_finite() && points_per_pixel > 0.0 {
        points_per_pixel
    } else {
        1.0
    };
    [size.x, size.y].map(|side| {
        let px = side * scale;
        // NaN and non-positive both mean "no region to fill this frame";
        // an overflow to `+inf` means the opposite, so the two degenerate
        // cases have to land at opposite ends rather than share a branch.
        if px.is_nan() || px <= 0.0 {
            return MIN_TEXTURE_SIDE;
        }
        if px.is_infinite() {
            return MAX_TEXTURE_SIDE;
        }
        // `div_ceil` then multiply: round *up* to the bucket, so the cached
        // texture is never smaller than the region it fills (which would
        // upscale and blur). Saturating, so an absurd float cannot wrap.
        let px = (px.ceil() as u32).min(MAX_TEXTURE_SIDE);
        px.div_ceil(TARGET_BUCKET)
            .saturating_mul(TARGET_BUCKET)
            .clamp(MIN_TEXTURE_SIDE, MAX_TEXTURE_SIDE)
    })
}

/// The centred sub-rect of a `src`-sized source image that has `dst`'s
/// aspect ratio, as `[x, y, width, height]` in source pixels.
///
/// This is the "cover" fit both issues ask for ("resized (scaled/cropped)
/// to fit the region"): the whole destination is filled, the source's
/// aspect ratio is preserved, and the overflowing axis is trimmed equally
/// from both ends rather than squashed. A source wider than the
/// destination loses its left and right edges; a taller one loses its top
/// and bottom.
///
/// Pure integer geometry with no dependency on `image` or `egui`, so every
/// aspect-ratio case is unit-testable. Comparisons are done in `u64` — the
/// cross-multiplication `src.w * dst.h` overflows `u32` for any image past
/// ~65k pixels on a side.
///
/// Zero-sized input in either argument is not meaningful; the result is
/// clamped so the returned width and height are always at least 1 and
/// never exceed `src`, which is what `crop_imm` requires.
pub fn cover_crop(src: [u32; 2], dst: [u32; 2]) -> [u32; 4] {
    let (sw, sh) = (src[0].max(1) as u64, src[1].max(1) as u64);
    let (dw, dh) = (dst[0].max(1) as u64, dst[1].max(1) as u64);

    // `sw/sh > dw/dh` without the division: the source is proportionally
    // wider than the destination, so height is the binding axis and width
    // is what gets trimmed.
    if sw * dh > dw * sh {
        let width = (sh * dw / dh).clamp(1, sw);
        [((sw - width) / 2) as u32, 0, width as u32, sh as u32]
    } else {
        let height = (sw * dh / dw).clamp(1, sh);
        [0, ((sh - height) / 2) as u32, sw as u32, height as u32]
    }
}

/// Decodes `bytes` and returns an `egui`-ready image of exactly `dst`
/// pixels, cover-cropped per [`cover_crop`].
///
/// Split from the file read so the decode half is testable from a byte
/// slice, and separate from any texture upload so it needs no live
/// `egui::Context`.
///
/// `CatmullRom` rather than `Lanczos3`: this runs on the UI thread the
/// frame the user picks a file (or the frame a resize crosses a bucket
/// boundary), and at these sizes the two are visually indistinguishable
/// behind the scrim the caller paints over the result.
fn decode_and_fit(bytes: &[u8], dst: [u32; 2]) -> Result<egui::ColorImage, ImageError> {
    let decoded =
        image::load_from_memory(bytes).map_err(|err| ImageError::Undecodable(err.to_string()))?;
    let src = [
        image::GenericImageView::width(&decoded),
        image::GenericImageView::height(&decoded),
    ];
    if src[0] == 0 || src[1] == 0 {
        return Err(ImageError::Undecodable("it has no pixels".to_string()));
    }

    let dst = dst.map(|side| side.clamp(MIN_TEXTURE_SIDE, MAX_TEXTURE_SIDE));
    let [x, y, w, h] = cover_crop(src, dst);
    let rgba = decoded
        .crop_imm(x, y, w, h)
        .resize_exact(dst[0], dst[1], image::imageops::FilterType::CatmullRom)
        .to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        &rgba.into_raw(),
    ))
}

/// Reads and decodes `path` to exactly `dst` pixels. The whole fallible
/// half of the pipeline in one function, with no `egui::Context` in sight,
/// which is what makes the graceful-degradation path testable on a headless
/// CI host.
pub fn load(path: &Path, dst: [u32; 2]) -> Result<egui::ColorImage, ImageError> {
    let bytes = std::fs::read(path).map_err(|err| ImageError::Unreadable(err.to_string()))?;
    decode_and_fit(&bytes, dst)
}

/// One slot's cached load: the key it was loaded under, and what came of
/// it. The `Err` arm is cached exactly like the `Ok` arm on purpose — see
/// the module doc's failure section. A missing file is retried when the
/// user re-picks (which clears the entry) or when the region size crosses a
/// bucket, not ten times a second forever.
struct Entry {
    path: PathBuf,
    dst: [u32; 2],
    result: Result<egui::TextureHandle, ImageError>,
}

/// The whole runtime-image cache: at most one live texture per slot.
///
/// Lives inside `ui::Icons` (behind a `RefCell`, since every painter in
/// `ui.rs` holds `&Icons`), which is what lets the header wash and the row
/// backdrop reach it without threading a new `&mut` parameter through the
/// twenty-odd call sites `draw_header`/`draw_header_menu` already have.
#[derive(Default)]
pub struct CustomImages {
    header: Option<Entry>,
    backdrop: Option<Entry>,
}

impl CustomImages {
    fn slot(&mut self, slot: ImageSlot) -> &mut Option<Entry> {
        match slot {
            ImageSlot::Header => &mut self.header,
            ImageSlot::Backdrop => &mut self.backdrop,
        }
    }

    fn slot_ref(&self, slot: ImageSlot) -> &Option<Entry> {
        match slot {
            ImageSlot::Header => &self.header,
            ImageSlot::Backdrop => &self.backdrop,
        }
    }

    /// The texture to paint for `slot`, loading it first if the cache holds
    /// nothing for this `(path, dst)` key. `None` means "paint the default
    /// artwork": either the load failed (see [`Self::error`] for what to
    /// tell the user) or this is the frame it is being attempted on.
    ///
    /// Returns a `TextureId` rather than a borrow of the handle so the
    /// caller can drop the `RefCell` guard before painting.
    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        slot: ImageSlot,
        path: &Path,
        dst: [u32; 2],
    ) -> Option<egui::TextureId> {
        let entry = self.slot(slot);
        let hit = entry
            .as_ref()
            .is_some_and(|cached| cached.path == path && cached.dst == dst);
        if !hit {
            let result = load(path, dst).map(|image| {
                ctx.load_texture(slot.texture_name(), image, egui::TextureOptions::LINEAR)
            });
            if let Err(err) = &result {
                log::warn!("{} {} {err}", slot.label(), path.display());
            }
            *entry = Some(Entry {
                path: path.to_path_buf(),
                dst,
                result,
            });
        }
        match &entry.as_ref().expect("just populated").result {
            Ok(texture) => Some(texture.id()),
            Err(_) => None,
        }
    }

    /// Why `slot`'s configured image is not painting, if it isn't. Read by
    /// the settings dropdown, which is the only place a user can see that
    /// a path they set is not doing anything.
    pub fn error(&self, slot: ImageSlot) -> Option<ImageError> {
        match &self.slot_ref(slot).as_ref()?.result {
            Ok(_) => None,
            Err(err) => Some(err.clone()),
        }
    }

    /// Drops `slot`'s cached texture (and any cached failure). Called when
    /// the user picks a new file, clears one, or resets to defaults —
    /// anything that makes the current entry meaningless — and also every
    /// frame a slot is configured with no path, so an image the user just
    /// cleared stops occupying GPU memory rather than lingering until the
    /// process exits.
    pub fn clear(&mut self, slot: ImageSlot) {
        *self.slot(slot) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- cover_crop geometry (issues #121, #253) --------------------------

    #[test]
    fn cover_crop_of_a_matching_aspect_ratio_takes_the_whole_source() {
        assert_eq!(cover_crop([800, 400], [400, 200]), [0, 0, 800, 400]);
        assert_eq!(cover_crop([100, 100], [50, 50]), [0, 0, 100, 100]);
    }

    /// A source proportionally wider than the destination keeps its full
    /// height and loses equal slices off both ends — never a squash, and
    /// never a left-aligned trim.
    #[test]
    fn cover_crop_of_a_wide_source_trims_width_symmetrically() {
        // 1000x100 into a 2:1 region: height binds, so width becomes 200.
        let [x, y, w, h] = cover_crop([1000, 100], [400, 200]);
        assert_eq!([y, w, h], [0, 200, 100]);
        assert_eq!(x, (1000 - 200) / 2);
        assert_eq!(x + w, 1000 - x, "the trim must be equal on both sides");
    }

    /// The mirror case: a portrait photo dropped into the wide header band.
    #[test]
    fn cover_crop_of_a_tall_source_trims_height_symmetrically() {
        // 100x1000 into a 2:1 region: width binds, so height becomes 50.
        let [x, y, w, h] = cover_crop([100, 1000], [400, 200]);
        assert_eq!([x, w, h], [0, 100, 50]);
        assert_eq!(y, (1000 - 50) / 2);
        assert_eq!(y + h, 1000 - y, "the trim must be equal on both sides");
    }

    /// The crop must stay inside the source in every case — `crop_imm`
    /// clamps rather than panicking, but a crop that needed clamping would
    /// mean the geometry was wrong.
    #[test]
    fn cover_crop_never_leaves_the_source_bounds() {
        let sources = [[1, 1], [1, 4000], [4000, 1], [1920, 1080], [37, 991]];
        let destinations = [[1, 1], [400, 200], [200, 400], [1, 1000], [1000, 1]];
        for src in sources {
            for dst in destinations {
                let [x, y, w, h] = cover_crop(src, dst);
                assert!(
                    w >= 1 && h >= 1,
                    "{src:?} -> {dst:?} produced an empty crop"
                );
                assert!(
                    x + w <= src[0] && y + h <= src[1],
                    "{src:?} -> {dst:?} produced {:?}, outside the source",
                    [x, y, w, h]
                );
            }
        }
    }

    /// Cross-multiplying in `u32` would overflow well before this; the
    /// implementation widens to `u64` for exactly this case.
    #[test]
    fn cover_crop_handles_sources_too_large_to_cross_multiply_in_u32() {
        let [_, _, w, h] = cover_crop([100_000, 80_000], [400, 200]);
        assert_eq!([w, h], [100_000, 50_000]);
    }

    // -- target_pixels bucketing ------------------------------------------

    #[test]
    fn target_pixels_scales_by_the_display_factor() {
        // 256x128pt at 2x is 512x256px, both already bucket multiples, so
        // this isolates the scaling from the rounding the next test covers.
        assert_eq!(target_pixels(egui::vec2(256.0, 128.0), 2.0), [512, 256]);
        assert_eq!(target_pixels(egui::vec2(256.0, 128.0), 1.0), [256, 128]);
    }

    #[test]
    fn target_pixels_rounds_up_so_the_texture_never_upscales() {
        let [w, h] = target_pixels(egui::vec2(401.0, 100.0), 1.0);
        assert!(w >= 401, "{w} would have to be upscaled to fill 401px");
        assert_eq!([w, h], [448, 128]);
    }

    /// The whole point of the bucket: a slow resize drag must not re-key
    /// the cache on every frame it produces.
    #[test]
    fn target_pixels_is_stable_across_small_size_changes() {
        let a = target_pixels(egui::vec2(300.0, 200.0), 1.0);
        for delta in [0.5, 1.0, 5.0, 11.0] {
            assert_eq!(
                target_pixels(egui::vec2(300.0 + delta, 200.0), 1.0),
                a,
                "a {delta}pt change re-keyed the cache"
            );
        }
    }

    #[test]
    fn target_pixels_clamps_degenerate_input_instead_of_panicking() {
        assert_eq!(target_pixels(egui::vec2(0.0, 0.0), 1.0), [1, 1]);
        assert_eq!(target_pixels(egui::vec2(-50.0, 100.0), 1.0), [1, 128]);
        assert_eq!(target_pixels(egui::vec2(f32::NAN, 100.0), 1.0), [1, 128]);
        assert_eq!(
            target_pixels(egui::vec2(100.0, 100.0), f32::NAN),
            [128, 128]
        );
        let huge = target_pixels(egui::vec2(f32::MAX, f32::MAX), 4.0);
        assert_eq!(huge, [MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE]);
    }

    // -- decode + graceful failure ----------------------------------------

    /// A one-pixel PNG, base64-free: the smallest valid file that proves
    /// the decoder is wired up and that the output is resized to the
    /// requested destination rather than the source's own size.
    fn one_pixel_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode a 1x1 png");
        bytes
    }

    #[test]
    fn decode_resizes_to_exactly_the_requested_destination() {
        let image = decode_and_fit(&one_pixel_png(), [64, 32]).expect("a valid png must decode");
        assert_eq!(image.size, [64, 32]);
    }

    #[test]
    fn decode_of_a_zero_destination_still_produces_a_paintable_image() {
        let image = decode_and_fit(&one_pixel_png(), [0, 0]).expect("a valid png must decode");
        assert_eq!(image.size, [1, 1], "a 0x0 texture cannot be uploaded");
    }

    #[test]
    fn decode_of_non_image_bytes_reports_undecodable_without_panicking() {
        let err = decode_and_fit(b"this is not an image at all", [64, 32])
            .expect_err("plain text is not an image");
        assert!(matches!(err, ImageError::Undecodable(_)), "{err:?}");
        assert!(err.to_string().contains("not a readable image"), "{err}");
    }

    #[test]
    fn decode_of_empty_bytes_reports_undecodable_without_panicking() {
        assert!(matches!(
            decode_and_fit(&[], [64, 32]),
            Err(ImageError::Undecodable(_))
        ));
    }

    #[test]
    fn load_of_a_missing_path_reports_unreadable_without_panicking() {
        let missing = std::env::temp_dir().join("shinra-custom-image-does-not-exist.png");
        let _ = std::fs::remove_file(&missing);
        let err = load(&missing, [64, 32]).expect_err("a missing file cannot load");
        assert!(matches!(err, ImageError::Unreadable(_)), "{err:?}");
        assert!(err.to_string().contains("could not be read"), "{err}");
    }

    #[test]
    fn load_of_a_real_file_round_trips_through_the_filesystem() {
        let path = std::env::temp_dir().join("shinra-custom-image-round-trip.png");
        std::fs::write(&path, one_pixel_png()).expect("write the fixture");
        let image = load(&path, [128, 64]).expect("a valid png on disk must load");
        assert_eq!(image.size, [128, 64]);
        let _ = std::fs::remove_file(&path);
    }

    /// The extension filter is a convenience over a content-sniffing
    /// decoder, so it must not be empty and must not claim formats the
    /// binary was not compiled to decode.
    #[test]
    fn every_offered_extension_is_lowercase_and_dotless() {
        assert!(!SUPPORTED_EXTENSIONS.is_empty());
        for ext in SUPPORTED_EXTENSIONS {
            assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} is not lowercase");
            assert!(!ext.starts_with('.'), "{ext} must not carry its own dot");
        }
    }

    // -- cache behavior ----------------------------------------------------

    #[test]
    fn slots_have_distinct_texture_names() {
        assert_ne!(
            ImageSlot::Header.texture_name(),
            ImageSlot::Backdrop.texture_name()
        );
        assert_eq!(ImageSlot::ALL.len(), 2);
    }

    /// The cache must remember a *failure* too, so a missing path is not
    /// re-opened ten times a second forever.
    #[test]
    fn a_failed_load_is_cached_and_reported_rather_than_retried() {
        let ctx = egui::Context::default();
        let mut cache = CustomImages::default();
        let missing = std::env::temp_dir().join("shinra-custom-image-cached-failure.png");
        let _ = std::fs::remove_file(&missing);

        assert!(
            cache
                .texture(&ctx, ImageSlot::Header, &missing, [64, 32])
                .is_none()
        );
        assert!(matches!(
            cache.error(ImageSlot::Header),
            Some(ImageError::Unreadable(_))
        ));
        // The other slot is untouched by the first one's failure.
        assert_eq!(cache.error(ImageSlot::Backdrop), None);

        cache.clear(ImageSlot::Header);
        assert_eq!(cache.error(ImageSlot::Header), None);
    }

    #[test]
    fn a_successful_load_is_reused_until_the_key_changes() {
        let ctx = egui::Context::default();
        let mut cache = CustomImages::default();
        let path = std::env::temp_dir().join("shinra-custom-image-cache-key.png");
        std::fs::write(&path, one_pixel_png()).expect("write the fixture");

        let first = cache
            .texture(&ctx, ImageSlot::Backdrop, &path, [64, 32])
            .expect("a valid png must upload");
        assert_eq!(
            cache.texture(&ctx, ImageSlot::Backdrop, &path, [64, 32]),
            Some(first),
            "the same key must not re-decode"
        );
        assert_eq!(cache.error(ImageSlot::Backdrop), None);

        let resized = cache
            .texture(&ctx, ImageSlot::Backdrop, &path, [128, 64])
            .expect("a valid png must upload at the new size");
        assert_ne!(resized, first, "a new destination size must re-decode");

        let _ = std::fs::remove_file(&path);
    }
}
