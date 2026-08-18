//! Per-class row icons (issue #9 slice 1), toolbar/glyph chrome, and equipped
//! Imagine icons (issue #33).
//!
//! The nine 128x128 class PNGs and 81 Imagine PNGs are compiled straight
//! into the executable via `include_bytes!`, the same as every other icon
//! set in this module (issue #123). They previously shipped as loose files
//! under `assets/classes/` and `assets/imagines/` beside the executable,
//! resolved at startup from `SHINRA_ASSETS_DIR`, `<exe dir>/assets`, or
//! `CARGO_MANIFEST_DIR/assets` — a design meant to make a takedown request
//! cost a directory deletion instead of a source change, rebuild, and
//! release. In practice that traded a real, if rare, availability risk for
//! that convenience: loose files can be silently lost to a user's own
//! deletion, an antivirus quarantine, or a partial zip extraction, and the
//! app would degrade to no icons with only a warning log. A takedown by
//! source deletion + rebuild + release is easy enough on its own, so
//! embedding wins.
//!
//! Every icon set here — `assets/icons/`, `assets/icons/glyphs/`,
//! `assets/classes/`, and `assets/imagines/` — is decoded once, at startup,
//! and uploaded to egui as a texture; `OverlayApp` holds the resulting icon
//! sets for the rest of the process's life so a row or glyph never
//! re-decodes or re-uploads on every frame. `shinra.ico` is separate: a
//! Win32 resource linked by `build.rs`, not one of these `IconSet`s.
//!
//! Source: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> (MIT) — see
//! `THIRD_PARTY_NOTICES.md`.
//!
//! `ImagineIcons` follows the same load-once pattern, keyed by the icon
//! basenames generated in `crate::imagines` rather than a closed key enum —
//! see its doc comment.

use crate::imagines;
use bpsr_meter::Class;
use eframe::egui;

/// Generates `class_icon_file` — an exhaustive match with no wildcard arm
/// pairing every `Class` variant to its icon's file name under
/// `assets/classes/`, or to `None` for `Class::Unknown` (which
/// intentionally has no icon) — `LOADABLE_CLASSES`, the list this module's
/// tests iterate for totality checks, and `CLASS_ICON_BYTES`, the
/// `(Class, &[u8])` table `ClassIcons::load` hands straight to
/// `IconSet::load`. All three are generated from the same list of variants
/// given to this macro, so they cannot silently disagree with each other.
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
/// Since the icons are `include_bytes!`-ed (issue #123), a `$path` that
/// names a file missing from `assets/classes/` is a compile error, not a
/// runtime `log::warn!` and a blank icon slot — one guarantee rustc gives
/// for free that the loose-file design (issue #103) could not.
macro_rules! class_icons {
    ($($variant:ident => $path:literal),+ $(,)?) => {
        /// The icon file name for `class` (relative to
        /// `assets/classes/`), or `None` if no icon exists for it
        /// (`Class::Unknown`). See the `class_icons!` invocation below for
        /// why adding a `Class` variant without pairing it here is a
        /// compile error rather than a silently-missing icon. Test-only
        /// (`CLASS_ICON_BYTES` below is what production code loads from) —
        /// `#[cfg(test)]`-gated rather than left for the compiler to warn
        /// about as dead code in a release build.
        #[cfg(test)]
        fn class_icon_file(class: Class) -> Option<&'static str> {
            match class {
                $(Class::$variant => Some($path),)+
                Class::Unknown => None,
            }
        }

        /// Every `Class` variant `class_icon_file` pairs to `Some` file
        /// name, in the order given to `class_icons!`. Used only by this
        /// module's tests for totality/uniqueness checks — see the
        /// `class_icons!` doc comment. Test-only, same reasoning as
        /// `class_icon_file` above.
        #[cfg(test)]
        const LOADABLE_CLASSES: &[Class] = &[$(Class::$variant),+];

        /// Every `Class` variant paired with its icon's compiled-in bytes.
        /// `ClassIcons::load` hands this straight to `IconSet::load`, the
        /// same shape `TOOLBAR_ICON_BYTES` and `GLYPH_ICON_BYTES` use below.
        const CLASS_ICON_BYTES: &[(Class, &[u8])] = &[
            $((Class::$variant, include_bytes!(concat!("../assets/classes/", $path)))),+
        ];
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
/// malformed slice — logs and returns `None` instead. Every caller (class,
/// toolbar, glyph, Imagine) passes `&'static [u8]` from `include_bytes!`,
/// so a decode failure here would mean a corrupt committed PNG, not a
/// runtime read failure — still worth handling without a panic rather than
/// assuming it can't happen. `label` identifies which icon this is in the
/// log line (e.g. "class icon Stormblade", "toolbar icon Settings") so a
/// decode failure points at the icon set that actually failed, rather than
/// always reading "class icon" regardless of which `IconSet` called this.
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
    /// anything `AsRef<[u8]>` rather than a bare `&[u8]` so every caller can
    /// pass its `&'static [u8]` `include_bytes!` slice directly, with no
    /// conversion needed regardless of key type. `texture_name` and
    /// `log_label` both take the key but are kept as two separate closures
    /// rather than one shared string, since each is worded for a different
    /// audience:
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
}

