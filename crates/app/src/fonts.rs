//! System font loading (Windows only): a real bold weight (issue #56) and a
//! CJK fallback (issue #14).
//!
//! egui's bundled default fonts have no CJK coverage *and* no bold weight,
//! and vendoring either a Noto-CJK-sized file or a second Latin weight would
//! balloon the self-contained binary this project ships as (see the CI job
//! asserting a small single-file exe). Instead, at startup we probe the local
//! `C:\Windows\Fonts` install for what we need and register whatever is
//! actually there:
//!
//! * a humanist-sans regular/bold pair (Segoe UI first — it ships on every
//!   Windows install and is what the reference render in
//!   `docs/reference/new-shinra-ex.webp` is set in), the regular going to the
//!   *head* of `FontFamily::Proportional` so it wins for Latin while egui's
//!   bundled default still backstops any glyph it lacks, and the bold going
//!   under the named family `FontFamily::Name("bold")`;
//! * a CJK-covering font, appended *after* everything else as a pure
//!   fallback, exactly as before.
//!
//! Nothing here is required: on Linux (where this is developed and
//! CI-tested) no candidate exists, every probe misses, and the app runs on
//! egui's bundled defaults alone. The named `"bold"` family is still
//! registered in that case — pointed at the regular/default chain — because
//! epaint *panics* on a `FontFamily::Name` that is not bound to any font, so
//! it must never be possible for the family to be missing once
//! `install_cjk_fallback` has run. Whether a *real* bold was found is
//! published separately via `has_real_bold`, which is what lets `ui.rs` keep
//! its faux-bold double-paint hack as the degraded path.

use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

/// Directory Windows installs its system fonts into.
const WINDOWS_FONTS_DIR: &str = r"C:\Windows\Fonts";

/// System font files that cover CJK glyphs, in probe order: the first one
/// present on disk wins. Windows ships at least one of these on any
/// non-minimal locale install — Microsoft YaHei (Simplified Chinese),
/// Meiryo (Japanese), Malgun Gothic (Korean).
const CJK_FONT_CANDIDATES: &[&str] = &["msyh.ttc", "meiryo.ttc", "malgun.ttf"];

/// A regular/bold system font file pair (issue #56).
struct UiFontPair {
    regular: &'static str,
    bold: &'static str,
}

/// UI font pairs in probe order — the first pair whose *both* files exist
/// wins, and failing that the first pair whose regular exists (see
/// `pick_ui_font`).
///
/// Segoe UI is first because the reference render is set in it and it is
/// present on every Windows install since Vista. Its semibold
/// (`seguisb.ttf`) is listed as a second Segoe entry rather than a fallback
/// inside the first: it is an acceptable stand-in for the true bold
/// (`segoeuib.ttf`) but visibly lighter, so it must only be reached when the
/// true bold is genuinely absent. Tahoma and Arial are last-resort pairs —
/// neither matches the reference's humanist shapes as well, but both are
/// near-universally present and both do ship a real bold file.
const UI_FONT_PAIRS: &[UiFontPair] = &[
    UiFontPair {
        regular: "segoeui.ttf",
        bold: "segoeuib.ttf",
    },
    UiFontPair {
        regular: "segoeui.ttf",
        bold: "seguisb.ttf",
    },
    UiFontPair {
        regular: "tahoma.ttf",
        bold: "tahomabd.ttf",
    },
    UiFontPair {
        regular: "arial.ttf",
        bold: "arialbd.ttf",
    },
];

/// What the UI-font probe found on disk (issue #56). Three distinct
/// outcomes rather than an `Option<(regular, bold)>`, because "a regular but
/// no bold" is a real, supported state — the app then uses that regular for
/// everything and `ui.rs` fakes the bold weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiFontChoice {
    /// Both weights found: real bold, no faux-bold hack needed.
    Both {
        regular: &'static str,
        bold: &'static str,
    },
    /// Only the regular weight found.
    RegularOnly(&'static str),
    /// Nothing found (the Linux/CI case): egui's bundled defaults carry the
    /// whole UI.
    Neither,
}

impl UiFontChoice {
    fn regular(self) -> Option<&'static str> {
        match self {
            Self::Both { regular, .. } | Self::RegularOnly(regular) => Some(regular),
            Self::Neither => None,
        }
    }

    fn bold(self) -> Option<&'static str> {
        match self {
            Self::Both { bold, .. } => Some(bold),
            Self::RegularOnly(_) | Self::Neither => None,
        }
    }
}

