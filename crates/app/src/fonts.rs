//! System CJK font loading (Windows only).
//!
//! egui's bundled default fonts have no CJK coverage, and vendoring a
//! Noto-CJK-sized font file would balloon the self-contained binary this
//! project ships as (see the CI job asserting a small single-file exe).
//! Instead, at startup we probe the local `C:\Windows\Fonts` install for a
//! CJK-covering system font and register it as a *fallback* — appended
//! after egui's bundled defaults, so Latin text keeps its existing metrics —
//! for both the Proportional and Monospace families.

/// System font files that cover CJK glyphs, in probe order: the first one
/// present on disk wins. Windows ships at least one of these on any
/// non-minimal locale install — Microsoft YaHei (Simplified Chinese),
/// Meiryo (Japanese), Malgun Gothic (Korean).
const CJK_FONT_CANDIDATES: &[&str] = &["msyh.ttc", "meiryo.ttc", "malgun.ttf"];

/// Directory Windows installs its system fonts into.
const WINDOWS_FONTS_DIR: &str = r"C:\Windows\Fonts";

/// Name the loaded font is registered under in egui's font table.
const CJK_FONT_KEY: &str = "cjk_fallback";

/// First candidate for which `exists` reports `true`, in probe order.
/// Pure so the probe order is unit-testable without touching a real
/// filesystem.
fn pick_candidate(exists: impl Fn(&str) -> bool) -> Option<&'static str> {
    CJK_FONT_CANDIDATES
        .iter()
        .copied()
        .find(|name| exists(name))
}

/// Installs a system CJK font as a fallback for both egui font families, if
/// one can be found and read. Never panics: a missing `C:\Windows\Fonts`
/// (e.g. running off-Windows) or an unreadable font file is logged and the
/// app continues on egui's bundled defaults only, same as before this fix.
pub fn install_cjk_fallback(ctx: &eframe::egui::Context) {
    use eframe::egui;

    let Some(candidate) =
        pick_candidate(|name| std::path::Path::new(WINDOWS_FONTS_DIR).join(name).is_file())
    else {
        log::info!(
            "no system CJK font found under {WINDOWS_FONTS_DIR}; CJK names will render as tofu"
        );
        return;
    };

    let path = std::path::Path::new(WINDOWS_FONTS_DIR).join(candidate);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            log::warn!("found {candidate} but couldn't read it: {err}");
            return;
        }
    };

    // Start from the bundled defaults (not `FontDefinitions::empty()`) so
    // Latin/emoji glyphs keep using egui's built-in fonts; ours is appended
    // as a fallback, not a replacement.
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_KEY.to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push(CJK_FONT_KEY.to_owned());
    }

    ctx.set_fonts(fonts);
    log::info!("loaded system CJK font fallback: {candidate}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_earlier_candidate_when_multiple_exist() {
        let present = ["meiryo.ttc", "msyh.ttc"];
        let picked = pick_candidate(|name| present.contains(&name));
        assert_eq!(picked, Some("msyh.ttc"));
    }

    #[test]
    fn falls_back_to_a_later_candidate_when_earlier_ones_are_absent() {
        let present = ["malgun.ttf"];
        let picked = pick_candidate(|name| present.contains(&name));
        assert_eq!(picked, Some("malgun.ttf"));
    }

    #[test]
    fn none_when_no_candidate_exists() {
        let picked = pick_candidate(|_| false);
        assert_eq!(picked, None);
    }
}