/// Textures for every class an icon was successfully decoded for, uploaded
/// once via `ClassIcons::load`. Loaded lazily on `OverlayApp`'s first
/// `ui()` call rather than in `OverlayApp::new`, because the `egui::Context`
/// `load_texture` needs doesn't exist yet at construction time.
pub struct ClassIcons(IconSet<Class>);

impl ClassIcons {
    /// Decodes and uploads a texture for every `CLASS_ICON_BYTES` entry.
    /// Same load-once pattern as `ToolbarIcons::load`/`GlyphIcons::load`
    /// below — see `ClassIcons`'s doc comment.
    pub fn load(ctx: &egui::Context) -> Self {
        Self(IconSet::load(
            ctx,
            CLASS_ICON_BYTES,
            |class| format!("class-icon-{}", class.name()),
            |class| format!("class icon {}", class.name()),
        ))
    }

    /// The texture for `class`, or `None` if no icon is loaded for it
    /// (`Class::Unknown`, or — never expected in practice, since
    /// `CLASS_ICON_BYTES` are compile-time constants — a PNG that failed to
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
    /// Decodes and uploads a texture for every `imagines::IMAGINE_ICON_BYTES`
    /// entry. Same load-once pattern as `ToolbarIcons::load`/`GlyphIcons::load`
    /// — see `ClassIcons`'s doc comment.
    pub fn load(ctx: &egui::Context) -> Self {
        Self(IconSet::load(
            ctx,
            imagines::IMAGINE_ICON_BYTES,
            |icon| format!("imagine-icon-{icon}"),
            |icon| format!("imagine icon {icon}"),
        ))
    }

    /// The texture for the icon basename `icon`, or `None` if no Imagine
    /// icon is loaded for it — an id `crate::imagines` doesn't know, or —
    /// never expected in practice, since `IMAGINE_ICON_BYTES` are
    /// compile-time constants — a PNG that failed to decode. The caller
    /// paints the blank-circle slot placeholder in that case, never a
    /// panic. Takes `&'static str` rather than a bare `&str`:
    /// `IconSet<&'static str>::get` needs an exact `K` match, and every real
    /// caller already holds a `&'static str` from `imagines::Imagine::icon`.
    pub fn get(&self, icon: &'static str) -> Option<&egui::TextureHandle> {
        self.0.get(icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- class icons (issue #52) ------------------------------------------

    #[test]
    fn unknown_class_has_no_icon_file() {
        assert!(class_icon_file(Class::Unknown).is_none());
    }

    #[test]
    fn every_embedded_class_icon_decodes() {
        for &(class, bytes) in CLASS_ICON_BYTES {
            assert!(
                decode(&format!("class icon {class:?}"), bytes).is_some(),
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
        for &(basename, bytes) in imagines::IMAGINE_ICON_BYTES {
            let image = decode(&format!("imagine icon {basename}"), bytes)
                .unwrap_or_else(|| panic!("{basename}'s icon failed to decode"));
            assert_eq!(
                image.size,
                [48, 48],
                "{basename}'s icon must be rasterized at 48x48"
            );
        }
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