/// Name the bold family is registered under in egui's font table. Also the
/// string `ui.rs`'s `bold()` builds its `FontId` from, via `bold_family()`,
/// so the two can never drift apart.
const BOLD_FAMILY_NAME: &str = "bold";

/// The named font family the bold weight is registered under. Always bound
/// to *something* after `install_cjk_fallback` — see the module doc for why
/// an unbound named family is not merely useless but fatal.
pub fn bold_family() -> egui::FontFamily {
    egui::FontFamily::Name(BOLD_FAMILY_NAME.into())
}

/// Set by `install_cjk_fallback` once per process, read by `ui.rs` on every
/// text paint. An `AtomicBool` rather than a `OnceLock<bool>` so the
/// never-installed case (unit tests build a bare `egui::Context` and never
/// call `install_cjk_fallback`) reads `false` without blocking or
/// initializing anything.
static HAS_REAL_BOLD: AtomicBool = AtomicBool::new(false);

/// Whether a genuine bold font file was found and registered, i.e. whether
/// `bold_family()` resolves to something actually heavier than the regular
/// weight. `false` until `install_cjk_fallback` says otherwise — so on Linux
/// and in unit tests (which never install anything) `ui.rs` keeps painting
/// its faux-bold double pass instead.
pub fn has_real_bold() -> bool {
    HAS_REAL_BOLD.load(Ordering::Relaxed)
}

/// First candidate for which `exists` reports `true`, in probe order.
/// Pure so the probe order is unit-testable without touching a real
/// filesystem.
fn pick_candidate(exists: impl Fn(&str) -> bool) -> Option<&'static str> {
    CJK_FONT_CANDIDATES
        .iter()
        .copied()
        .find(|name| exists(name))
}

/// Picks the UI font pair to install (issue #56), same pure/testable shape
/// as `pick_candidate`: a complete pair beats an earlier pair's lone
/// regular, because a real bold is the whole point of the probe — falling
/// back to Segoe UI regular with a faked bold when Tahoma ships a real one
/// would trade the feature away for a nicer-shaped regular.
fn pick_ui_font(exists: impl Fn(&str) -> bool) -> UiFontChoice {
    if let Some(pair) = UI_FONT_PAIRS
        .iter()
        .find(|pair| exists(pair.regular) && exists(pair.bold))
    {
        return UiFontChoice::Both {
            regular: pair.regular,
            bold: pair.bold,
        };
    }
    match UI_FONT_PAIRS.iter().find(|pair| exists(pair.regular)) {
        Some(pair) => UiFontChoice::RegularOnly(pair.regular),
        None => UiFontChoice::Neither,
    }
}

/// Reads one system font file and registers its bytes under its own file
/// name as the font key, returning that key. `None` — logged, never a panic
/// — if the file turned out to be unreadable between the `is_file` probe and
/// here; every caller treats that as "this weight simply isn't available".
fn register(fonts: &mut egui::FontDefinitions, name: &str) -> Option<String> {
    let path = std::path::Path::new(WINDOWS_FONTS_DIR).join(name);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            log::warn!("found {name} but couldn't read it: {err}");
            return None;
        }
    };
    fonts.font_data.insert(
        name.to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    Some(name.to_owned())
}

