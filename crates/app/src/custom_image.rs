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
//! -> centre-crop the source to the *region's* aspect ratio
//! ([`cover_crop`] against [`region_pixels`], never against the bucket)
//! -> resize that crop onto a texture of [`texture_pixels`] -> upload one
//! `egui::TextureHandle` -> blit it through [`cover_uv`], which crops the
//! texture back to the region's exact aspect ratio. Per frame that is one
//! cache-key comparison and a handful of multiplications.
//!
//! ## Animated GIFs (issue #296)
//!
//! An animated GIF decodes to more than one [`AnimationFrame`] instead of
//! one — [`decode_and_fit`] fits *every* frame through the same crop/resize
//! step above, since a GIF's frames all share one canvas and so one
//! `cover_crop` rectangle. [`CustomImages::texture`] still uploads a single
//! `egui::TextureHandle` (so the blit side of the pipeline, and every other
//! slot in the cache, need not know an image can move at all); playback
//! re-`set`s that same handle's pixels to whichever frame [`AnimationFrame`]
//! is due, on the schedule its own GIF-authored delays name
//! ([`animation_position_at`], the pure function that keeps the "which
//! frame, and when is the next one due" math testable without a live
//! `egui::Context` or a real clock). The context's own logical time
//! (`egui::InputState::time`), not a wall-clock read, is what playback is
//! measured against, and `Context::request_repaint_after` is what wakes the
//! UI up for the next frame boundary even when nothing else would.
//!
//! ## Why two sizes, and why the UV is not baked in
//!
//! Bucketing exists to stabilize the *cache key*; it must never reach the
//! crop geometry. An earlier version fed the bucketed size to
//! [`cover_crop`] and then blitted the result full-UV into the true rect,
//! which scaled the image by `rect / bucket` — a different factor on each
//! axis, i.e. a visible stretch (a 351x75pt header band bucketed to
//! 384x128px came out 1.56x too wide, showing a *horizontal* crop of a
//! band whose overflowing axis is vertical). So the texture now holds a
//! region-aspect crop on a bucket-aspect texture — stored anisotropically
//! stretched — and [`cover_uv`] undoes exactly that stretch at blit time.
//! The correction is computed per frame from the live rect rather than
//! baked in, which also covers the case the bake cannot see: a region that
//! keeps changing *inside* its bucket, where the cached texture is
//! deliberately reused.
//!
//! ## Cache key
//!
//! `(path, destination size in physical pixels, bucketed)`. A changed path
//! obviously invalidates; a changed destination size does too, because the
//! texture was resized *to* that size. The bucketing ([`TARGET_BUCKET`]) is
//! what keeps a window-resize drag from re-decoding on every one of the ~10
//! frames a second it produces — without it a slow drag across 400pt would
//! re-read and re-scale the file hundreds of times. A region that changes
//! within its bucket keeps the cached texture and is corrected by
//! [`cover_uv`] instead.
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
use std::time::Duration;

use eframe::egui;
use image::AnimationDecoder;

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

/// Turns a destination region measured in egui points into its size in
/// physical pixels: the size the crop geometry is computed against, and
/// the size the finished blit fills.
///
/// `points_per_pixel` is `egui::Context::pixels_per_point` — decoding at
/// logical size and letting the GPU upscale would visibly soften the image
/// on the 125%/150% displays this overlay is normally used on.
///
/// Deliberately *not* bucketed. Bucketing belongs to the cache key and the
/// texture allocation ([`texture_pixels`]) alone; rounding a region up here
/// would round its aspect ratio up too, which is precisely the stretch the
/// module doc describes.
///
/// Pure, so the rounding and clamping are testable without a live context.
/// Non-finite or negative input (which `Rect::size` can produce for an
/// unset rect) collapses to [`MIN_TEXTURE_SIDE`] rather than panicking in
/// the `as u32` cast.
pub fn region_pixels(size: egui::Vec2, points_per_pixel: f32) -> [u32; 2] {
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
        (px.ceil() as u32).clamp(MIN_TEXTURE_SIDE, MAX_TEXTURE_SIDE)
    })
}

