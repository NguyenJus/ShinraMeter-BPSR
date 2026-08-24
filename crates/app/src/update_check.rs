//! Manual "check for updates" (issue #171): decides whether the running
//! build is behind the latest tagged GitHub release, with no automatic or
//! background checking — the header dropdown's "Check for updates" item
//! (`ui::draw_header_menu`) is the only trigger, and every request is a
//! deliberate, one-shot click.
//!
//! Split the way `platform.rs`'s doc comment describes its own FFI/pure
//! split: everything in this module that *decides* something — parsing a
//! release tag, comparing two versions, picking the release page URL out of
//! the API response — is a pure function of strings already in hand, with
//! no network or Windows dependency, so it is exercised directly by the
//! unit tests below on this (Linux) dev/CI host. The one function that
//! actually reaches the network, `check_for_update`, is a thin wrapper that
//! calls `platform::http_get` (WinHTTP on Windows; an `Err` stub
//! everywhere else — see that function's doc comment) and reduces the
//! response through the pure functions here. `ui::draw_header_menu` never
//! calls `platform::http_get` itself; it only ever calls
//! `check_for_update`, and always from a spawned thread (never the UI
//! thread), delivering the result back over a `crossbeam_channel` the same
//! way `pipeline::spawn` and `settings::spawn_writer` hand results back to
//! their callers.

/// The GitHub `owner/repo` this build's releases are checked against.
/// Confirmed against the real remote with `gh repo view --json
/// nameWithOwner` while implementing issue #171 — kept as a named constant
/// (rather than inlined into `GITHUB_API_PATH`) so a fork or rename only
/// has to change one line.
const GITHUB_REPO: &str = "NguyenJus/ShinraMeter-BPSR";

/// Host `check_for_update` connects to — GitHub's REST API, not
/// `github.com` itself.
const GITHUB_API_HOST: &str = "api.github.com";

/// What a manual "check for updates" click ends up showing, once the
/// request (network and all) has resolved. `ui::draw_header_menu` renders
/// each variant as its own inline line under the button — see that
/// function for the actual text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The running build's version is at or ahead of the latest tagged
    /// release. "At or ahead" (not just "equal") on purpose: a locally
    /// built dev version newer than anything tagged yet is not a pending
    /// update either.
    UpToDate,
    /// A newer release is tagged on GitHub. `tag` is the raw tag string
    /// (e.g. `"v0.3.0"`) and `url` is that release's own GitHub page
    /// (`html_url` from the API response) — what `ui::draw_header_menu`
    /// hands to `ui.hyperlink_to`.
    UpdateAvailable { tag: String, url: String },
}

/// Parses a release tag like `v0.2.0` — or `0.2.0`, with no leading `v`,
/// since that is also how `env!("CARGO_PKG_VERSION")` looks — into
/// `(major, minor, patch)`.
///
/// `None` for anything that isn't exactly three dot-separated unsigned
/// integers behind at most one leading `v`: a pre-release/build suffix
/// (`v0.3.0-rc1`, `v0.3.0+build.4`), a two- or four-segment version, empty
/// segments, or non-numeric junk. Issue #171 asks for "malformed/absent tag
/// handling" explicitly — the caller (`decide`) turns a `None` here into an
/// error line in the dropdown rather than guessing at a comparison that
/// might be wrong.
pub fn parse_version_tag(tag: &str) -> Option<(u32, u32, u32)> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    let mut parts = stripped.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        // A fourth dot-separated segment — not a plain MAJOR.MINOR.PATCH.
        return None;
    }
    Some((major, minor, patch))
}

/// Whether `remote` is a strictly newer version than `current`. Tuple
/// `(u32, u32, u32)` already orders lexicographically by major, then
/// minor, then patch — exactly semver precedence for a plain
/// MAJOR.MINOR.PATCH triple (no pre-release/build metadata, which
/// `parse_version_tag` already refuses) — so this is a thin, named wrapper
/// around `>` rather than new comparison logic, kept as its own function so
/// call sites read as a decision ("is this newer") instead of a bare
/// operator.
///
/// A `remote` that is older *or equal* is deliberately not "newer" — issue
/// #171 asks for that case to read "up to date", not "update available".
pub fn is_newer(current: (u32, u32, u32), remote: (u32, u32, u32)) -> bool {
    remote > current
}

/// The subset of a GitHub `.../releases/latest` response this module reads.
/// `#[serde(deny_unknown_fields)]` is deliberately *not* set — the real
/// response has dozens of other fields (`id`, `author`, `assets`, ...) that
/// this module has no use for, and `serde_json` ignores anything not named
/// here by default.
#[derive(Debug, serde::Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: String,
}

