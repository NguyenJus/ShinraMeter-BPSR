//! Per-class row icons (issue #9 slice 1), toolbar/glyph chrome, and equipped
//! Imagine icons (issue #33).
//!
//! The nine 128x128 class PNGs and 81 Imagine PNGs are no longer baked into
//! the executable via `include_bytes!`. Instead, they ship as files under
//! `assets/classes/` and `assets/imagines/` beside the executable and are
//! resolved at startup by `crate::assets`, which checks `SHINRA_ASSETS_DIR`
//! env var, then `<exe dir>/assets`, then `CARGO_MANIFEST_DIR/assets` (for
//! dev and tests), then gives up gracefully. This reverses the previous goal
//! of single-file self-containment: a takedown request now costs a directory
//! deletion instead of a source change, rebuild, and release. Embedding
//! third-party game art in a GPL-3.0 binary carries a combined-work tension
//! the project would rather not carry.
//!
//! What stays embedded and why: `assets/icons/` and `assets/icons/glyphs/`
//! (MIT ShinraMeter / Google Material / Pictogrammers / project-authored —
//! no takedown exposure) and `shinra.ico` (a Win32 resource linked by
//! `build.rs`). Each is decoded once, at startup, and uploaded to egui as
//! a texture; `OverlayApp` holds the resulting icon sets for the rest of the
//! process's life so a row or glyph never re-decodes or re-uploads on every
//! frame.
//!
//! Source: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> (MIT) — see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! `ImagineIcons` follows the same load-once pattern, keyed by the icon
//! basenames generated in `crate::imagines` rather than a closed key enum —
//! see its doc comment.

use crate::assets;
use crate::imagines;
use bpsr_meter::Class;
use eframe::egui;

/// Generates `class_icon_file` — an exhaustive match with no wildcard arm
/// pairing every `Class` variant to its icon's file name under
/// `<assets root>/classes/`, or to `None` for `Class::Unknown` (which
/// intentionally has no icon) — and `LOADABLE_CLASSES`, the list
/// `ClassIcons::load_from` iterates to know which classes to attempt
/// loading a texture for. Both are generated from the same list of
/// variants given to this macro, so they cannot silently disagree with
/// each other.
///
/// Before this (issue #52), the class→bytes pairing was a hand-maintained
/// `const` slice, and its only coverage check compared that slice's length
/// against a second hand-maintained list (`ALL_CLASSES`, formerly in this
/// module's tests). A `Class` variant added to
/// `crates/meter/src/event.rs` without a matching entry in either list
/// compiled, passed that test, and silently rendered no icon — the failure
/// was invisible until someone noticed a blank icon slot in a row. Matching
/// on the real `Class` type instead means rustc's own exhaustiveness
/// checker is the enforcement: a new variant makes `class_icon_file` fail
/// to compile until it is paired here. Mirrors `Class::role()` in
/// `crates/meter/src/event.rs`, which uses the same "no wildcard arm" trick
/// for the same reason.
///
/// What this no longer guarantees (issue #103): that the named file
/// actually exists under `assets/classes/` — the PNGs are read from disk at
/// runtime now, not `include_bytes!`-ed, so a missing file is a runtime
/// `log::warn!` and a blank icon slot, not a compile error. `icons::tests`
/// carries a disk-existence test as the replacement guarantee.
///
/// `LOADABLE_CLASSES` only drives load iteration order — `Class` has no
/// built-in enumerator, so *something* has to list its variants for
/// `ClassIcons::load_from` to walk. It cannot go stale relative to
/// `class_icon_file`'s match arms (they're written once, here, and both are
/// generated from it), so the only representation left to keep in sync with
/// reality is the match itself, which the compiler already does.
macro_rules! class_icons {
    ($($variant:ident => $path:literal),+ $(,)?) => {
        /// The icon file name for `class` (relative to
        /// `<assets root>/classes/`), or `None` if no icon exists for it
        /// (`Class::Unknown`). See the `class_icons!` invocation below for
        /// why adding a `Class` variant without pairing it here is a
        /// compile error rather than a silently-missing icon.
        fn class_icon_file(class: Class) -> Option<&'static str> {
            match class {
                $(Class::$variant => Some($path),)+
                Class::Unknown => None,
            }
        }

        /// Every `Class` variant `class_icon_file` pairs to `Some` file
        /// name, in the order given to `class_icons!`. Used only by
        /// `ClassIcons::load_from` to know which classes to attempt
        /// loading a texture for — see the `class_icons!` doc comment.
        const LOADABLE_CLASSES: &[Class] = &[$(Class::$variant),+];
    };
}