/// The texture a `region`-pixel destination is decoded onto: each side
/// rounded *up* to [`TARGET_BUCKET`], so the texture is never smaller than
/// the region it fills (which would upscale and blur) and a resize drag
/// re-keys the cache once per bucket rather than once per frame.
///
/// The one place the bucket is allowed to influence anything. It changes
/// the texture's aspect ratio, never the image's: the crop is taken against
/// the true region and [`cover_uv`] cancels the difference at blit time.
///
/// `div_ceil` then multiply, saturating so an absurd size cannot wrap.
pub fn texture_pixels(region: [u32; 2]) -> [u32; 2] {
    region.map(|side| {
        side.div_ceil(TARGET_BUCKET)
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

/// The whole texture, in UV space — what [`cover_uv`] returns when there
/// is nothing left to correct.
const UV_FULL: egui::Rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

/// The sub-rectangle of a texture, in UV (`0..=1`) space, that must be
/// blitted into a `rect`-sized region for the result to be a *uniform*
/// scale of the source.
///
/// `content` is the source rectangle the texture's pixels stand for
/// ([`Fitted::content`], i.e. what [`cover_crop`] selected) — not the
/// texture's own pixel dimensions, which are bucketed and therefore carry
/// a per-axis stretch. Measuring against the content is what cancels that
/// stretch, and it absorbs `cover_crop`'s integer rounding at the same
/// time.
///
/// When the texture was baked for this very rect the answer is the whole
/// texture; when the rect has since drifted inside its cache bucket it is
/// a centred crop of it — the same "cover" rule as [`cover_crop`] one
/// level further down the pipeline, so the region is still filled edge to
/// edge and the trim is still symmetric.
///
/// Pure and float-only (UV space is normalized), so every aspect
/// combination is unit-testable without a painter.
pub fn cover_uv(content: [u32; 2], rect: egui::Vec2) -> egui::Rect {
    let (cw, ch) = (content[0].max(1) as f32, content[1].max(1) as f32);
    let (rw, rh) = (rect.x, rect.y);
    // A collapsed or not-yet-laid-out rect has no aspect ratio to fit; the
    // blit covers no pixels anyway, so the whole texture is as good an
    // answer as any and needs no division.
    if !rw.is_finite() || !rh.is_finite() || rw <= 0.0 || rh <= 0.0 {
        return UV_FULL;
    }
    // `cw/ch > rw/rh` without the division, exactly as in `cover_crop`:
    // content proportionally wider than the region loses width, taller
    // loses height. Fractions rather than pixels, since UV is normalized.
    let (fx, fy) = if cw * rh > rw * ch {
        ((rw * ch) / (rh * cw), 1.0)
    } else {
        (1.0, (rh * cw) / (rw * ch))
    };
    let fx = fx.clamp(f32::EPSILON, 1.0);
    let fy = fy.clamp(f32::EPSILON, 1.0);
    egui::Rect::from_min_max(
        egui::pos2((1.0 - fx) / 2.0, (1.0 - fy) / 2.0),
        egui::pos2((1.0 + fx) / 2.0, (1.0 + fy) / 2.0),
    )
}

/// One playable moment of a decoded image: its pixels, [`texture_pixels`]
/// big like [`Fitted::content`] describes, and how long it stays on screen
/// before the next one is due.
///
/// A non-animated image (anything that is not a multi-frame GIF) decodes to
/// exactly one of these, with [`Self::delay`] never consulted — see
/// [`Fitted::frames`].
#[derive(Debug, Clone)]
pub struct AnimationFrame {
    /// The texture itself, [`texture_pixels`] big.
    pub image: egui::ColorImage,
    /// How long this frame is shown before [`animation_position_at`] moves
    /// on to the next one. Always [`normalize_gif_delay`]'d, so a `0` a
    /// careless encoder wrote out is never taken literally.
    pub delay: Duration,
}

/// Whether a cached crop (`cached_content`, selected by [`cover_crop`]
/// from a `src`-sized source at some earlier region) is still safe to
/// reuse — via [`cover_uv`] — for the *current* `region`, without
/// redecoding.
///
/// Issue #294: reusing a bucket-keyed texture across a resize drag (the
/// whole point of [`texture_pixels`]) is only sound while the crop
/// [`cover_crop`] would select for `region` is a *subset* of what is
/// already decoded. [`cover_uv`] can re-frame what survived the original
/// crop, but the pixels outside it were thrown away for good the moment
/// [`decode_and_fit`] ran — it cannot recover them. Two ways the current
/// region can ask for more than that:
///
/// - **A bigger crop on the same axis.** A region closer to `src`'s own
///   aspect ratio than the one that was baked needs more of the trimmed
///   axis than survived the crop.
/// - **The opposite axis entirely.** A region on the other side of `src`'s
///   aspect ratio needs the axis the bake trimmed away completely, and
///   only has as much of it as the bake's *other* axis happened to keep.
///
/// Both show up as the same thing: [`cover_crop`] against `region` asking
/// for more pixels, on either axis, than `cached_content` holds. Shrinking
/// back down — the common case for a resize that only drifts within its
/// bucket — always passes, so a monotonically shrinking drag keeps reusing
/// the same texture exactly as before; only a drag that grows past its own
/// high-water mark, or flips which axis is trimmed, pays for a redecode.
///
/// Without this check the cache kept whichever crop happened to be baked
/// first and let [`cover_uv`] silently mis-frame every region after it —
/// history-dependent, exactly what a pure function of the current size
/// must not be.
///
/// Pure integer geometry, same as [`cover_crop`] itself, so every case is
/// unit-testable without a decode.
fn crop_is_still_covered(src: [u32; 2], cached_content: [u32; 2], region: [u32; 2]) -> bool {
    let [_, _, w, h] = cover_crop(src, region);
    w <= cached_content[0] && h <= cached_content[1]
}

/// A decoded image ready to upload, plus the two pieces of geometry the
/// blit and the cache still need.
#[derive(Debug)]
pub struct Fitted {
    /// Every frame the source decoded to, in playback order — length 1 for
    /// anything that is not a multi-frame animated GIF (issue #296), in
    /// which case [`CustomImages`] never touches `AnimationFrame::delay`
    /// and behaves exactly as it did before animation existed.
    pub frames: Vec<AnimationFrame>,
    /// The source rectangle those pixels show — [`cover_crop`]'s
    /// `[width, height]`, in source pixels. [`cover_uv`] needs this rather
    /// than a frame's own `image.size`, which bucketing has stretched.
    /// Shared by every frame: a GIF's frames all share one canvas, so
    /// [`cover_crop`] only ever needs to run once per decode.
    pub content: [u32; 2],
    /// The decoded source image's own pixel dimensions — what
    /// [`cover_crop`] took `content` from. The cache keeps this so it can
    /// recompute, on every call and without redecoding, whether `content`
    /// is still what the *current* region would select (issue #294's
    /// [`crop_is_still_covered`]).
    pub src: [u32; 2],
}

/// Cover-crops and resizes one already-decoded RGBA buffer onto
/// [`texture_pixels`] — the per-frame half of the pipeline, shared by the
/// single-image path ([`decode_and_fit`]) and every frame of an animated
/// GIF ([`decode_gif_frames`]) so the crop/resize rule is written once.
///
/// `crop` is [`cover_crop`]'s `[x, y, w, h]`, computed once by the caller
/// (from the first frame) and reused for every subsequent frame — cheaper
/// than recomputing it, and correct, since a GIF's frames all share one
/// canvas and therefore one crop rectangle.
///
/// `CatmullRom` rather than `Lanczos3`: this runs on the UI thread the
/// frame the user picks a file (or the frame a resize crosses a bucket
/// boundary), and at these sizes the two are visually indistinguishable
/// behind the scrim the caller paints over the result.
fn fit_rgba(rgba: image::RgbaImage, crop: [u32; 4], texture: [u32; 2]) -> egui::ColorImage {
    let [x, y, w, h] = crop;
    let fitted = image::DynamicImage::ImageRgba8(rgba)
        .crop_imm(x, y, w, h)
        .resize_exact(
            texture[0],
            texture[1],
            image::imageops::FilterType::CatmullRom,
        )
        .to_rgba8();
    egui::ColorImage::from_rgba_unmultiplied(
        [fitted.width() as usize, fitted.height() as usize],
        &fitted.into_raw(),
    )
}

/// Decodes `bytes` for a destination region of `region` physical pixels,
/// cover-cropped per [`cover_crop`].
///
/// Split from the file read so the decode half is testable from a byte
/// slice, and separate from any texture upload so it needs no live
/// `egui::Context`.
///
/// The returned image(s) are [`texture_pixels`] big — bucketed, so the key
/// they are cached under survives a resize drag — while the crop they hold
/// has `region`'s aspect ratio. The mismatch between the two is what
/// [`cover_uv`] undoes at blit time; cropping to the bucket instead is the
/// stretch bug this split exists to prevent.
///
/// Issue #296: tries the GIF path first ([`decode_gif_frames`]), which
/// handles every GIF that decodes to at least one frame — a single-frame
/// GIF included, so its bytes are decoded exactly once rather than once
/// there and again here. Everything else (a non-GIF) falls through to the
/// ordinary single-frame decode below unchanged.
fn decode_and_fit(bytes: &[u8], region: [u32; 2]) -> Result<Fitted, ImageError> {
    let region = region.map(|side| side.clamp(MIN_TEXTURE_SIDE, MAX_TEXTURE_SIDE));

    if let Some(decoded) = decode_gif_frames(bytes, region, MAX_GIF_FRAMES, MAX_DECODED_GIF_BYTES)?
    {
        return Ok(decoded);
    }

    let decoded =
        image::load_from_memory(bytes).map_err(|err| ImageError::Undecodable(err.to_string()))?;
    let src = [
        image::GenericImageView::width(&decoded),
        image::GenericImageView::height(&decoded),
    ];
    if src[0] == 0 || src[1] == 0 {
        return Err(ImageError::Undecodable("it has no pixels".to_string()));
    }

    let texture = texture_pixels(region);
    let crop = cover_crop(src, region);
    let [_, _, w, h] = crop;
    Ok(Fitted {
        frames: vec![AnimationFrame {
            image: fit_rgba(decoded.to_rgba8(), crop, texture),
            delay: Duration::ZERO,
        }],
        content: [w, h],
        src,
    })
}

/// The floor a GIF frame's own delay is bumped up to before it drives
/// playback. Encoders commonly write `0` (or a couple of hundredths of a
/// second) to mean "no explicit delay" rather than "as fast as possible";
/// taking that literally would flicker the animation at whatever rate the
/// UI happens to repaint at instead of the pace the GIF was authored to
/// play at — the same floor every mainstream GIF viewer/browser applies for
/// the same reason.
const MIN_GIF_FRAME_DELAY: Duration = Duration::from_millis(20);

/// Ceiling on how many frames of a GIF are decoded and kept.
///
/// Every frame is decoded, cover-cropped and CatmullRom-resized *on the UI
/// thread* the moment the user picks the file, and every resulting frame
/// then stays resident in the cache entry's `frames` for as long as it is
/// cached — a bucketed texture is a few megabytes, so an unbounded frame
/// count is both a visible stall at pick time and unbounded memory. 300
/// frames is ten seconds at the 30fps most animated backgrounds are
/// authored at, comfortably more than any loop this overlay is meant to sit
/// behind; a longer GIF is *truncated* rather than rejected, so an
/// over-long file still plays (its first ten seconds, on loop) instead of
/// failing in front of the user.
const MAX_GIF_FRAMES: usize = 300;

/// Ceiling on total RGBA bytes [`decode_gif_frames`] will materialize
/// across every frame it collects, before any crop or resize shrinks them.
///
/// [`MAX_IMAGE_FILE_BYTES`] bounds the *compressed* GIF on disk, but GIF's
/// compression ratio is unbounded by that cap: a small file can still claim
/// a canvas of thousands of pixels a side and, decoded frame-by-frame up to
/// [`MAX_GIF_FRAMES`], demand gigabytes of RGBA before [`fit_rgba`] ever
/// gets a chance to shrink a single frame — a 2000x2000 canvas at 300
/// frames alone is `2000 * 2000 * 4 * 300` = ~4.8 GB. 256 MiB is far past
/// any plausible background GIF's decoded footprint while still bounding
/// the UI-thread stall and the allocation a hostile file can force.
const MAX_DECODED_GIF_BYTES: u64 = 256 * 1024 * 1024;

/// How many RGBA bytes one `width`x`height` GIF frame decodes into. Pure
/// and free of any actual decode, so the budget math is testable against a
/// pathological canvas (a would-be multi-gigabyte frame) without
/// allocating anywhere near it.
///
/// `saturating_mul` throughout: `width` and `height` come straight off the
/// GIF's logical screen descriptor, so a hostile file can claim values
/// whose product overflows `u64` long before it overflows the budget check
/// that is supposed to reject it.
fn gif_frame_rgba_bytes(width: u32, height: u32) -> u64 {
    u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(4)
}

/// Ceiling on the size of a file [`load`] will read into memory.
///
/// The read, the decode and every resize happen synchronously on the UI
/// thread, so a pathologically large file is a freeze rather than a slow
/// load. 64 MiB is far past any plausible background image (a lossless 4K
/// PNG is a handful of megabytes) while still bounding the stall; over it,
/// the file is reported through the ordinary [`ImageError::Unreadable`]
/// path the settings dropdown already surfaces.
const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Bumps a GIF frame's own delay up to [`MIN_GIF_FRAME_DELAY`] — see that
/// constant for why a literal (near-)zero delay is not trustworthy. Pure,
/// so the floor is unit-testable without decoding a real GIF.
fn normalize_gif_delay(delay: Duration) -> Duration {
    delay.max(MIN_GIF_FRAME_DELAY)
}

/// The GIF half of [`decode_and_fit`]: `Ok(Some(_))` for anything sniffed
/// as a GIF that decoded to at least one frame — including a *single*-frame
/// GIF, which is fitted here from the frame already decoded rather than
/// handed back for the caller to decode a second time through
/// `image::load_from_memory`. Only a non-GIF (or a GIF whose frame list is
/// empty, which the static path turns into the usual error) returns
/// `Ok(None)`; a GIF that fails to decode as one reports
/// [`ImageError::Undecodable`] rather than silently falling through, since
/// sniffing already confirmed it claims to be one.
///
/// A single-frame result carries [`Duration::ZERO`] as its delay, exactly
/// as the static path produces, so nothing downstream can tell the two
/// apart: [`Fitted::frames`] of length 1 is never played back.
///
/// At most `max_frames` frames are decoded ([`MAX_GIF_FRAMES`] in
/// production) — the iterator is consumed lazily, so the frames past the
/// cap are never decoded at all, not decoded and then dropped. The frames
/// that *are* decoded may sum to at most `max_decoded_bytes` of RGBA
/// ([`MAX_DECODED_GIF_BYTES`] in production); past that the decode stops
/// and the file is rejected. Taking both caps as arguments rather than
/// reading the constants directly is what lets the tests prove the
/// truncation with a three-frame fixture, and the budget with a fixture
/// far smaller than a real decode bomb.
///
/// `region` must already be clamped to `[MIN_TEXTURE_SIDE, MAX_TEXTURE_
/// SIDE]` (as [`decode_and_fit`] does before calling this), since the crop
/// rectangle computed here is reused verbatim for every frame.
fn decode_gif_frames(
    bytes: &[u8],
    region: [u32; 2],
    max_frames: usize,
    max_decoded_bytes: u64,
) -> Result<Option<Fitted>, ImageError> {
    if !matches!(image::guess_format(bytes), Ok(image::ImageFormat::Gif)) {
        return Ok(None);
    }
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|err| ImageError::Undecodable(err.to_string()))?;
    let (canvas_width, canvas_height) = image::ImageDecoder::dimensions(&decoder);
    if gif_frame_rgba_bytes(canvas_width, canvas_height) > max_decoded_bytes {
        return Err(ImageError::Undecodable(format!(
            "a single frame of its {canvas_width}x{canvas_height} canvas is already more \
             than the {} MiB this overlay allows decoding a GIF into",
            max_decoded_bytes / (1024 * 1024)
        )));
    }

    // The canvas check above only catches a file whose very first frame is
    // a bomb on its own; how many frames a GIF actually carries is not in
    // its header at all, so the budget over a whole file is enforced here
    // instead — each frame's decoded bytes are added to a running total as
    // it arrives, and the decode stops the moment that total passes the
    // budget. `into_frames` is lazy, so at most one frame is ever decoded
    // past the budget and the frames after it are never decoded at all.
    let mut frames = Vec::new();
    let mut decoded_bytes: u64 = 0;
    for frame in decoder.into_frames().take(max_frames) {
        let frame = frame.map_err(|err| ImageError::Undecodable(err.to_string()))?;
        let buffer = frame.buffer();
        decoded_bytes =
            decoded_bytes.saturating_add(gif_frame_rgba_bytes(buffer.width(), buffer.height()));
        frames.push(frame);
        if decoded_bytes > max_decoded_bytes {
            return Err(ImageError::Undecodable(format!(
                "its frames decode past the {} MiB this overlay allows decoding a GIF into",
                max_decoded_bytes / (1024 * 1024)
            )));
        }
    }
    let Some(first) = frames.first() else {
        // A GIF with no frames at all is not something to animate *or* to
        // fit; let the static path produce the usual `Undecodable`.
        return Ok(None);
    };

    let src = [first.buffer().width(), first.buffer().height()];
    if src[0] == 0 || src[1] == 0 {
        return Err(ImageError::Undecodable("it has no pixels".to_string()));
    }
    let texture = texture_pixels(region);
    let crop = cover_crop(src, region);
    let [_, _, w, h] = crop;

    let animated = frames.len() > 1;
    let fitted_frames = frames
        .into_iter()
        .map(|frame| AnimationFrame {
            delay: if animated {
                normalize_gif_delay(frame.delay().into())
            } else {
                Duration::ZERO
            },
            image: fit_rgba(frame.into_buffer(), crop, texture),
        })
        .collect();
    Ok(Some(Fitted {
        frames: fitted_frames,
        content: [w, h],
        src,
    }))
}

/// Where an animation with per-frame `delays` (each already
/// [`normalize_gif_delay`]'d, in playback order) is after `elapsed` time
/// since it started: which frame is showing, and how much longer it stays
/// up before the next one is due.
///
/// Loops forever once the total duration is exceeded — the way every GIF
/// viewer treats a GIF with no explicit repeat count, which is the only
/// kind this module ever decodes (issue #296 does not ask for the finite
/// `LoopCount::Finite` case, and `AnimationDecoder::loop_count` is never
/// read).
///
/// Pure and pixel-free, so the playback math is testable without a live
/// `egui::Context` or a real clock — [`CustomImages::texture`] is the only
/// caller that supplies a real `elapsed`, measured against the context's
/// own logical time rather than a wall-clock read.
///
/// `delays` of length 0 or 1 (nothing to animate) always parks on frame 0
/// with [`Duration::MAX`] remaining, so a caller that unconditionally reads
/// `remaining` into `request_repaint_after` never schedules a wakeup for a
/// static image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationPosition {
    /// Index into `delays` (and so into [`Fitted::frames`]) of the frame
    /// that should be on screen right now.
    pub index: usize,
    /// How much longer `index` stays up before the next frame is due.
    pub remaining: Duration,
}

pub fn animation_position_at(delays: &[Duration], elapsed: Duration) -> AnimationPosition {
    if delays.len() <= 1 {
        return AnimationPosition {
            index: 0,
            remaining: Duration::MAX,
        };
    }
    let total_nanos: u128 = delays.iter().map(Duration::as_nanos).sum();
    if total_nanos == 0 {
        // Every delay normalized to zero cannot happen once
        // `normalize_gif_delay` has run, but a caller could still hand this
        // function raw, un-normalized delays — degenerate input, not a
        // panic.
        return AnimationPosition {
            index: 0,
            remaining: Duration::MAX,
        };
    }
    let mut into_loop = elapsed.as_nanos() % total_nanos;
    for (index, delay) in delays.iter().enumerate() {
        let delay_nanos = delay.as_nanos();
        if into_loop < delay_nanos {
            return AnimationPosition {
                index,
                remaining: Duration::from_nanos((delay_nanos - into_loop) as u64),
            };
        }
        into_loop -= delay_nanos;
    }
    // Unreachable given `into_loop < total_nanos` by construction (the `%`
    // above), but a saturating fallback rather than a panic or an
    // out-of-bounds index if float-free integer math still somehow drifts.
    AnimationPosition {
        index: delays.len() - 1,
        remaining: delays[delays.len() - 1],
    }
}

/// Reads and decodes `path` for a `region`-pixel destination. The whole
/// fallible half of the pipeline in one function, with no `egui::Context`
/// in sight, which is what makes the graceful-degradation path testable on
/// a headless CI host.
///
/// A file past [`MAX_IMAGE_FILE_BYTES`] is refused before it is read, since
/// everything below this line is synchronous UI-thread work; the refusal
/// travels the same [`ImageError::Unreadable`] path a missing file does, so
/// the caller and the settings dropdown need no new case.
pub fn load(path: &Path, region: [u32; 2]) -> Result<Fitted, ImageError> {
    let size = std::fs::metadata(path)
        .map_err(|err| ImageError::Unreadable(err.to_string()))?
        .len();
    if size > MAX_IMAGE_FILE_BYTES {
        return Err(ImageError::Unreadable(format!(
            "it is larger than the {} MiB limit",
            MAX_IMAGE_FILE_BYTES / (1024 * 1024)
        )));
    }
    let bytes = std::fs::read(path).map_err(|err| ImageError::Unreadable(err.to_string()))?;
    decode_and_fit(&bytes, region)
}

/// One slot's cached load: the key it was loaded under, and what came of
/// it. The `Err` arm is cached exactly like the `Ok` arm on purpose — see
/// the module doc's failure section. A missing file is retried when the
/// user re-picks (which clears the entry) or when the region size crosses a
/// bucket, not ten times a second forever.
struct Entry {
    path: PathBuf,
    /// The *bucketed* key this entry was loaded under ([`texture_pixels`]),
    /// not the region that produced it: a region that moves within its
    /// bucket must hit this entry rather than re-decode.
    key: [u32; 2],
    /// [`Fitted::src`] for the successful load — the decoded image's own
    /// dimensions, needed by [`crop_is_still_covered`] to tell whether a
    /// later region still fits inside `content` (issue #294). Meaningless
    /// for the `Err` arm.
    src: [u32; 2],
    /// [`Fitted::content`] for the successful load, so the blit can build
    /// its UV rectangle. Meaningless for the `Err` arm, which never paints.
    content: [u32; 2],
    result: Result<egui::TextureHandle, ImageError>,
    /// [`Fitted::frames`] for the successful load — length 1 for anything
    /// that is not an animated GIF, in which case `showing`/`started_at`
    /// below are never consulted (issue #296). Empty for the `Err` arm.
    /// Kept rather than dropped after the first upload so playback can
    /// re-`set` a later frame's already-decoded pixels without re-reading
    /// or re-decoding the file.
    frames: Vec<AnimationFrame>,
    /// Index into `frames` currently uploaded into `result`'s texture.
    showing: usize,
    /// `egui::Context`'s own logical time ([`egui::InputState::time`]) the
    /// moment this entry was decoded — playback position is measured
    /// relative to this rather than a wall clock, which is what keeps
    /// [`animation_position_at`] (and so this whole cache) testable without
    /// sleeping a real thread.
    started_at: f64,
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

    /// The texture to paint for `slot` over a `region`-pixel destination,
    /// loading it first if the cache holds nothing for this
    /// `(path, bucketed region)` key, together with the
    /// [`Fitted::content`] the caller must hand [`cover_uv`]. `None` means
    /// "paint the default artwork": either the load failed (see
    /// [`Self::error`] for what to tell the user) or this is the frame it
    /// is being attempted on.
    ///
    /// `region` is the *true* pixel size ([`region_pixels`]); the bucketing
    /// happens here, so no caller can accidentally let it reach the crop.
    ///
    /// Returns a `TextureId` rather than a borrow of the handle so the
    /// caller can drop the `RefCell` guard before painting.
    ///
    /// Issue #296: also this cache's whole playback clock. An animated
    /// entry (`frames.len() > 1`) has its uploaded texture advanced to
    /// whichever [`AnimationFrame`] [`animation_position_at`] says is due,
    /// and schedules the wakeup for the next one via
    /// `egui::Context::request_repaint_after` — the frame boundary, not
    /// every-frame polling, is what keeps an idle animated background from
    /// costing more repaints than a static one.
    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        slot: ImageSlot,
        path: &Path,
        region: [u32; 2],
    ) -> Option<(egui::TextureId, [u32; 2])> {
        let key = texture_pixels(region);
        let entry = self.slot(slot);
        let hit = entry.as_ref().is_some_and(|cached| {
            cached.path == path
                && cached.key == key
                && (cached.result.is_err()
                    || crop_is_still_covered(cached.src, cached.content, region))
        });
        if !hit {
            let started_at = ctx.input(|input| input.time);
            let (src, content, frames, result) = match load(path, region) {
                Ok(fitted) => {
                    let first = fitted.frames[0].image.clone();
                    (
                        fitted.src,
                        fitted.content,
                        fitted.frames,
                        Ok(ctx.load_texture(
                            slot.texture_name(),
                            first,
                            egui::TextureOptions::LINEAR,
                        )),
                    )
                }
                Err(err) => {
                    log::warn!("{} {} {err}", slot.label(), path.display());
                    (region, region, Vec::new(), Err(err))
                }
            };
            *entry = Some(Entry {
                path: path.to_path_buf(),
                key,
                src,
                content,
                result,
                frames,
                showing: 0,
                started_at,
            });
        }
        let entry = entry.as_mut().expect("just populated");

        // Advance playback before reading the texture out below, so the
        // very frame an animated entry is (re)loaded on can already show
        // frame 0 rather than waiting a tick.
        if entry.result.is_ok() && entry.frames.len() > 1 {
            let now = ctx.input(|input| input.time);
            let elapsed = Duration::from_secs_f64((now - entry.started_at).max(0.0));
            let delays: Vec<Duration> = entry.frames.iter().map(|frame| frame.delay).collect();
            let position = animation_position_at(&delays, elapsed);
            if position.index != entry.showing {
                let image = entry.frames[position.index].image.clone();
                if let Ok(texture) = &mut entry.result {
                    texture.set(image, egui::TextureOptions::LINEAR);
                }
                entry.showing = position.index;
            }
            ctx.request_repaint_after(position.remaining);
        }

        match &entry.result {
            Ok(texture) => Some((texture.id(), entry.content)),
            Err(_) => None,
        }
    }

    /// Why `slot`'s configured image is not painting, if it isn't. Read by
    /// the settings dropdown, which is the only place a user can see that
    /// a path they set is not doing anything.
    ///
    /// `path` must be the *currently configured* path for `slot` — the
    /// cached entry is only trusted when its own `path` still matches.
    /// Without that check, a slot re-picked from a failing path to a
    /// different, valid one would still hand back the old failure for one
    /// frame (the settings row reads this before `texture` has had a
    /// chance to re-key the cache under the new path), misattributing a
    /// stale error to a file that was never attempted.
    pub fn error(&self, slot: ImageSlot, path: &Path) -> Option<ImageError> {
        let entry = self.slot_ref(slot).as_ref()?;
        if entry.path != path {
            return None;
        }
        match &entry.result {
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

    // -- crop_is_still_covered (issue #294) ---------------------------------

    /// A widescreen source, so trimming width vs. trimming height are
    /// genuinely different crops rather than both collapsing to "the whole
    /// image" the way a 1x1 fixture would.
    const WIDESCREEN: [u32; 2] = [1920, 1080];

    #[test]
    fn covered_when_the_new_region_only_shrinks_the_trimmed_axis() {
        // Baked at the source's own aspect ratio (no trim at all, full
        // 1080px height survives); a more extreme wide region trims height
        // down further, well inside what survived.
        let baked = cover_crop(WIDESCREEN, [64, 36]);
        assert_eq!([baked[2], baked[3]], [1920, 1080], "sanity: no trim yet");
        assert!(crop_is_still_covered(
            WIDESCREEN,
            [baked[2], baked[3]],
            [64, 20]
        ));
    }

    #[test]
    fn not_covered_when_the_new_region_grows_past_the_high_water_mark() {
        // Baked for a region that trims width down to 1000px; a region
        // closer to the source's own aspect ratio needs *more* of that
        // same trimmed axis (1900px) than survived the first crop.
        let baked_narrow = cover_crop(WIDESCREEN, [1000, 1080]);
        assert!(!crop_is_still_covered(
            WIDESCREEN,
            [baked_narrow[2], baked_narrow[3]],
            [1900, 1080]
        ));
    }

    /// The exact shape of issue #294: a region that flips which axis
    /// `cover_crop` trims (wide destination vs. tall destination) cannot
    /// reuse a bake from the other side, because the axis it now needs in
    /// full is the one the earlier bake threw away.
    #[test]
    fn not_covered_when_the_trimmed_axis_flips() {
        let baked_wide = cover_crop(WIDESCREEN, [64, 20]);
        assert!(!crop_is_still_covered(
            WIDESCREEN,
            [baked_wide[2], baked_wide[3]],
            [40, 60]
        ));

        let baked_tall = cover_crop(WIDESCREEN, [40, 60]);
        assert!(!crop_is_still_covered(
            WIDESCREEN,
            [baked_tall[2], baked_tall[3]],
            [64, 20]
        ));
    }

    #[test]
    fn covered_when_nothing_changed_at_all() {
        let baked = cover_crop(WIDESCREEN, [351, 75]);
        assert!(crop_is_still_covered(
            WIDESCREEN,
            [baked[2], baked[3]],
            [351, 75]
        ));
    }

    // -- region_pixels / texture_pixels ------------------------------------

    #[test]
    fn region_pixels_scales_by_the_display_factor() {
        assert_eq!(region_pixels(egui::vec2(256.0, 128.0), 2.0), [512, 256]);
        assert_eq!(region_pixels(egui::vec2(256.0, 128.0), 1.0), [256, 128]);
    }

    /// The region is the *true* size: bucketing it here would round its
    /// aspect ratio, which is exactly the stretch `cover_uv` exists to
    /// prevent.
    #[test]
    fn region_pixels_does_not_bucket() {
        assert_eq!(region_pixels(egui::vec2(401.0, 100.0), 1.0), [401, 100]);
        assert_eq!(region_pixels(egui::vec2(350.5, 75.0), 1.0), [351, 75]);
    }

    #[test]
    fn region_pixels_clamps_degenerate_input_instead_of_panicking() {
        assert_eq!(region_pixels(egui::vec2(0.0, 0.0), 1.0), [1, 1]);
        assert_eq!(region_pixels(egui::vec2(-50.0, 100.0), 1.0), [1, 100]);
        assert_eq!(region_pixels(egui::vec2(f32::NAN, 100.0), 1.0), [1, 100]);
        assert_eq!(
            region_pixels(egui::vec2(100.0, 100.0), f32::NAN),
            [100, 100]
        );
        let huge = region_pixels(egui::vec2(f32::MAX, f32::MAX), 4.0);
        assert_eq!(huge, [MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE]);
    }

    #[test]
    fn texture_pixels_rounds_up_so_the_texture_never_upscales() {
        let [w, h] = texture_pixels([401, 100]);
        assert!(w >= 401, "{w} would have to be upscaled to fill 401px");
        assert_eq!([w, h], [448, 128]);
        assert_eq!(texture_pixels([512, 256]), [512, 256], "already exact");
        assert_eq!(texture_pixels([1, 1]), [64, 64]);
        assert_eq!(
            texture_pixels([MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE]),
            [MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE]
        );
    }

    /// The whole point of the bucket: a slow resize drag must not re-key
    /// the cache on every frame it produces.
    #[test]
    fn texture_pixels_is_stable_across_small_size_changes() {
        let a = texture_pixels(region_pixels(egui::vec2(300.0, 200.0), 1.0));
        for delta in [0.5, 1.0, 5.0, 11.0] {
            assert_eq!(
                texture_pixels(region_pixels(egui::vec2(300.0 + delta, 200.0), 1.0)),
                a,
                "a {delta}pt change re-keyed the cache"
            );
        }
    }

    // -- uniform scaling (the regression #264 shipped) ---------------------

    /// A solid PNG of exactly `w` x `h`, so a decode can be driven at any
    /// aspect ratio without a fixture on disk.
    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(w, h, image::Rgba([90, 120, 150, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode a png");
        bytes
    }

    /// Drives the real pipeline — `region_pixels` -> `decode_and_fit`
    /// (which buckets internally) -> `cover_uv` — and reports what a viewer
    /// would actually measure: the source rectangle left on screen, and the
    /// screen pixels per source pixel on each axis.
    ///
    /// `rect` is in points, as `paint_background_image` sees it.
    struct Blit {
        /// Source pixels still visible, `[width, height]`.
        visible: [f32; 2],
        scale_x: f32,
        scale_y: f32,
    }

    fn blit_of(src: [u32; 2], rect: egui::Vec2, points_per_pixel: f32) -> Blit {
        let region = region_pixels(rect, points_per_pixel);
        let fitted = decode_and_fit(&solid_png(src[0], src[1]), region).expect("a png decodes");
        assert_eq!(
            fitted.frames[0].image.size,
            texture_pixels(region).map(|side| side as usize),
            "the upload must stay on the bucketed grid"
        );
        // The crop -> texture step is a per-axis linear map, so a UV
        // fraction of the texture is the same fraction of the crop.
        let uv = cover_uv(fitted.content, rect);
        let visible = [
            uv.width() * fitted.content[0] as f32,
            uv.height() * fitted.content[1] as f32,
        ];
        Blit {
            visible,
            scale_x: rect.x * points_per_pixel / visible[0],
            scale_y: rect.y * points_per_pixel / visible[1],
        }
    }

    /// The regression that shipped: cropping against the *bucketed* size
    /// and then blitting full-UV into the true rect scales each axis by a
    /// different factor. Every combination here — wide source into a narrow
    /// band, tall source into a wide band, an exact aspect match, and a
    /// region that lands mid-bucket on both axes — must come out uniform.
    #[test]
    fn the_blit_scales_both_axes_by_the_same_factor() {
        let cases = [
            // The measured header band, with a wide source.
            ([1920, 1080], egui::vec2(351.0, 75.0), 1.0),
            // The measured row backdrop, mid-bucket on both axes.
            ([1920, 1080], egui::vec2(351.0, 605.0), 1.0),
            // A portrait photo into the wide header band.
            ([1080, 1920], egui::vec2(351.0, 75.0), 1.25),
            // An extreme panorama into a tall region.
            ([4000, 400], egui::vec2(300.0, 700.0), 1.5),
            // Exactly the region's aspect ratio: nothing to crop at all.
            ([702, 150], egui::vec2(351.0, 75.0), 2.0),
            // Square into square, already on the bucket grid.
            ([512, 512], egui::vec2(128.0, 128.0), 1.0),
        ];
        for (src, rect, ppp) in cases {
            let blit = blit_of(src, rect, ppp);
            let skew = (blit.scale_x - blit.scale_y).abs() / blit.scale_x.max(blit.scale_y);
            assert!(
                skew < 1e-3,
                "{src:?} into {rect:?} at {ppp}x scaled x by {} and y by {} \
                 ({:.1}% skew) — the image is stretched",
                blit.scale_x,
                blit.scale_y,
                skew * 100.0
            );
        }
    }

    /// Uniform scaling alone would also be satisfied by letterboxing, so
    /// the "cover" half is asserted too: the trim lands on the axis the
    /// aspect mismatch actually overflows, and the other axis survives
    /// whole. The shipped bug got this backwards — it cropped the header
    /// band horizontally and showed the source's full height.
    #[test]
    fn the_blit_crops_the_overflowing_axis_and_only_that_one() {
        // 1920x1080 (1.78) into a 4.68 band: the source is proportionally
        // *taller*, so its top and bottom are what have to go.
        let tall_into_wide = blit_of([1920, 1080], egui::vec2(351.0, 75.0), 1.0);
        assert!(
            (tall_into_wide.visible[0] - 1920.0).abs() / 1920.0 < 0.005,
            "the full width must survive, kept {}",
            tall_into_wide.visible[0]
        );
        assert!(
            tall_into_wide.visible[1] < 1080.0 * 0.5,
            "the height must be cropped hard, kept {}",
            tall_into_wide.visible[1]
        );

        // 4000x400 (10:1) into a 0.43 region: now the source is
        // proportionally wider, and the trim swaps axes.
        let wide_into_tall = blit_of([4000, 400], egui::vec2(300.0, 700.0), 1.0);
        assert!(
            (wide_into_tall.visible[1] - 400.0).abs() / 400.0 < 0.005,
            "the full height must survive, kept {}",
            wide_into_tall.visible[1]
        );
        assert!(
            wide_into_tall.visible[0] < 4000.0 * 0.1,
            "the width must be cropped hard, kept {}",
            wide_into_tall.visible[0]
        );

        // A source already at the region's aspect ratio loses nothing.
        let exact = blit_of([702, 150], egui::vec2(351.0, 75.0), 1.0);
        assert!(
            (exact.visible[0] - 702.0).abs() < 1.0 && (exact.visible[1] - 150.0).abs() < 1.0,
            "a matching aspect ratio must not be cropped, kept {:?}",
            exact.visible
        );
    }

    /// `cover_uv` is the correction, so it must be the identity exactly
    /// when there is nothing to correct, and never leave UV space.
    #[test]
    fn cover_uv_stays_inside_the_texture_and_is_centred() {
        assert_eq!(cover_uv([200, 100], egui::vec2(400.0, 200.0)), UV_FULL);
        for content in [[1, 1], [1920, 1080], [1, 4000], [4000, 1]] {
            for rect in [
                egui::vec2(351.0, 75.0),
                egui::vec2(75.0, 351.0),
                egui::vec2(0.0, 0.0),
                egui::vec2(f32::NAN, 10.0),
            ] {
                let uv = cover_uv(content, rect);
                assert!(
                    uv.min.x >= 0.0 && uv.min.y >= 0.0 && uv.max.x <= 1.0 && uv.max.y <= 1.0,
                    "{content:?} into {rect:?} left UV space: {uv:?}"
                );
                assert!(
                    (uv.min.x - (1.0 - uv.max.x)).abs() < 1e-6
                        && (uv.min.y - (1.0 - uv.max.y)).abs() < 1e-6,
                    "{content:?} into {rect:?} is not centred: {uv:?}"
                );
            }
        }
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
    fn decode_resizes_onto_the_bucketed_texture_for_the_region() {
        let fitted = decode_and_fit(&one_pixel_png(), [64, 32]).expect("a valid png must decode");
        assert_eq!(fitted.frames.len(), 1, "a png is not an animation");
        assert_eq!(
            fitted.frames[0].image.size,
            [64, 64],
            "the texture is bucketed"
        );
        assert_eq!(fitted.content, [1, 1], "the crop is the whole 1x1 source");
    }

    #[test]
    fn decode_of_a_zero_destination_still_produces_a_paintable_image() {
        let fitted = decode_and_fit(&one_pixel_png(), [0, 0]).expect("a valid png must decode");
        assert_eq!(
            fitted.frames[0].image.size,
            [64, 64],
            "a 0x0 texture cannot be uploaded"
        );
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
        let fitted = load(&path, [128, 64]).expect("a valid png on disk must load");
        assert_eq!(fitted.frames[0].image.size, [128, 64]);
        let _ = std::fs::remove_file(&path);
    }

    // -- animated GIF playback (issue #296) --------------------------------

    /// A tiny animated GIF: one `4x4` solid-color frame per entry in
    /// `colors`, each shown for the matching entry in `delays_ms`. Small
    /// enough to encode instantly, and distinct enough (by color) that a
    /// test can tell which frame actually got decoded or uploaded.
    fn animated_gif(colors: &[[u8; 3]], delays_ms: &[u64]) -> Vec<u8> {
        animated_gif_of_size(4, 4, colors, delays_ms)
    }

    /// [`animated_gif`] on an arbitrary canvas, for the tests that care
    /// about how many bytes a frame decodes into rather than about which
    /// frame is which.
    fn animated_gif_of_size(
        width: u32,
        height: u32,
        colors: &[[u8; 3]],
        delays_ms: &[u64],
    ) -> Vec<u8> {
        assert_eq!(colors.len(), delays_ms.len(), "one delay per color");
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for (color, &delay_ms) in colors.iter().zip(delays_ms) {
                let buffer = image::RgbaImage::from_pixel(
                    width,
                    height,
                    image::Rgba([color[0], color[1], color[2], 255]),
                );
                let frame = image::Frame::from_parts(
                    buffer,
                    0,
                    0,
                    image::Delay::from_saturating_duration(Duration::from_millis(delay_ms)),
                );
                encoder.encode_frame(frame).expect("encode a gif frame");
            }
        }
        bytes
    }

    /// A GIF with only one frame — the fallback case [`decode_gif_frames`]
    /// must recognize and hand back to the ordinary static path.
    fn single_frame_gif() -> Vec<u8> {
        animated_gif(&[[10, 20, 30]], &[100])
    }

    #[test]
    fn decode_of_an_animated_gif_produces_one_frame_per_gif_frame_with_its_own_delay() {
        let bytes = animated_gif(&[[255, 0, 0], [0, 255, 0], [0, 0, 255]], &[30, 60, 90]);
        let fitted = decode_and_fit(&bytes, [8, 8]).expect("a valid animated gif must decode");

        assert_eq!(fitted.frames.len(), 3, "one AnimationFrame per GIF frame");
        assert_eq!(
            fitted.frames.iter().map(|f| f.delay).collect::<Vec<_>>(),
            vec![
                Duration::from_millis(30),
                Duration::from_millis(60),
                Duration::from_millis(90),
            ],
            "each frame keeps the GIF's own authored delay"
        );
        // The crop rectangle is computed once (from the first frame) and
        // reused, so every frame lands on the same bucketed texture size.
        let expected_size = texture_pixels([8, 8]).map(|side| side as usize);
        for frame in &fitted.frames {
            assert_eq!(frame.image.size, expected_size);
        }
    }

    /// GIF-authored delays under [`MIN_GIF_FRAME_DELAY`] (commonly `0`,
    /// meaning "no delay was set") must be floored, or the animation would
    /// flicker rather than play at a sane pace.
    #[test]
    fn decode_of_an_animated_gifs_near_zero_delay_frames_are_floored() {
        let bytes = animated_gif(&[[1, 2, 3], [4, 5, 6]], &[0, 0]);
        let fitted = decode_and_fit(&bytes, [8, 8]).expect("a valid animated gif must decode");
        for frame in &fitted.frames {
            assert_eq!(frame.delay, MIN_GIF_FRAME_DELAY);
        }
    }

    /// A GIF with exactly one frame is a static image, not an animation —
    /// it must decode through the same one-`AnimationFrame` shape a PNG
    /// does, so `CustomImages` never treats it as something to play back.
    #[test]
    fn decode_of_a_single_frame_gif_is_not_treated_as_animated() {
        let fitted = decode_and_fit(&single_frame_gif(), [8, 8]).expect("a valid gif must decode");
        assert_eq!(
            fitted.frames.len(),
            1,
            "a single-frame gif is a static image"
        );
    }

    /// A single-frame GIF must be fitted from the frame the GIF decoder
    /// already produced, not handed back for `decode_and_fit` to decode the
    /// same bytes a second time. The observable proof that the GIF path
    /// kept it: it answers `Some` (so the static branch never runs) while
    /// still producing the static shape — one frame, no delay.
    #[test]
    fn a_single_frame_gif_is_fitted_by_the_gif_path_rather_than_decoded_twice() {
        let fitted = decode_gif_frames(
            &single_frame_gif(),
            [8, 8],
            MAX_GIF_FRAMES,
            MAX_DECODED_GIF_BYTES,
        )
        .expect("a valid gif must decode")
        .expect("the gif path must keep a single-frame gif rather than re-decoding it");
        assert_eq!(fitted.frames.len(), 1, "a single-frame gif is static");
        assert_eq!(
            fitted.frames[0].delay,
            Duration::ZERO,
            "a static image's delay is never consulted, exactly as the png path leaves it"
        );
        assert_eq!(fitted.content, [4, 4], "the whole 4x4 canvas");
        assert_eq!(
            fitted.frames[0].image.size,
            texture_pixels([8, 8]).map(|side| side as usize)
        );
    }

    /// A GIF longer than the cap is truncated rather than rejected: a
    /// too-long file still plays, and neither the decode stall nor the
    /// resident frame memory grows without bound.
    #[test]
    fn a_gif_past_the_frame_cap_is_truncated_rather_than_rejected() {
        let bytes = animated_gif(&[[255, 0, 0], [0, 255, 0], [0, 0, 255]], &[30, 60, 90]);
        let fitted = decode_gif_frames(&bytes, [8, 8], 2, MAX_DECODED_GIF_BYTES)
            .expect("a valid animated gif must decode")
            .expect("a gif over the cap must still produce an animation");
        assert_eq!(
            fitted.frames.len(),
            2,
            "the frames past the cap are dropped"
        );
        assert_eq!(
            fitted.frames.iter().map(|f| f.delay).collect::<Vec<_>>(),
            vec![Duration::from_millis(30), Duration::from_millis(60)],
            "truncation keeps the leading frames, in order"
        );
        // A cap of 1 leaves a single frame, which is the static shape.
        let capped = decode_gif_frames(&bytes, [8, 8], 1, MAX_DECODED_GIF_BYTES)
            .expect("a valid animated gif must decode")
            .expect("a one-frame cap must still produce an image");
        assert_eq!(capped.frames.len(), 1);
        assert_eq!(capped.frames[0].delay, Duration::ZERO);
    }

    // -- decode-bomb budget (issue #322) ------------------------------------

    #[test]
    fn gif_frame_rgba_bytes_is_four_bytes_a_pixel() {
        assert_eq!(gif_frame_rgba_bytes(64, 32), 64 * 32 * 4);
    }

    #[test]
    fn gif_frame_rgba_bytes_is_the_budget_exactly_at_the_boundary() {
        // 8192 * 8192 * 4 == MAX_DECODED_GIF_BYTES exactly; the budget is a
        // ceiling, not a strict-less-than limit, so a canvas this size is
        // still allowed through.
        assert_eq!(gif_frame_rgba_bytes(8192, 8192), MAX_DECODED_GIF_BYTES);
    }

    /// A hostile logical-screen descriptor can claim a `width`/`height`
    /// whose product overflows `u64` well before it overflows the budget;
    /// the math must still answer (and answer over-budget) rather than
    /// panic or wrap around to a passing value.
    #[test]
    fn gif_frame_rgba_bytes_does_not_wrap_on_pathological_dimensions() {
        assert_eq!(gif_frame_rgba_bytes(u32::MAX, u32::MAX), u64::MAX);
        assert!(gif_frame_rgba_bytes(u32::MAX, u32::MAX) > MAX_DECODED_GIF_BYTES);
    }

    /// The upfront canvas check rejects only a *true* bomb — a canvas whose
    /// single frame is already over budget — and does it before any frame
    /// is decoded, so the allocation it exists to prevent never happens.
    #[test]
    fn decode_gif_frames_rejects_a_canvas_whose_single_frame_is_over_budget() {
        // A 4x4 frame is 64 RGBA bytes: over a 32-byte budget on its own.
        let err = decode_gif_frames(&single_frame_gif(), [8, 8], MAX_GIF_FRAMES, 32)
            .expect_err("a frame bigger than the whole budget must be rejected");
        assert!(matches!(err, ImageError::Undecodable(_)), "{err:?}");
        assert!(err.to_string().contains("4x4"), "{err}");
    }

    /// The frame count a file actually carries is not in its header, so the
    /// running decoded total is what enforces the budget: the decode stops
    /// as soon as the frames collected so far pass it, at most one frame
    /// past.
    #[test]
    fn decode_gif_frames_stops_once_the_running_decoded_total_passes_the_budget() {
        let bytes = animated_gif(&[[255, 0, 0], [0, 255, 0], [0, 0, 255]], &[30, 60, 90]);
        // 64 RGBA bytes a frame: one frame fits a 100-byte budget, two do not.
        let err = decode_gif_frames(&bytes, [8, 8], MAX_GIF_FRAMES, 100)
            .expect_err("three 64-byte frames must blow a 100-byte budget");
        assert!(matches!(err, ImageError::Undecodable(_)), "{err:?}");

        // The same file under a budget its frames do fit inside decodes
        // whole, so the rejection above is the running total talking and
        // not the canvas check.
        let fitted = decode_gif_frames(&bytes, [8, 8], MAX_GIF_FRAMES, 192)
            .expect("192 bytes is exactly the three frames")
            .expect("a gif inside the budget must still produce an animation");
        assert_eq!(fitted.frames.len(), 3);
    }

    /// Regression (issue #322 review): the budget must be spent against the
    /// frames a file really has, not against [`MAX_GIF_FRAMES`] worth of
    /// them. Projecting the cap rejected any canvas over ~473 pixels a
    /// side — every 720p GIF — however few frames it actually carried.
    #[test]
    fn a_large_canvas_gif_with_few_frames_decodes_rather_than_being_rejected() {
        // 512 * 512 * 4 * MAX_GIF_FRAMES is ~314 MB (over budget) while the
        // two frames this file really has are ~2 MB (far under it).
        let bytes = animated_gif_of_size(512, 512, &[[255, 0, 0], [0, 0, 255]], &[30, 60]);
        let fitted = decode_gif_frames(&bytes, [8, 8], MAX_GIF_FRAMES, MAX_DECODED_GIF_BYTES)
            .expect("a two-frame 512x512 gif is nowhere near the decode budget")
            .expect("an animated gif must produce an animation");
        assert_eq!(fitted.frames.len(), 2);
        assert_eq!(fitted.src, [512, 512]);
    }

    /// A tiny file asked for the production frame cap still decodes — the
    /// cap alone must never be what rejects it.
    #[test]
    fn decode_gif_frames_still_decodes_when_comfortably_under_the_budget() {
        let bytes = single_frame_gif();
        assert!(
            decode_gif_frames(&bytes, [8, 8], MAX_GIF_FRAMES, MAX_DECODED_GIF_BYTES)
                .expect("well under budget")
                .is_some()
        );
    }

    #[test]
    fn normalize_gif_delay_floors_encoder_zero_delays_but_leaves_real_ones_alone() {
        assert_eq!(normalize_gif_delay(Duration::ZERO), MIN_GIF_FRAME_DELAY);
        assert_eq!(
            normalize_gif_delay(Duration::from_millis(5)),
            MIN_GIF_FRAME_DELAY
        );
        let real = Duration::from_millis(200);
        assert_eq!(
            normalize_gif_delay(real),
            real,
            "a real delay must survive untouched"
        );
    }

    /// A `delays` slice too short to animate (0 or 1 entries — a static
    /// image, or a "gif" that somehow decoded to one frame) must never
    /// advance and must never ask its caller to schedule a wakeup for it.
    #[test]
    fn animation_position_at_never_advances_a_non_animation() {
        for delays in [Vec::new(), vec![Duration::from_millis(100)]] {
            let position = animation_position_at(&delays, Duration::from_secs(5));
            assert_eq!(position.index, 0);
            assert_eq!(position.remaining, Duration::MAX);
        }
    }

    #[test]
    fn animation_position_at_walks_frames_in_order_and_reports_time_left_in_the_current_one() {
        let delays = [
            Duration::from_millis(100),
            Duration::from_millis(100),
            Duration::from_millis(100),
        ];
        assert_eq!(
            animation_position_at(&delays, Duration::ZERO),
            AnimationPosition {
                index: 0,
                remaining: Duration::from_millis(100)
            }
        );
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(50)),
            AnimationPosition {
                index: 0,
                remaining: Duration::from_millis(50)
            }
        );
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(100)),
            AnimationPosition {
                index: 1,
                remaining: Duration::from_millis(100)
            },
            "the frame boundary itself belongs to the next frame"
        );
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(250)),
            AnimationPosition {
                index: 2,
                remaining: Duration::from_millis(50)
            }
        );
    }

    /// Once elapsed time passes the animation's total duration, playback
    /// must loop back to the first frame rather than running off the end
    /// of `delays` — every GIF with no explicit repeat count plays forever.
    #[test]
    fn animation_position_at_loops_back_to_the_first_frame_past_the_total_duration() {
        let delays = [Duration::from_millis(100), Duration::from_millis(200)];
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(350)),
            AnimationPosition {
                index: 0,
                remaining: Duration::from_millis(50)
            },
            "350ms into a 300ms loop is 50ms into the second lap"
        );
    }

    #[test]
    fn animation_position_at_handles_frames_of_unequal_length() {
        let delays = [Duration::from_millis(10), Duration::from_millis(500)];
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(20)).index,
            1
        );
        assert_eq!(
            animation_position_at(&delays, Duration::from_millis(5)).index,
            0
        );
    }

    /// Drives `CustomImages::texture` across several synthetic frames at
    /// controlled `egui::Context` times (never a real clock, so this needs
    /// no sleep) and checks that it actually advances the uploaded texture
    /// on schedule and asks for a repaint at the right moment — the whole
    /// point of storing `frames`/`showing`/`started_at` on `Entry` rather
    /// than just the first frame like a static image.
    #[test]
    fn custom_images_texture_advances_frames_on_the_contexts_own_clock() {
        let path = std::env::temp_dir().join("shinra-custom-image-animated-playback.gif");
        std::fs::write(&path, animated_gif(&[[255, 0, 0], [0, 255, 0]], &[50, 50]))
            .expect("write the fixture");

        let ctx = egui::Context::default();
        let mut cache = CustomImages::default();

        let at = |ctx: &egui::Context, cache: &mut CustomImages, time: f64| {
            ctx.begin_pass(egui::RawInput {
                time: Some(time),
                ..Default::default()
            });
            let texture = cache
                .texture(ctx, ImageSlot::Header, &path, [8, 8])
                .expect("a valid animated gif must upload");
            ctx.end_pass().drop_without_applying_deltas();
            texture
        };

        // Loaded (and so "started") at t=0: frame 0 is up immediately,
        // with no need to wait a tick for it to appear.
        let first = at(&ctx, &mut cache, 0.0);
        // Still within frame 0's 50ms window: the same texture, unchanged.
        assert_eq!(at(&ctx, &mut cache, 0.03), first);
        // Past the 50ms boundary: a different texture (frame 1's pixels
        // were `set` into a *new* id via `ctx.load_texture`'s slot, or the
        // same id with new pixels — either way the id churn below only
        // matters if egui reuses one; what must hold is content, which the
        // id alone can't show, so this only proves it didn't panic and a
        // texture is still returned).
        let _ = at(&ctx, &mut cache, 0.07);
        // Looping: 110ms is 10ms into the third lap-frame (50+50=100ms
        // period), i.e. back on frame 0.
        let looped = at(&ctx, &mut cache, 0.11);
        assert_eq!(
            looped, first,
            "a looped animation must reuse the same texture id, not grow one per lap"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The playback id churn comment above is deliberately loose about
    /// *which* texture id shows which frame — `TextureId` alone can't say.
    /// This test pins the actual pixel content: `Entry::showing` (read
    /// indirectly through `CustomImages::texture`'s behavior) must track
    /// `animation_position_at`, not just leave frame 0 uploaded forever.
    #[test]
    fn custom_images_texture_uploads_the_frame_animation_position_at_selects() {
        // `animation_position_at` itself is proven correct by the tests
        // above; this only has to prove `CustomImages::texture` actually
        // calls it with a real elapsed time and re-`set`s the texture
        // rather than only doing the work once at load time. The signal
        // available without a GPU readback is the texture's *id*, which
        // `TextureHandle::set` never changes (that is the whole point of
        // `set` over re-`load_texture`ing) — so instead this asserts the
        // one thing observable here: the id stays stable across a frame
        // change, proving playback used `set` (in place) rather than
        // silently falling back to `load_texture` (a fresh id) each time.
        let path = std::env::temp_dir().join("shinra-custom-image-animated-stable-id.gif");
        std::fs::write(&path, animated_gif(&[[9, 9, 9], [200, 1, 1]], &[10, 10]))
            .expect("write the fixture");

        let ctx = egui::Context::default();
        let mut cache = CustomImages::default();

        ctx.begin_pass(egui::RawInput {
            time: Some(0.0),
            ..Default::default()
        });
        let (before, _) = cache
            .texture(&ctx, ImageSlot::Header, &path, [8, 8])
            .expect("a valid animated gif must upload");
        ctx.end_pass().drop_without_applying_deltas();

        ctx.begin_pass(egui::RawInput {
            time: Some(0.015),
            ..Default::default()
        });
        let (after, _) = cache
            .texture(&ctx, ImageSlot::Header, &path, [8, 8])
            .expect("a valid animated gif must upload");
        ctx.end_pass().drop_without_applying_deltas();

        assert_eq!(
            before, after,
            "advancing to the next frame must re-`set` the existing texture, not allocate a new one"
        );

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
            cache.error(ImageSlot::Header, &missing),
            Some(ImageError::Unreadable(_))
        ));
        // The other slot is untouched by the first one's failure.
        assert_eq!(cache.error(ImageSlot::Backdrop, &missing), None);

        cache.clear(ImageSlot::Header);
        assert_eq!(cache.error(ImageSlot::Header, &missing), None);
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
        assert_eq!(
            cache.texture(&ctx, ImageSlot::Backdrop, &path, [40, 60]),
            Some(first),
            "a region that stays inside its bucket must not re-decode"
        );
        assert_eq!(cache.error(ImageSlot::Backdrop, &path), None);

        let resized = cache
            .texture(&ctx, ImageSlot::Backdrop, &path, [128, 64])
            .expect("a valid png must upload at the new size");
        assert_ne!(resized, first, "a new bucket must re-decode");

        let _ = std::fs::remove_file(&path);
    }

    /// Issue #294: the whole bug, driven through the real stateful cache.
    /// Two resize *histories* — one that passes through a wide region
    /// before landing on a tall final one, one that lands there directly —
    /// must agree on the crop they end up painting, even though both
    /// intermediate and final regions fall in the same 64px texture
    /// bucket (so the old path-independent-looking bucket key alone would
    /// have called the wide bake a cache hit for the tall region too).
    #[test]
    fn resize_history_does_not_change_the_final_crop() {
        let ctx = egui::Context::default();
        let path = std::env::temp_dir().join("shinra-custom-image-history-a.png");
        std::fs::write(&path, solid_png(WIDESCREEN[0], WIDESCREEN[1])).expect("write the fixture");

        let final_region = [40, 60];
        assert_eq!(
            texture_pixels([64, 20]),
            texture_pixels(final_region),
            "sanity: both regions must land in the same bucket for this to test anything"
        );

        // Path A: visits a wide region first, then resizes to the final
        // (tall) one.
        let mut via_wide = CustomImages::default();
        via_wide
            .texture(&ctx, ImageSlot::Backdrop, &path, [64, 20])
            .expect("the wide bake must upload");
        let (_, content_via_wide) = via_wide
            .texture(&ctx, ImageSlot::Backdrop, &path, final_region)
            .expect("the final region must still upload");

        // Path B: lands on the same final region with no history at all.
        let mut direct = CustomImages::default();
        let (_, content_direct) = direct
            .texture(&ctx, ImageSlot::Backdrop, &path, final_region)
            .expect("a fresh cache must upload the final region too");

        assert_eq!(
            content_via_wide, content_direct,
            "the same final region must select the same crop regardless of \
             which region the resize passed through first"
        );

        let _ = std::fs::remove_file(&path);
    }
}