/// Installs the system UI font pair (issue #56) and the system CJK fallback
/// (issue #14), building **one** `FontDefinitions` and calling `set_fonts`
/// exactly once — two calls would have the second silently discard the
/// first's registrations.
///
/// Keeps its original name and signature because `main.rs` calls it and the
/// two files are edited independently; it is the single font-install entry
/// point for the whole app, CJK fallback included.
///
/// Never panics: a missing `C:\Windows\Fonts` (e.g. running off-Windows) or
/// an unreadable font file just means fewer registrations and the app
/// continues on egui's bundled defaults, same as before this fix.
pub fn install_cjk_fallback(ctx: &egui::Context) {
    let exists = |name: &str| std::path::Path::new(WINDOWS_FONTS_DIR).join(name).is_file();

    // Start from the bundled defaults (not `FontDefinitions::empty()`) so
    // Latin/emoji glyphs always have a backstop; everything below either
    // prepends to or appends around them, never replaces them.
    let mut fonts = egui::FontDefinitions::default();

    let choice = pick_ui_font(exists);
    let regular_key = choice.regular().and_then(|name| register(&mut fonts, name));
    let bold_key = choice.bold().and_then(|name| register(&mut fonts, name));

    // Head of the proportional family, not the tail: this font is meant to
    // *win* for Latin (the reference render's look), with egui's bundled
    // default demoted to a fallback for anything it doesn't cover.
    if let Some(key) = &regular_key {
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, key.clone());
    }

    // The named bold family, always registered — see the module doc. Its
    // chain is the bold file (when there is one) followed by the whole
    // proportional chain, so a glyph the bold weight lacks still renders,
    // just at regular weight, rather than as tofu.
    let mut bold_chain: Vec<String> = bold_key.iter().cloned().collect();
    bold_chain.extend(
        fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(bold_family(), bold_chain);

    // CJK last, appended after everything above in every family, so it only
    // ever answers for glyphs no Latin font could.
    match pick_candidate(exists).and_then(|name| register(&mut fonts, name)) {
        Some(key) => {
            for family in [
                egui::FontFamily::Proportional,
                egui::FontFamily::Monospace,
                bold_family(),
            ] {
                fonts.families.entry(family).or_default().push(key.clone());
            }
            log::info!("loaded system CJK font fallback: {key}");
        }
        None => log::info!(
            "no system CJK font found under {WINDOWS_FONTS_DIR}; CJK names will render as tofu"
        ),
    }

    HAS_REAL_BOLD.store(bold_key.is_some(), Ordering::Relaxed);
    log::info!(
        "UI font: regular={regular_key:?} bold={bold_key:?} (faux bold: {})",
        !has_real_bold()
    );
    ctx.set_fonts(fonts);
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

    // -- UI font pair probe (issue #56) -----------------------------------

    /// A stock Windows install: Segoe UI with its true bold, which is the
    /// pair the reference render is set in.
    #[test]
    fn picks_segoe_ui_with_its_true_bold_when_both_are_present() {
        let present = ["segoeui.ttf", "segoeuib.ttf", "arial.ttf", "arialbd.ttf"];
        assert_eq!(
            pick_ui_font(|name| present.contains(&name)),
            UiFontChoice::Both {
                regular: "segoeui.ttf",
                bold: "segoeuib.ttf",
            }
        );
    }

    /// Semibold is only reached when the true bold is missing — it is a
    /// visibly lighter stand-in, so it must never win over `segoeuib.ttf`.
    #[test]
    fn falls_back_to_semibold_only_when_the_true_bold_is_absent() {
        let present = ["segoeui.ttf", "seguisb.ttf"];
        assert_eq!(
            pick_ui_font(|name| present.contains(&name)),
            UiFontChoice::Both {
                regular: "segoeui.ttf",
                bold: "seguisb.ttf",
            }
        );
    }

    /// A complete pair beats an earlier pair's lone regular: a real bold is
    /// what the probe exists for.
    #[test]
    fn a_complete_later_pair_beats_an_earlier_pairs_lone_regular() {
        let present = ["segoeui.ttf", "tahoma.ttf", "tahomabd.ttf"];
        assert_eq!(
            pick_ui_font(|name| present.contains(&name)),
            UiFontChoice::Both {
                regular: "tahoma.ttf",
                bold: "tahomabd.ttf",
            }
        );
    }

    /// No bold anywhere: the regular still gets installed and `ui.rs` fakes
    /// the weight.
    #[test]
    fn regular_only_when_no_bold_file_exists() {
        let present = ["segoeui.ttf"];
        assert_eq!(
            pick_ui_font(|name| present.contains(&name)),
            UiFontChoice::RegularOnly("segoeui.ttf")
        );
    }

    /// The Linux/CI case: nothing found at all.
    #[test]
    fn neither_when_no_ui_font_exists() {
        assert_eq!(pick_ui_font(|_| false), UiFontChoice::Neither);
    }

    /// `Neither`/`RegularOnly` must never hand out a bold key — that is the
    /// flag `has_real_bold` (and therefore `ui.rs`'s faux-bold path) is
    /// derived from.
    #[test]
    fn only_a_complete_pair_yields_a_bold_file() {
        assert_eq!(UiFontChoice::Neither.bold(), None);
        assert_eq!(UiFontChoice::RegularOnly("segoeui.ttf").bold(), None);
        assert_eq!(
            UiFontChoice::Both {
                regular: "segoeui.ttf",
                bold: "segoeuib.ttf",
            }
            .bold(),
            Some("segoeuib.ttf")
        );
    }

    /// On any machine that never ran `install_cjk_fallback` — every unit
    /// test, and every Linux run — the app must report no real bold, which
    /// is what keeps `ui.rs` off the (unbound) named family.
    #[test]
    fn no_real_bold_is_reported_before_install_runs() {
        assert!(!has_real_bold());
    }
}