class_icons! {
    Stormblade => "stormblade.png",
    FrostMage => "frost_mage.png",
    TwinStriker => "twin_striker.png",
    WindKnight => "wind_knight.png",
    VerdantOracle => "verdant_oracle.png",
    HeavyGuardian => "heavy_guardian.png",
    Marksman => "marksman.png",
    ShieldKnight => "shield_knight.png",
    BeatPerformer => "beat_performer.png",
}

/// Decodes one PNG into an egui-ready image. Never panics on a
/// malformed slice — logs and returns `None` instead. Toolbar and glyph
/// callers pass `&'static [u8]` from `include_bytes!`, while class and
/// Imagine callers pass `Vec<u8>` read from disk at runtime, which is a real
/// failure path. `label` identifies which icon this is in the log line
/// (e.g. "class icon Stormblade", "toolbar icon Settings") so a decode
/// failure points at the icon set that actually failed, rather than always
/// reading "class icon" regardless of which `IconSet` called this.
fn decode(label: &str, bytes: &[u8]) -> Option<egui::ColorImage> {
    match image::load_from_memory(bytes) {
        Ok(image) => {
            let rgba = image.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            Some(egui::ColorImage::from_rgba_unmultiplied(
                size,
                &rgba.into_raw(),
            ))
        }
        Err(err) => {
            log::warn!("failed to decode {label}: {err}");
            None
        }
    }
}

/// Shared load/get pattern behind both `ClassIcons` and `ToolbarIcons`
/// (issue #41 review: `ToolbarIcons` started as a near-verbatim copy of
/// `ClassIcons`, and issue #49 already plans a third icon set, which would
/// otherwise make it a third copy). `K` is one of the small `Copy` key
/// enums (`Class`, `ToolbarIcon`), so the linear-scan `get` below costs
/// nothing that would justify a `HashMap` for the handful of entries either
/// set actually has.
struct IconSet<K> {
    textures: Vec<(K, egui::TextureHandle)>,
}

impl<K: Copy + PartialEq> IconSet<K> {
    /// Decodes and uploads every `(key, bytes)` entry, skipping (and
    /// logging via `decode`) any whose PNG fails to decode. The payload is
    /// anything `AsRef<[u8]>` rather than a bare `&[u8]`: toolbar/glyph pass
    /// `&'static [u8]` slices from `include_bytes!`, while class/imagine pass
    /// owned `Vec<u8>` read from disk at runtime, and neither should have to
    /// allocate or copy to satisfy the other. `texture_name` and `log_label`
    /// both take the key but are kept as two separate closures rather than
    /// one shared string, since each is worded for a different audience:
    /// `texture_name` becomes egui's texture id (`ctx.load_texture`'s debug
    /// name), `log_label` becomes `decode`'s human-readable failure-log
    /// identifier.
    fn load<B: AsRef<[u8]>>(
        ctx: &egui::Context,
        entries: &[(K, B)],
        texture_name: impl Fn(K) -> String,
        log_label: impl Fn(K) -> String,
    ) -> Self {
        let textures = entries
            .iter()
            .filter_map(|(key, bytes)| {
                let image = decode(&log_label(*key), bytes.as_ref())?;
                let handle =
                    ctx.load_texture(texture_name(*key), image, egui::TextureOptions::LINEAR);
                Some((*key, handle))
            })
            .collect();
        Self { textures }
    }

    fn get(&self, key: K) -> Option<&egui::TextureHandle> {
        self.textures
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, handle)| handle)
    }

    fn len(&self) -> usize {
        self.textures.len()
    }
}

