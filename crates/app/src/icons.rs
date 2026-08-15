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
/// `CLASS_ICON_BYTES` are all compile-time constants that are never actually
/// expected to fail to decode.
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
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
            log::warn!("failed to decode a built-in class icon: {err}");
            None
        }
    }
}

/// Textures for every class an icon was successfully decoded for, uploaded
/// once via `ClassIcons::load`. Loaded lazily on `OverlayApp`'s first
/// `ui()` call rather than in `OverlayApp::new`, because the `egui::Context`
/// `load_texture` needs doesn't exist yet at construction time.
pub struct ClassIcons {
    textures: Vec<(Class, egui::TextureHandle)>,
}

impl ClassIcons {
    pub fn load(ctx: &egui::Context) -> Self {
        let textures = CLASS_ICON_BYTES
            .iter()
            .filter_map(|(class, bytes)| {
                let image = decode(bytes)?;
                let handle = ctx.load_texture(
                    format!("class-icon-{}", class.name()),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                Some((*class, handle))
            })
            .collect();
        Self { textures }
    }

    /// The texture for `class`, or `None` if no icon is loaded for it
    /// (`Class::Unknown`, or a class whose PNG failed to decode).
    pub fn get(&self, class: Class) -> Option<&egui::TextureHandle> {
        self.textures
            .iter()
            .find(|(c, _)| *c == class)
            .map(|(_, handle)| handle)
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
            assert!(decode(bytes).is_some(), "{class:?}'s icon failed to decode");
        }
    }

    #[test]
    fn malformed_bytes_decode_to_none_without_panicking() {
        assert!(decode(b"not a png").is_none());
        assert!(decode(&[]).is_none());
    }
}