/// Pulls the release tag and its GitHub page URL out of a
/// `.../releases/latest` API response body.
///
/// `Err` for anything that doesn't deserialize as a JSON object with both
/// fields present as strings — a malformed body, an HTML error page (rate
/// limiting and some outages return one instead of JSON), or a future API
/// shape change all land here as a message for the dropdown's error line,
/// rather than panicking.
pub fn parse_release_response(json: &str) -> Result<(String, String), String> {
    serde_json::from_str::<ReleaseResponse>(json)
        .map(|response| (response.tag_name, response.html_url))
        .map_err(|err| format!("couldn't parse the GitHub releases response: {err}"))
}

/// The pure decision at the heart of the update check: given the running
/// build's version and the tag/URL already pulled out of the API response,
/// decide `CheckOutcome`. Free of any I/O — no network, no `platform`
/// dependency — so it and everything it calls (`parse_version_tag`,
/// `is_newer`) are exercised directly by the tests below, without a
/// Windows host or a live GitHub API to talk to. `check_for_update` is the
/// only caller that hands this real data; the tests call it directly.
pub fn decide(
    current_version: &str,
    tag_name: &str,
    html_url: &str,
) -> Result<CheckOutcome, String> {
    let current = parse_version_tag(current_version).ok_or_else(|| {
        format!("the running build's version {current_version:?} isn't a plain MAJOR.MINOR.PATCH")
    })?;
    let remote = parse_version_tag(tag_name).ok_or_else(|| {
        format!("the latest release tag {tag_name:?} isn't a plain vMAJOR.MINOR.PATCH tag")
    })?;
    if is_newer(current, remote) {
        Ok(CheckOutcome::UpdateAvailable {
            tag: tag_name.to_string(),
            url: html_url.to_string(),
        })
    } else {
        Ok(CheckOutcome::UpToDate)
    }
}