/// Textures for every class an icon was successfully decoded for, uploaded
/// once via `ClassIcons::load`. Loaded lazily on `OverlayApp`'s first
/// `ui()` call rather than in `OverlayApp::new`, because the `egui::Context`
/// `load_texture` needs doesn't exist yet at construction time.
pub struct ClassIcons(IconSet<Class>);

impl ClassIcons {
    /// Resolves the asset root via `assets::root()` and loads from it. The
    /// sole production call site (`ui.rs`'s `Icons::load`) instead calls
    /// `load_from` directly so it can resolve the root once and share it
    /// with `ImagineIcons`; this wrapper exists for callers (tests, and any
    /// future one-off) that don't need that sharing.
    pub fn load(ctx: &egui::Context) -> Self {
        Self::load_from(ctx, assets::root().0.as_deref())
    }

    /// Reads each `LOADABLE_CLASSES` entry's PNG from `<root>/classes/`,
    /// decodes, and uploads a texture for it — skipping (and `log::warn!`ing
    /// the class and the full path for) any file that fails to read.
    /// `root == None` returns an empty set without touching the filesystem,
    /// the same degradation a decode failure already produces.
    pub fn load_from(ctx: &egui::Context, root: Option<&std::path::Path>) -> Self {
        let entries: Vec<(Class, Vec<u8>)> = root
            .map(|root| {
                LOADABLE_CLASSES
                    .iter()
                    .filter_map(|&class| {
                        let file = class_icon_file(class)?;
                        let path = root.join("classes").join(file);
                        match std::fs::read(&path) {
                            Ok(bytes) => Some((class, bytes)),
                            Err(err) => {
                                log::warn!(
                                    "failed to read class icon for {class:?} at {}: {err}",
                                    path.display()
                                );
                                None
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self(IconSet::load(
            ctx,
            &entries,
            |class| format!("class-icon-{}", class.name()),
            |class| format!("class icon {}", class.name()),
        ))
    }

    /// How many `LOADABLE_CLASSES` entries have no loaded texture — a
    /// missing asset root, a missing file, or a decode failure. `ui.rs`'s
    /// startup log reports this.
    pub fn missing(&self) -> usize {
        LOADABLE_CLASSES.len() - self.0.len()
    }

    /// The texture for `class`, or `None` if no icon is loaded for it
    /// (`Class::Unknown`, a missing/unreadable file, or a PNG that failed to
    /// decode).
    pub fn get(&self, class: Class) -> Option<&egui::TextureHandle> {
        self.0.get(class)
    }
}

/// One toolbar control/decoration an icon exists for (issue #41). Distinct
/// from `Class` above: these are UI chrome, not per-player data, and every
/// variant here is expected to have an entry in `TOOLBAR_ICON_BYTES` — unlike
/// `Class::Unknown`, there is no "no icon for this one" case, since the set
/// is small and fixed by `draw_header`'s call sites rather than derived from
/// game data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarIcon {
    /// Settings-menu trigger, replacing the bare `"S"` button (issue #41).
    Settings,
    /// Reset button, replacing the `"Reset"` text button.
    Reset,
    /// Close button, replacing the `"×"` glyph button.
    Close,
}

/// Every `ToolbarIcon` an embedded PNG exists for. `Settings`, `Reset`, and
/// `Close` are sourced from neowutran/ShinraMeter's `resources/img/` (MIT) —
/// see `THIRD_PARTY_NOTICES.md`. No pin/lock and no minimize icon exists in
/// that repo (issue #41's scope note): the header dropdown's "Minimize to
/// tray" item (issue #71) is plain text rather than an icon, and pin/lock is
/// out of scope entirely (no pinning feature exists yet). The death-count
/// glyph
/// (formerly `ToolbarIcon::Skull`) moved to `GlyphIcon::Skull` (issue #59)
/// once the pill row it feeds started painting rasterized glyphs
/// throughout.
const TOOLBAR_ICON_BYTES: &[(ToolbarIcon, &[u8])] = &[
    (
        ToolbarIcon::Settings,
        include_bytes!("../assets/icons/settings.png"),
    ),
    (
        ToolbarIcon::Reset,
        include_bytes!("../assets/icons/reset.png"),
    ),
    (
        ToolbarIcon::Close,
        include_bytes!("../assets/icons/close.png"),
    ),
];

/// Textures for the toolbar icons, uploaded once via `ToolbarIcons::load`.
/// Same lazy-load, load-once-per-process pattern as `ClassIcons` — see its
/// doc comment — and in fact loaded alongside it, from the same
/// `OverlayApp::ui`'s single `get_or_insert_with` call (`ui.rs`'s `Icons`
/// wrapper), so there is exactly one lazy-init site for all icon textures,
/// not two.
pub struct ToolbarIcons(IconSet<ToolbarIcon>);

impl ToolbarIcons {
    pub fn load(ctx: &egui::Context) -> Self {
        Self(IconSet::load(
            ctx,
            TOOLBAR_ICON_BYTES,
            |icon| format!("toolbar-icon-{icon:?}"),
            |icon| format!("toolbar icon {icon:?}"),
        ))
    }

    /// The texture for `icon`, or `None` if its PNG failed to decode (never
    /// expected in practice — `TOOLBAR_ICON_BYTES` are compile-time
    /// constants — but callers fall back to the original glyph rather than
    /// paint nothing; see `ui.rs`'s `icon_button`).
    pub fn get(&self, icon: ToolbarIcon) -> Option<&egui::TextureHandle> {
        self.0.get(icon)
    }
}

/// One vendored SVG glyph (issue #59), rasterized to PNG by
/// `scripts/rasterize-icons.sh`. Separate from `ToolbarIcon` because these
/// are painted through `Painter::image` at caller-derived sizes and tints
/// (see `ui.rs`'s `paint_stat_pill`), not through `toolbar_icon_image`'s
/// fixed size and fixed tint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphIcon {
    Emblem,
    Timer,
    Speed,
    Heart,
    Skull,
    MouseOff,
    CloudOff,
    Check,
    /// The toggle cluster's Share button (issue #82) — MDI's `export`
    /// glyph (an arrow lifting out of a tray), reused for "copy a
    /// screenshot to the clipboard" since neither ShinraMeter's `SVG.xaml`
    /// nor the existing vendored set has a dedicated share/copy icon.
    Share,
}

/// Every `GlyphIcon` an embedded PNG exists for. Provenance is split three
/// ways — see `THIRD_PARTY_NOTICES.md`'s "ShinraMeter encounter emblem",
/// "Google Material Symbols", and "Material Design Icons (Pictogrammers)"
/// sections: `Emblem` is ShinraMeter's own artwork; `Timer`, `Speed`,
/// `Heart`, `CloudOff`, and `Check` are Google Material Symbols; `Skull`,
/// `MouseOff`, and `Share` are Pictogrammers' Material Design Icons.
const GLYPH_ICON_BYTES: &[(GlyphIcon, &[u8])] = &[
    (
        GlyphIcon::Emblem,
        include_bytes!("../assets/icons/glyphs/emblem.png"),
    ),
    (
        GlyphIcon::Timer,
        include_bytes!("../assets/icons/glyphs/timer.png"),
    ),
    (
        GlyphIcon::Speed,
        include_bytes!("../assets/icons/glyphs/speed.png"),
    ),
    (
        GlyphIcon::Heart,
        include_bytes!("../assets/icons/glyphs/heart.png"),
    ),
    (
        GlyphIcon::Skull,
        include_bytes!("../assets/icons/glyphs/skull.png"),
    ),
    (
        GlyphIcon::MouseOff,
        include_bytes!("../assets/icons/glyphs/mouse_off.png"),
    ),
    (
        GlyphIcon::CloudOff,
        include_bytes!("../assets/icons/glyphs/cloud_off.png"),
    ),
    (
        GlyphIcon::Check,
        include_bytes!("../assets/icons/glyphs/check.png"),
    ),
    (
        GlyphIcon::Share,
        include_bytes!("../assets/icons/glyphs/share.png"),
    ),
];

/// Textures for the vendored glyph icons, uploaded once via
/// `GlyphIcons::load`. Same lazy-load, load-once-per-process pattern as
/// `ClassIcons`/`ToolbarIcons` — see `ClassIcons`'s doc comment.
pub struct GlyphIcons(IconSet<GlyphIcon>);

impl GlyphIcons {
    pub fn load(ctx: &egui::Context) -> Self {
        Self(IconSet::load(
            ctx,
            GLYPH_ICON_BYTES,
            |icon| format!("glyph-icon-{icon:?}"),
            |icon| format!("glyph icon {icon:?}"),
        ))
    }

    /// The texture for `icon`, or `None` if its PNG failed to decode (never
    /// expected in practice — `GLYPH_ICON_BYTES` are compile-time constants).
    pub fn get(&self, icon: GlyphIcon) -> Option<&egui::TextureHandle> {
        self.0.get(icon)
    }
}

// IMAGINE-TAKEDOWN: one of five sites — see
// `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
//
/// Textures for equipped-Imagine row icons (issue #33), uploaded once via
/// `ImagineIcons::load`. Same lazy-load, load-once-per-process pattern as
/// `ClassIcons`/`ToolbarIcons`/`GlyphIcons` — see `ClassIcons`'s doc comment
/// — but keyed by the icon *basename* (`&'static str`) rather than a closed
/// key enum: `crate::imagines` maps many skill ids to one of 81 icon
/// basenames, and that set is open-ended game data, not fixed UI chrome.
/// `&'static str` already satisfies `IconSet<K>`'s `K: Copy + PartialEq`
/// bound, so no new key enum is needed.
pub struct ImagineIcons(IconSet<&'static str>);

impl ImagineIcons {
    /// Resolves the asset root via `assets::root()` and loads from it. The
    /// sole production call site (`ui.rs`'s `Icons::load`) instead calls
    /// `load_from` directly so it can resolve the root once and share it
    /// with `ClassIcons`; this wrapper exists for callers (tests, and any
    /// future one-off) that don't need that sharing.
    pub fn load(ctx: &egui::Context) -> Self {
        Self::load_from(ctx, assets::root().0.as_deref())
    }

    /// Reads each `IMAGINE_ICON_FILES` basename's PNG from
    /// `<root>/imagines/`, decodes, and uploads a texture for it —
    /// skipping (and `log::warn!`ing the basename and the full path for)
    /// any file that fails to read. `root == None` returns an empty set
    /// without touching the filesystem, the same degradation a decode
    /// failure already produces.
    pub fn load_from(ctx: &egui::Context, root: Option<&std::path::Path>) -> Self {
        let entries: Vec<(&'static str, Vec<u8>)> = root
            .map(|root| {
                imagines::IMAGINE_ICON_FILES
                    .iter()
                    .filter_map(|&basename| {
                        let path = root.join("imagines").join(format!("{basename}.png"));
                        match std::fs::read(&path) {
                            Ok(bytes) => Some((basename, bytes)),
                            Err(err) => {
                                log::warn!(
                                    "failed to read imagine icon {basename} at {}: {err}",
                                    path.display()
                                );
                                None
                            }
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self(IconSet::load(
            ctx,
            &entries,
            |icon| format!("imagine-icon-{icon}"),
            |icon| format!("imagine icon {icon}"),
        ))
    }

    /// How many `IMAGINE_ICON_FILES` entries have no loaded texture — a
    /// missing asset root, a missing file, or a decode failure. `ui.rs`'s
    /// startup log reports this.
    pub fn missing(&self) -> usize {
        imagines::IMAGINE_ICON_FILES.len() - self.0.len()
    }

    /// The texture for the icon basename `icon`, or `None` if no Imagine
    /// icon is loaded for it (an id `crate::imagines` doesn't know, a
    /// missing/unreadable file, or a PNG that failed to decode) — the
    /// caller paints the blank-circle slot placeholder in that case, never
    /// a panic. Takes `&'static str` rather
    /// than a bare `&str`: `IconSet<&'static str>::get` needs an exact `K`
    /// match, and every real caller already holds a `&'static str` from
    /// `imagines::Imagine::icon`.
    pub fn get(&self, icon: &'static str) -> Option<&egui::TextureHandle> {
        self.0.get(icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- asset root guard (issue #103) --------------------------------------
    //
    // Every disk-backed test below (`every_loadable_classes_icon_decodes`,
    // `every_imagine_icon_decodes_at_48x48`, etc.) is only a meaningful
    // check if `assets::root()` actually resolves under `cargo test` — this
    // test is the guard that makes that assumption explicit and gives it a
    // dedicated failure message, rather than letting it fail incidentally
    // inside some unrelated-looking test.
    #[test]
    fn assets_root_resolves_under_cargo_test() {
        assert!(
            assets::root().0.is_some(),
            "no asset root resolved under cargo test — expected the D1 candidate-3 \
             fallback (assets/ under CARGO_MANIFEST_DIR) to resolve"
        );
    }

    // -- class icons (issue #52) ------------------------------------------

    #[test]
    fn unknown_class_has_no_icon_file() {
        assert!(class_icon_file(Class::Unknown).is_none());
    }

    #[test]
    fn every_loadable_classes_icon_decodes() {
        let root = assets::root()
            .0
            .expect("assets root must resolve under cargo test (see D1 candidate 3)");
        for &class in LOADABLE_CLASSES {
            let file = class_icon_file(class).expect("LOADABLE_CLASSES entries must be Some");
            let bytes = std::fs::read(root.join("classes").join(file))
                .unwrap_or_else(|err| panic!("failed to read {class:?}'s icon: {err}"));
            assert!(
                decode(&format!("class icon {class:?}"), &bytes).is_some(),
                "{class:?}'s icon failed to decode"
            );
        }
    }

    #[test]
    fn no_two_classes_share_the_same_icon_asset() {
        for (i, &a) in LOADABLE_CLASSES.iter().enumerate() {
            for &b in &LOADABLE_CLASSES[i + 1..] {
                assert_ne!(
                    class_icon_file(a),
                    class_icon_file(b),
                    "{a:?} and {b:?} reference the same icon file"
                );
            }
        }
    }

    #[test]
    fn malformed_bytes_decode_to_none_without_panicking() {
        assert!(decode("test", b"not a png").is_none());
        assert!(decode("test", &[]).is_none());
    }

    #[test]
    fn class_icons_get_is_defined_for_every_loadable_class() {
        let ctx = egui::Context::default();
        let icons = ClassIcons::load(&ctx);
        for &class in LOADABLE_CLASSES {
            assert!(
                icons.get(class).is_some(),
                "{class:?} has no loaded texture"
            );
        }
    }

    /// Replacement for `ClassIcons::load_from(&ctx, None)`'s no-filesystem
    /// degradation: a missing asset root produces an empty set (`get`
    /// returns `None` for everything, `missing()` equals the full entry
    /// count) rather than a panic — this is the fallback path a real
    /// `assets::root()` failure (no `SHINRA_ASSETS_DIR`, no exe dir, no
    /// manifest dir) takes in production.
    #[test]
    fn class_icons_load_from_none_root_degrades_without_panicking() {
        let ctx = egui::Context::default();
        let icons = ClassIcons::load_from(&ctx, None);
        assert!(
            icons.get(Class::Stormblade).is_none(),
            "a None root must not produce any loaded class texture"
        );
        assert_eq!(icons.missing(), LOADABLE_CLASSES.len());
    }

    /// Same degradation, but for a root that resolves to `Some` path which
    /// simply doesn't exist on disk — the "root exists in principle, files
    /// don't" case, distinct from `None` in that `load_from` does attempt
    /// (and fail) an `fs::read` per class rather than skipping the
    /// filesystem entirely.
    #[test]
    fn class_icons_load_from_nonexistent_root_degrades_without_panicking() {
        let ctx = egui::Context::default();
        let icons = ClassIcons::load_from(
            &ctx,
            Some(std::path::Path::new("/definitely/does/not/exist")),
        );
        assert!(
            icons.get(Class::Stormblade).is_none(),
            "a nonexistent root must not produce any loaded class texture"
        );
        assert_eq!(icons.missing(), LOADABLE_CLASSES.len());
    }

    // -- toolbar icons (issue #41) ----------------------------------------

    const ALL_TOOLBAR_ICONS: &[ToolbarIcon] = &[
        ToolbarIcon::Settings,
        ToolbarIcon::Reset,
        ToolbarIcon::Close,
    ];

    #[test]
    fn every_toolbar_icon_id_has_an_embedded_entry() {
        assert_eq!(
            TOOLBAR_ICON_BYTES.len(),
            ALL_TOOLBAR_ICONS.len(),
            "TOOLBAR_ICON_BYTES must have exactly one entry per ToolbarIcon variant"
        );
        for icon in ALL_TOOLBAR_ICONS {
            assert!(
                TOOLBAR_ICON_BYTES.iter().any(|(i, _)| i == icon),
                "{icon:?} has no embedded icon"
            );
        }
    }

    #[test]
    fn every_embedded_toolbar_icon_decodes() {
        for (icon, bytes) in TOOLBAR_ICON_BYTES {
            assert!(
                decode(&format!("toolbar icon {icon:?}"), bytes).is_some(),
                "{icon:?}'s icon failed to decode"
            );
        }
    }

    #[test]
    fn toolbar_icons_get_is_defined_for_every_toolbar_icon() {
        let ctx = egui::Context::default();
        let icons = ToolbarIcons::load(&ctx);
        for icon in ALL_TOOLBAR_ICONS {
            assert!(icons.get(*icon).is_some(), "{icon:?} has no loaded texture");
        }
    }

    // -- glyph icons (issue #59) --------------------------------------------

    const ALL_GLYPH_ICONS: &[GlyphIcon] = &[
        GlyphIcon::Emblem,
        GlyphIcon::Timer,
        GlyphIcon::Speed,
        GlyphIcon::Heart,
        GlyphIcon::Skull,
        GlyphIcon::MouseOff,
        GlyphIcon::CloudOff,
        GlyphIcon::Check,
        GlyphIcon::Share,
    ];

    #[test]
    fn every_glyph_icon_id_has_an_embedded_entry() {
        assert_eq!(
            GLYPH_ICON_BYTES.len(),
            ALL_GLYPH_ICONS.len(),
            "GLYPH_ICON_BYTES must have exactly one entry per GlyphIcon variant"
        );
        for icon in ALL_GLYPH_ICONS {
            assert!(
                GLYPH_ICON_BYTES.iter().any(|(i, _)| i == icon),
                "{icon:?} has no embedded icon"
            );
        }
    }

    #[test]
    fn every_embedded_glyph_icon_decodes() {
        for (icon, bytes) in GLYPH_ICON_BYTES {
            assert!(
                decode(&format!("glyph icon {icon:?}"), bytes).is_some(),
                "{icon:?}'s icon failed to decode"
            );
        }
    }

    #[test]
    fn glyph_icons_get_is_defined_for_every_glyph_icon() {
        let ctx = egui::Context::default();
        let icons = GlyphIcons::load(&ctx);
        for icon in ALL_GLYPH_ICONS {
            assert!(icons.get(*icon).is_some(), "{icon:?} has no loaded texture");
        }
    }

    #[test]
    fn the_emblem_is_rastered_larger_than_the_small_glyphs() {
        let emblem_bytes = GLYPH_ICON_BYTES
            .iter()
            .find(|(icon, _)| *icon == GlyphIcon::Emblem)
            .map(|(_, bytes)| *bytes)
            .expect("Emblem must be in GLYPH_ICON_BYTES");
        let timer_bytes = GLYPH_ICON_BYTES
            .iter()
            .find(|(icon, _)| *icon == GlyphIcon::Timer)
            .map(|(_, bytes)| *bytes)
            .expect("Timer must be in GLYPH_ICON_BYTES");
        let emblem = image::load_from_memory(emblem_bytes).expect("emblem.png must decode");
        let timer = image::load_from_memory(timer_bytes).expect("timer.png must decode");
        assert_eq!(emblem.width(), 512);
        assert_eq!(timer.width(), 64);
    }

    // -- imagine icons (issue #33) -----------------------------------------
    //
    // The invariant "every icon `imagines::ALL_ENTRIES` references is
    // present in `IMAGINE_ICON_FILES`" is already enforced by
    // `imagines.rs`'s own generated `#[cfg(test)]` module (`ALL_ENTRIES` is
    // private to that module, not visible here across the sibling-module
    // boundary), so it isn't repeated in this file.

    #[test]
    fn every_imagine_icon_decodes_at_48x48() {
        let root = assets::root()
            .0
            .expect("assets root must resolve under cargo test (see D1 candidate 3)");
        for &basename in imagines::IMAGINE_ICON_FILES {
            let bytes = std::fs::read(root.join("imagines").join(format!("{basename}.png")))
                .unwrap_or_else(|err| panic!("failed to read {basename}'s icon: {err}"));
            let image = decode(&format!("imagine icon {basename}"), &bytes)
                .unwrap_or_else(|| panic!("{basename}'s icon failed to decode"));
            assert_eq!(
                image.size,
                [48, 48],
                "{basename}'s icon must be rasterized at 48x48"
            );
        }
    }

    /// `every_imagine_icon_decodes_at_48x48`'s failure message names only
    /// the basename, not the full path it tried to read — this test is the
    /// explicit existence assertion with the full path surfaced, direct
    /// replacement for `include_bytes!`'s build-time existence check on the
    /// Imagine side (mirrors `every_loadable_classes_icon_decodes` on the
    /// class side).
    #[test]
    fn every_imagine_icon_file_exists_on_disk() {
        let root = assets::root()
            .0
            .expect("assets root must resolve under cargo test (see D1 candidate 3)");
        for &basename in imagines::IMAGINE_ICON_FILES {
            let path = root.join("imagines").join(format!("{basename}.png"));
            assert!(
                path.exists(),
                "imagine icon file missing: {}",
                path.display()
            );
        }
    }

    /// Replacement for `ImagineIcons::load_from(&ctx, None)`'s no-filesystem
    /// degradation — see `class_icons_load_from_none_root_degrades_without_panicking`
    /// for the class-side counterpart and its reasoning.
    #[test]
    fn imagine_icons_load_from_none_root_degrades_without_panicking() {
        let ctx = egui::Context::default();
        let icons = ImagineIcons::load_from(&ctx, None);
        let representative = imagines::IMAGINE_ICON_FILES[0];
        assert!(
            icons.get(representative).is_none(),
            "a None root must not produce any loaded imagine texture"
        );
        assert_eq!(icons.missing(), imagines::IMAGINE_ICON_FILES.len());
    }

    /// Same degradation, but for a root that resolves to `Some` path which
    /// simply doesn't exist on disk — see
    /// `class_icons_load_from_nonexistent_root_degrades_without_panicking`
    /// for the class-side counterpart and its reasoning.
    #[test]
    fn imagine_icons_load_from_nonexistent_root_degrades_without_panicking() {
        let ctx = egui::Context::default();
        let icons = ImagineIcons::load_from(
            &ctx,
            Some(std::path::Path::new("/definitely/does/not/exist")),
        );
        let representative = imagines::IMAGINE_ICON_FILES[0];
        assert!(
            icons.get(representative).is_none(),
            "a nonexistent root must not produce any loaded imagine texture"
        );
        assert_eq!(icons.missing(), imagines::IMAGINE_ICON_FILES.len());
    }

    #[test]
    fn imagine_icon_basenames_are_unique() {
        for (i, &a) in imagines::IMAGINE_ICON_FILES.iter().enumerate() {
            for &b in &imagines::IMAGINE_ICON_FILES[i + 1..] {
                assert_ne!(a, b, "icon basename {a:?} appears more than once");
            }
        }
    }

    #[test]
    fn imagine_icons_get_is_defined_for_every_icon_file() {
        let ctx = egui::Context::default();
        let icons = ImagineIcons::load(&ctx);
        for &basename in imagines::IMAGINE_ICON_FILES {
            assert!(
                icons.get(basename).is_some(),
                "{basename} has no loaded texture"
            );
        }
    }
}
