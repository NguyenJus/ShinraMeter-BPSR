//! Per-class row icons (issue #9 slice 1).
//!
//! The nine 128x128 PNGs under `crates/app/assets/classes/` are baked into
//! the executable with `include_bytes!`, same as `fonts.rs`'s reasoning for
//! never loading assets off disk at runtime: this stays a single
//! self-contained exe. Each is decoded once, at startup, and uploaded to
//! egui as a texture; `OverlayApp` holds the resulting `ClassIcons` for the
//! rest of the process's life so a row never re-decodes or re-uploads on
//! every frame.
//!
//! Source: <https://github.com/Blue-Protocol-Source/BPSR-ZDPS> (MIT) — see
//! `THIRD_PARTY_NOTICES.md`.

use bpsr_meter::Class;
use eframe::egui;

/// One embedded PNG per class an icon exists for. `Class::Unknown` has no
/// entry — an unrecognized (or absent) class simply paints no icon, never a
/// fallback glyph (see `ClassIcons::get`).
const CLASS_ICON_BYTES: &[(Class, &[u8])] = &[
    (
        Class::Stormblade,
        include_bytes!("../assets/classes/stormblade.png"),
    ),
    (
        Class::FrostMage,
        include_bytes!("../assets/classes/frost_mage.png"),
    ),
    (
        Class::TwinStriker,
        include_bytes!("../assets/classes/twin_striker.png"),
    ),
    (
        Class::WindKnight,
        include_bytes!("../assets/classes/wind_knight.png"),
    ),
    (
        Class::VerdantOracle,
        include_bytes!("../assets/classes/verdant_oracle.png"),
    ),
    (
        Class::HeavyGuardian,
        include_bytes!("../assets/classes/heavy_guardian.png"),
    ),
    (
        Class::Marksman,
        include_bytes!("../assets/classes/marksman.png"),
    ),
    (
        Class::ShieldKnight,
        include_bytes!("../assets/classes/shield_knight.png"),
    ),
    (
        Class::BeatPerformer,
        include_bytes!("../assets/classes/beat_performer.png"),
    ),
];

/// Decodes one embedded PNG into an egui-ready image. Never panics on a
/// malformed slice — logs and returns `None` instead, belt-and-braces since
/// every caller's bytes are all compile-time constants that are never
/// actually expected to fail to decode. `label` identifies which icon this
/// is in the log line (e.g. "class icon Stormblade", "toolbar icon
/// Settings") so a decode failure points at the icon set that actually
/// failed, rather than always reading "class icon" regardless of which
/// `IconSet` called this.
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
    /// logging via `decode`) any whose PNG fails to decode. `texture_name`
    /// and `log_label` both take the key but are kept as two separate
    /// closures rather than one shared string, since each is worded for a
    /// different audience: `texture_name` becomes egui's texture id
    /// (`ctx.load_texture`'s debug name), `log_label` becomes `decode`'s
    /// human-readable failure-log identifier.
    fn load(
        ctx: &egui::Context,
        entries: &[(K, &[u8])],
        texture_name: impl Fn(K) -> String,
        log_label: impl Fn(K) -> String,
    ) -> Self {
        let textures = entries
            .iter()
            .filter_map(|(key, bytes)| {
                let image = decode(&log_label(*key), bytes)?;
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
    pub fn load(ctx: &egui::Context) -> Self {
        Self(IconSet::load(
            ctx,
            CLASS_ICON_BYTES,
            |class| format!("class-icon-{}", class.name()),
            |class| format!("class icon {}", class.name()),
        ))
    }

    /// The texture for `class`, or `None` if no icon is loaded for it
    /// (`Class::Unknown`, or a class whose PNG failed to decode).
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
    /// Decorative clock painted immediately left of the duration text.
    Clock,
}

/// Every `ToolbarIcon` an embedded PNG exists for, sourced from
/// neowutran/ShinraMeter's `resources/img/` (MIT) — see
/// `THIRD_PARTY_NOTICES.md`. No pin/lock and no minimize icon exists in that
/// repo (issue #41's scope note): minimize is instead drawn procedurally by
/// `ui.rs`'s `minimize_button`, and pin/lock is out of scope entirely (no
/// pinning feature exists yet).
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
    (
        ToolbarIcon::Clock,
        include_bytes!("../assets/icons/clock.png"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class this crate can render an icon for. Kept as an explicit
    /// list (rather than iterating a derive) because `Class` has no
    /// enumerator of its own — this is also the totality check: if a new
    /// `Class` variant is added without pairing it here (or explicitly
    /// excluding it, like `Unknown`), this test's length assertion below
    /// catches the mismatch.
    const ALL_CLASSES: &[Class] = &[
        Class::Stormblade,
        Class::FrostMage,
        Class::TwinStriker,
        Class::WindKnight,
        Class::VerdantOracle,
        Class::HeavyGuardian,
        Class::Marksman,
        Class::ShieldKnight,
        Class::BeatPerformer,
    ];

    #[test]
    fn every_known_class_has_an_embedded_icon() {
        assert_eq!(
            CLASS_ICON_BYTES.len(),
            ALL_CLASSES.len(),
            "CLASS_ICON_BYTES must have exactly one entry per known class"
        );
        for class in ALL_CLASSES {
            assert!(
                CLASS_ICON_BYTES.iter().any(|(c, _)| c == class),
                "{class:?} has no embedded icon"
            );
        }
    }

    #[test]
    fn unknown_class_has_no_embedded_icon() {
        assert!(!CLASS_ICON_BYTES.iter().any(|(c, _)| *c == Class::Unknown));
    }

    #[test]
    fn every_embedded_icon_decodes() {
        for (class, bytes) in CLASS_ICON_BYTES {
            assert!(
                decode(&format!("class icon {class:?}"), bytes).is_some(),
                "{class:?}'s icon failed to decode"
            );
        }
    }

    #[test]
    fn malformed_bytes_decode_to_none_without_panicking() {
        assert!(decode("test", b"not a png").is_none());
        assert!(decode("test", &[]).is_none());
    }

    // -- toolbar icons (issue #41) ----------------------------------------

    const ALL_TOOLBAR_ICONS: &[ToolbarIcon] = &[
        ToolbarIcon::Settings,
        ToolbarIcon::Reset,
        ToolbarIcon::Close,
        ToolbarIcon::Clock,
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
}