/// Runs the whole manual "check for updates" request (issue #171): fetches
/// `https://api.github.com/repos/NguyenJus/ShinraMeter-BPSR/releases/latest`
/// over `platform::http_get` — WinHTTP on Windows, an `Err` stub on every
/// other target (see that function's doc comment) — and reduces the
/// response through `parse_release_response` and `decide`.
///
/// Blocking, and deliberately not `async`: `ui::draw_header_menu`'s "Check
/// for updates" click spawns a plain `std::thread` to call this and sends
/// the `Result` back over a `crossbeam_channel`, the same one-shot-thread
/// pattern `settings::spawn_writer`'s doc comment points at — never calls
/// it from the UI thread's `ui()`/`draw_header_menu` directly, which would
/// stall every frame for however long the request takes.
pub fn check_for_update(current_version: &str) -> Result<CheckOutcome, String> {
    let path = format!("/repos/{GITHUB_REPO}/releases/latest");
    let user_agent = format!("ShinraMeter-BPSR/{current_version}");
    let body = crate::platform::http_get(GITHUB_API_HOST, &path, &user_agent)?;
    let (tag_name, html_url) = parse_release_response(&body)?;
    decide(current_version, &tag_name, &html_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_version_tag --------------------------------------------

    #[test]
    fn parse_version_tag_reads_a_v_prefixed_tag() {
        assert_eq!(parse_version_tag("v0.2.0"), Some((0, 2, 0)));
    }

    #[test]
    fn parse_version_tag_reads_a_bare_version_with_no_v_prefix() {
        // `env!("CARGO_PKG_VERSION")` looks like this, not `v0.2.0`.
        assert_eq!(parse_version_tag("0.2.0"), Some((0, 2, 0)));
    }

    #[test]
    fn parse_version_tag_reads_multi_digit_segments() {
        assert_eq!(parse_version_tag("v12.34.56"), Some((12, 34, 56)));
    }

    #[test]
    fn parse_version_tag_rejects_a_prerelease_suffix() {
        assert_eq!(parse_version_tag("v0.3.0-rc1"), None);
    }

    #[test]
    fn parse_version_tag_rejects_a_missing_segment() {
        assert_eq!(parse_version_tag("v0.2"), None);
    }

    #[test]
    fn parse_version_tag_rejects_an_extra_segment() {
        assert_eq!(parse_version_tag("v0.2.0.1"), None);
    }

    #[test]
    fn parse_version_tag_rejects_non_numeric_junk() {
        assert_eq!(parse_version_tag("not-a-version"), None);
    }

    #[test]
    fn parse_version_tag_rejects_an_empty_string() {
        assert_eq!(parse_version_tag(""), None);
    }

    // -- is_newer -------------------------------------------------------

    #[test]
    fn is_newer_true_for_a_newer_patch() {
        assert!(is_newer((0, 2, 0), (0, 2, 1)));
    }

    #[test]
    fn is_newer_true_for_a_newer_minor_even_with_a_lower_patch() {
        assert!(is_newer((0, 2, 9), (0, 3, 0)));
    }

    #[test]
    fn is_newer_true_for_a_newer_major() {
        assert!(is_newer((0, 2, 0), (1, 0, 0)));
    }

    #[test]
    fn is_newer_false_when_remote_equals_current() {
        assert!(!is_newer((0, 2, 0), (0, 2, 0)));
    }

    #[test]
    fn is_newer_false_when_remote_is_older() {
        assert!(!is_newer((0, 2, 0), (0, 1, 9)));
    }

    // -- parse_release_response -----------------------------------------

    #[test]
    fn parse_release_response_reads_tag_and_html_url() {
        let json = r#"{
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0",
            "id": 12345,
            "name": "v0.3.0"
        }"#;
        assert_eq!(
            parse_release_response(json),
            Ok((
                "v0.3.0".to_string(),
                "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0".to_string()
            ))
        );
    }

    #[test]
    fn parse_release_response_rejects_malformed_json() {
        assert!(parse_release_response("not json").is_err());
    }

    #[test]
    fn parse_release_response_rejects_json_missing_tag_name() {
        let json = r#"{"html_url": "https://example.com"}"#;
        assert!(parse_release_response(json).is_err());
    }

    #[test]
    fn parse_release_response_rejects_an_html_error_page() {
        // Rate limiting / outages sometimes hand back HTML instead of JSON.
        assert!(parse_release_response("<html><body>rate limited</body></html>").is_err());
    }

    // -- decide -----------------------------------------------------------

    #[test]
    fn decide_reports_up_to_date_when_remote_equals_current() {
        assert_eq!(
            decide("0.2.0", "v0.2.0", "https://example.com"),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn decide_reports_up_to_date_when_remote_is_older() {
        assert_eq!(
            decide("0.2.0", "v0.1.9", "https://example.com"),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn decide_reports_update_available_with_the_release_url_when_remote_is_newer() {
        assert_eq!(
            decide("0.2.0", "v0.3.0", "https://example.com/releases/tag/v0.3.0"),
            Ok(CheckOutcome::UpdateAvailable {
                tag: "v0.3.0".to_string(),
                url: "https://example.com/releases/tag/v0.3.0".to_string(),
            })
        );
    }

    #[test]
    fn decide_errors_on_a_malformed_remote_tag_instead_of_guessing() {
        assert!(decide("0.2.0", "not-a-version", "https://example.com").is_err());
    }

    #[test]
    fn decide_errors_on_a_malformed_current_version() {
        assert!(decide("garbage", "v0.2.0", "https://example.com").is_err());
    }

    // -- issue #234 regression: a running build must never be told an
    // identical version is available -------------------------------------
    //
    // The reported symptom was v0.2.4 reporting "update available: v0.2.4"
    // against itself. Investigating this module found the comparison
    // already correct on both counts the issue called out — the `v`
    // prefix (`parse_version_tag` strips it before comparing) and the
    // equal-version case (`is_newer` requires strictly greater) — so these
    // pin the exact reported version numbers as a named regression rather
    // than changing any comparison behavior. The actual bug was tag/version
    // drift: the `v0.2.4` tag was pushed without a matching
    // `crates/app/Cargo.toml` version bump, so the running build's
    // `env!("CARGO_PKG_VERSION")` was still `"0.2.3"` when it fetched a
    // `"v0.2.4"` release tag — a real, older-than-remote case this module
    // correctly reports as `UpdateAvailable`. `.github/workflows/release.yml`
    // now refuses to publish a release when the pushed tag and the crate
    // version disagree, closing off that drift at the source.
    #[test]
    fn decide_reports_up_to_date_for_the_issue_234_matching_versions() {
        assert_eq!(
            decide("0.2.4", "v0.2.4", "https://example.com"),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn decide_reports_update_available_when_the_crate_version_was_not_bumped_for_the_tag() {
        // The actual failure mode behind issue #234: the crate version
        // (what `current_version` is here) lagged the pushed tag, so this
        // is correctly `UpdateAvailable`, not a bug in this comparison.
        assert_eq!(
            decide("0.2.3", "v0.2.4", "https://example.com/releases/tag/v0.2.4"),
            Ok(CheckOutcome::UpdateAvailable {
                tag: "v0.2.4".to_string(),
                url: "https://example.com/releases/tag/v0.2.4".to_string(),
            })
        );
    }
}
