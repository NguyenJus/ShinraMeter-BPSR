//! Manual "check for updates" (issue #171) and, on top of it, the in-place
//! update that click can now perform (issue #250): decides whether the
//! running build is behind the latest tagged GitHub release and, if the
//! user asks for it, downloads that release's executable, swaps it in over
//! the running one and hands `ui.rs` the path to relaunch. There is still
//! no automatic or background checking — the header dropdown's "Check for
//! updates" item (`ui::draw_header_menu`) is the only trigger, and every
//! request is a deliberate, one-shot click.
//!
//! Split the way `platform.rs`'s doc comment describes its own FFI/pure
//! split: everything in this module that *decides* something — parsing a
//! release tag, comparing two versions, picking the release page URL and
//! the downloadable asset out of the API response, splitting that asset's
//! URL into the host/path WinHTTP wants, working out where the download is
//! staged and where the outgoing executable is moved aside to — is a pure
//! function of strings and paths already in hand, with no network and no
//! Windows dependency, so it is exercised directly by the unit tests below
//! on this (Linux) dev/CI host. Even the file swap itself
//! (`swap_in_staged_executable`) is plain `std::fs::rename` against paths
//! the caller supplies, so the tests drive the real thing in a scratch
//! directory rather than a mock.
//!
//! The two functions that actually reach the network, `check_for_update`
//! and `install_update`, are thin wrappers that call
//! `platform::http_get`/`platform::http_get_bytes` (WinHTTP on Windows; an
//! `Err` stub everywhere else — see those functions' doc comments) and
//! reduce the response through the pure functions here.
//! `ui::draw_header_menu` never calls `platform::http_get*` itself; it only
//! ever calls `check_for_update`/`install_update`, and always from a
//! spawned thread (never the UI thread), delivering the result back over a
//! `crossbeam_channel` the same way `pipeline::spawn` and
//! `settings::spawn_writer` hand results back to their callers.
//!
//! ## Why the swap works the way it does (issue #250)
//!
//! Windows will not let a running process's image file be overwritten or
//! deleted, but it *will* let it be renamed — the file is locked against
//! writes and deletes, not against a directory-entry move. So the update
//! is: download to `<exe>.new`, rename the running `<exe>` to `<exe>.old`,
//! rename `<exe>.new` onto `<exe>`, relaunch, exit, and delete the leftover
//! `<exe>.old` on the *next* launch (`clean_up_previous_update`), by which
//! point nothing has it open any more.
//!
//! The alternative — writing a `.bat` that waits for this process to exit,
//! copies the new file over the old one and restarts it — was rejected: it
//! flashes a console window, leaves a script behind if the app is killed
//! mid-update, and puts the critical step in a process this code cannot
//! test at all. The rename dance is three `std::fs::rename` calls, every
//! one of which the tests below drive for real. Its cost is the leftover
//! `<exe>.old` sitting beside the executable until the next launch, which
//! is a stray file rather than a broken install.
//!
//! Both staged paths are *siblings* of the executable, not `%TEMP%` entries,
//! for a reason that matters: `std::fs::rename` cannot move across volumes,
//! and `%TEMP%` is routinely on a different one from a meter dropped in a
//! folder on a second drive. Downloading beside the target keeps every step
//! a same-directory rename.

use std::path::{Path, PathBuf};

/// The GitHub `owner/repo` this build's releases are checked against.
/// Confirmed against the real remote with `gh repo view --json
/// nameWithOwner` while implementing issue #171 — kept as a named constant
/// (rather than inlined into `GITHUB_API_PATH`) so a fork or rename only
/// has to change one line.
const GITHUB_REPO: &str = "NguyenJus/ShinraMeter-BPSR";

/// Host `check_for_update` connects to — GitHub's REST API, not
/// `github.com` itself.
const GITHUB_API_HOST: &str = "api.github.com";

/// Suffix for the freshly downloaded executable, before it is swapped in.
/// Appended to the *whole* filename (`ShinraMeter-BPSR.exe.new`) rather
/// than replacing the extension, so the staged file can never be
/// double-clicked by mistake and never collides with a legitimate
/// `.exe` in the same folder.
const STAGED_SUFFIX: &str = ".new";

/// Suffix for the outgoing executable once it has been renamed aside. Same
/// whole-filename-append reasoning as `STAGED_SUFFIX`; see the module doc
/// comment for why the old file has to survive until the next launch.
const BACKUP_SUFFIX: &str = ".old";

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
    /// (e.g. `"v0.3.0"`), `url` is that release's own GitHub page
    /// (`html_url` from the API response), and `asset_url` is the
    /// downloadable Windows executable's `browser_download_url` when the
    /// release publishes one.
    ///
    /// `asset_url` is an `Option` rather than a required field because a
    /// release can legitimately not have one: every release tagged before
    /// issue #249 shipped a `.zip`, not a bare `.exe`, and a release whose
    /// upload failed halfway has no assets at all. `ui::draw_header_menu`
    /// falls back to the plain "Download" link to `url` in that case, so an
    /// old or malformed release degrades to exactly the pre-#250 behaviour
    /// instead of offering an install button that cannot work.
    UpdateAvailable {
        tag: String,
        url: String,
        asset_url: Option<String>,
    },
}

/// One entry of a release's `assets` array, narrowed to the two fields
/// `select_asset_url` decides on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// Everything this module reads out of a `.../releases/latest` response,
/// after `select_asset_url` has already reduced the `assets` array to the
/// one download that matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub html_url: String,
    pub asset_url: Option<String>,
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

/// Picks the downloadable Windows executable out of a release's assets
/// (issue #250).
///
/// Since issue #249 a release publishes exactly one asset, the bare
/// executable, named `ShinraMeter-BPSR-<tag>-windows-x64.exe`. This does
/// not *require* that name, because a release with a hand-uploaded extra
/// (a checksum file, a debug build) must not break updating: it takes the
/// first asset whose name ends in `.exe`, preferring one whose name also
/// says `windows-x64` so a hypothetical second architecture cannot be
/// picked by upload order. The `.exe` match is ASCII-case-insensitive —
/// GitHub preserves the uploaded name's case, and a `.EXE` is still the
/// file we want.
///
/// `None` when nothing matches: a pre-#249 zip-only release, or a release
/// whose upload never completed. See `CheckOutcome::UpdateAvailable`'s doc
/// comment for what the UI does with that.
pub fn select_asset_url(assets: &[ReleaseAsset]) -> Option<String> {
    let is_exe = |asset: &&ReleaseAsset| {
        std::path::Path::new(&asset.name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    };
    assets
        .iter()
        .find(|asset| is_exe(asset) && asset.name.to_ascii_lowercase().contains("windows-x64"))
        .or_else(|| assets.iter().find(is_exe))
        .map(|asset| asset.browser_download_url.clone())
}

/// The subset of a GitHub `.../releases/latest` response this module reads.
/// `#[serde(deny_unknown_fields)]` is deliberately *not* set — the real
/// response has dozens of other fields (`id`, `author`, `body`, ...) that
/// this module has no use for, and `serde_json` ignores anything not named
/// here by default. `assets` defaults to empty rather than being required
/// for the same reason `CheckOutcome::UpdateAvailable::asset_url` is an
/// `Option`: a response without it should degrade to the link, not fail the
/// whole check.
#[derive(Debug, serde::Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

/// Pulls the release tag, its GitHub page URL and its downloadable
/// executable out of a `.../releases/latest` API response body.
///
/// `Err` for anything that doesn't deserialize as a JSON object with
/// `tag_name` and `html_url` present as strings — a malformed body, an HTML
/// error page (rate limiting and some outages return one instead of JSON),
/// or a future API shape change all land here as a message for the
/// dropdown's error line, rather than panicking. A missing or unusable
/// `assets` array is *not* an error; see `select_asset_url`.
pub fn parse_release_response(json: &str) -> Result<ReleaseInfo, String> {
    serde_json::from_str::<ReleaseResponse>(json)
        .map(|response| ReleaseInfo {
            tag_name: response.tag_name,
            html_url: response.html_url,
            asset_url: select_asset_url(&response.assets),
        })
        .map_err(|err| format!("couldn't parse the GitHub releases response: {err}"))
}

/// The pure decision at the heart of the update check: given the running
/// build's version and what `parse_release_response` pulled out of the API
/// response, decide `CheckOutcome`. Free of any I/O — no network, no
/// `platform` dependency — so it and everything it calls
/// (`parse_version_tag`, `is_newer`) are exercised directly by the tests
/// below, without a Windows host or a live GitHub API to talk to.
/// `check_for_update` is the only caller that hands this real data; the
/// tests call it directly.
pub fn decide(current_version: &str, release: &ReleaseInfo) -> Result<CheckOutcome, String> {
    let current = parse_version_tag(current_version).ok_or_else(|| {
        format!("the running build's version {current_version:?} isn't a plain MAJOR.MINOR.PATCH")
    })?;
    let remote = parse_version_tag(&release.tag_name).ok_or_else(|| {
        format!(
            "the latest release tag {:?} isn't a plain vMAJOR.MINOR.PATCH tag",
            release.tag_name
        )
    })?;
    if is_newer(current, remote) {
        Ok(CheckOutcome::UpdateAvailable {
            tag: release.tag_name.clone(),
            url: release.html_url.clone(),
            asset_url: release.asset_url.clone(),
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
    let body = crate::platform::http_get(GITHUB_API_HOST, &path, &user_agent(current_version))?;
    let release = parse_release_response(&body)?;
    decide(current_version, &release)
}

/// The `User-Agent` both requests send. GitHub's API rejects a request that
/// arrives without one, and the download endpoint is happier with one too;
/// naming the running version makes a rate-limit or abuse report traceable
/// to a specific build.
fn user_agent(current_version: &str) -> String {
    format!("ShinraMeter-BPSR/{current_version}")
}

/// Whether a download host is one this app is willing to fetch an
/// executable from (issue #250).
///
/// This is a security check, not a tidiness one. The downloaded file is
/// swapped over the running executable and then relaunched *as
/// Administrator* (the app's manifest is `requireAdministrator`), so the
/// only thing standing between a bad `browser_download_url` and arbitrary
/// elevated code execution is that the URL came from a TLS-verified
/// `api.github.com` response. Pinning the host closes the gap where a
/// compromised or spoofed release body points the download somewhere else
/// entirely, and it costs nothing: GitHub's own
/// `browser_download_url` is always
/// `https://github.com/<owner>/<repo>/releases/download/...`.
///
/// Subdomains of `github.com` are accepted because GitHub has moved asset
/// hosting between hostnames before; the redirect *out* of `github.com` (to
/// `objects.githubusercontent.com`) is followed inside WinHTTP under its
/// no-HTTPS-downgrade policy and never reaches this check — see
/// `platform::http_get_bytes`.
pub fn is_trusted_download_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com" || host.ends_with(".github.com")
}

/// Splits an `https://host/path` URL into the `(host, path)` pair
/// `platform::http_get_bytes` wants — it takes the two separately because
/// that is the shape `WinHttpConnect`/`WinHttpOpenRequest` want — and
/// refuses anything that isn't a plain HTTPS URL on a trusted host.
///
/// Rejected, each for a concrete reason rather than out of strictness:
/// a non-`https://` scheme, because `http_get_bytes` has no plain-HTTP path
/// at all and would otherwise treat the whole string as a hostname; an
/// empty host; a host carrying an explicit port or userinfo, because
/// `http_get_bytes` always connects on `INTERNET_DEFAULT_HTTPS_PORT` and
/// would silently ignore the port it was told to use; and any host
/// `is_trusted_download_host` doesn't vouch for.
///
/// A URL with no path at all becomes `"/"`, which is what WinHTTP expects
/// for a root request — it never happens for a real asset URL, but leaving
/// an empty object string to WinHTTP is not worth finding out about at
/// runtime.
pub fn split_download_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("the release asset URL {url:?} is not an https:// URL"))?;
    let (host, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if host.is_empty() {
        return Err(format!("the release asset URL {url:?} has no host"));
    }
    if host.contains('@') || host.contains(':') {
        return Err(format!(
            "the release asset URL {url:?} carries a port or credentials, which this downloader doesn't support"
        ));
    }
    if !is_trusted_download_host(host) {
        return Err(format!(
            "the release asset URL {url:?} doesn't point at github.com; refusing to download an executable from it"
        ));
    }
    Ok((host.to_string(), path.to_string()))
}

/// The three filenames one in-place update touches — see the module doc
/// comment for the rename dance they take part in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    /// The running executable, and where the new one ends up.
    pub target: PathBuf,
    /// Where the download is written before anything is moved.
    pub staged: PathBuf,
    /// Where the outgoing executable is renamed to, since Windows won't let
    /// a running image be deleted or overwritten.
    pub backup: PathBuf,
}

/// Works out the staged/backup paths for an executable. Pure string work on
/// a path, so the tests below pin the exact names without touching a disk.
///
/// The suffixes are appended to the whole filename rather than replacing
/// the extension (`ShinraMeter-BPSR.exe` → `ShinraMeter-BPSR.exe.new`, not
/// `ShinraMeter-BPSR.new`) so the staged and backup files are never
/// themselves runnable `.exe`s, and both stay siblings of the target so
/// every rename is same-directory — see the module doc comment on why
/// `%TEMP%` is the wrong place for this.
pub fn update_paths(exe: &Path) -> UpdatePaths {
    let with_suffix = |suffix: &str| {
        let mut name = exe.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    UpdatePaths {
        target: exe.to_path_buf(),
        staged: with_suffix(STAGED_SUFFIX),
        backup: with_suffix(BACKUP_SUFFIX),
    }
}

/// Whether a downloaded body actually looks like a Windows executable.
///
/// `MZ` is the DOS header magic every PE image on Windows still starts
/// with. The check exists because the failure it catches is otherwise
/// silent and nasty: a captive portal, a proxy error page or a GitHub
/// maintenance page comes back with HTTP 200 and a few kilobytes of HTML,
/// and without this the app would cheerfully rename that over its own
/// executable and relaunch it. Refusing early leaves the installation
/// untouched.
///
/// Two bytes is not integrity verification and is not claimed to be — a
/// real check would need a signature or a published checksum, neither of
/// which the release pipeline produces today. What makes the download
/// trustworthy is that its URL came from a TLS-verified `api.github.com`
/// response and points at a pinned host (`split_download_url`).
pub fn looks_like_windows_executable(bytes: &[u8]) -> bool {
    bytes.starts_with(b"MZ")
}

/// Writes a downloaded executable to the staged path, refusing a body that
/// isn't one. Any previously staged file is replaced — a half-finished
/// download from an earlier attempt must never be the thing that gets
/// swapped in.
pub fn stage_downloaded_executable(paths: &UpdatePaths, bytes: &[u8]) -> Result<(), String> {
    if !looks_like_windows_executable(bytes) {
        return Err(format!(
            "the downloaded release asset isn't a Windows executable ({} bytes, no MZ header) — the download probably returned an error page",
            bytes.len()
        ));
    }
    std::fs::write(&paths.staged, bytes).map_err(|err| {
        format!(
            "couldn't write the downloaded update to {}: {err}",
            paths.staged.display()
        )
    })
}

/// The rename dance itself (issue #250): move the running executable aside
/// and the staged download into its place.
///
/// Nothing here is Windows-specific — it is three `std::fs::rename` calls
/// against paths the caller supplies — which is exactly why the tests below
/// can drive the real function in a scratch directory instead of mocking
/// it. What is Windows-specific is *why* it has to be a rename rather than
/// an overwrite; see the module doc comment.
///
/// On a failed second rename the backup is renamed back, so a partial
/// update leaves the previous executable in place rather than no executable
/// at all. If even that fails the error names where the previous build now
/// lives, so a user can rename it back by hand rather than being left with
/// an empty folder and no idea what happened.
pub fn swap_in_staged_executable(paths: &UpdatePaths) -> Result<(), String> {
    if paths.backup.exists()
        && let Err(err) = std::fs::remove_file(&paths.backup)
    {
        return Err(format!(
            "couldn't clear the leftover {} from a previous update: {err}",
            paths.backup.display()
        ));
    }
    std::fs::rename(&paths.target, &paths.backup).map_err(|err| {
        format!(
            "couldn't move the running executable aside to {}: {err}",
            paths.backup.display()
        )
    })?;
    if let Err(err) = std::fs::rename(&paths.staged, &paths.target) {
        return Err(match std::fs::rename(&paths.backup, &paths.target) {
            Ok(()) => format!(
                "couldn't move the downloaded update into place ({err}); the existing installation was left untouched"
            ),
            Err(restore_err) => format!(
                "couldn't move the downloaded update into place ({err}), and putting the previous executable back also failed ({restore_err}) — it is still at {}",
                paths.backup.display()
            ),
        });
    }
    Ok(())
}

/// Deletes what the previous in-place update left behind, for an
/// executable path the caller supplies. Returns one message per leftover it
/// could not remove, so the no-argument `clean_up_previous_update` below
/// can log them and the tests here can assert on them.
///
/// Both leftovers are best-effort by design and neither is an error worth
/// showing the user: the `.old` file is the *previous* build, which by this
/// point nothing has open (the process that held it is the one that exited
/// to make room for this one), and a stray `.new` means an earlier download
/// finished but its swap didn't — in both cases the installation currently
/// running is correct and the only cost of failing to delete is a file
/// sitting in a folder.
pub fn clean_up_previous_update_for(exe: &Path) -> Vec<String> {
    let paths = update_paths(exe);
    let mut problems = Vec::new();
    for leftover in [&paths.backup, &paths.staged] {
        if !leftover.exists() {
            continue;
        }
        if let Err(err) = std::fs::remove_file(leftover) {
            problems.push(format!("couldn't remove {}: {err}", leftover.display()));
        }
    }
    problems
}

/// Startup hook for the in-place updater (issue #250): removes the previous
/// update's renamed-aside executable now that nothing holds it open. Called
/// once from `main`, before the window exists — see the module doc comment
/// for why the file survives that long in the first place.
///
/// Silent on success and never fatal: a build that cannot tidy up after
/// itself is still a working build.
pub fn clean_up_previous_update() {
    let Ok(exe) = std::env::current_exe() else {
        // No `current_exe` means no paths to derive; nothing to clean.
        return;
    };
    for problem in clean_up_previous_update_for(&exe) {
        log::warn!("leftover from a previous in-place update: {problem}");
    }
}

/// Runs a whole in-place update (issue #250): downloads `asset_url`,
/// verifies it looks like an executable, stages it beside the running one
/// and swaps it in. Returns the path the caller should relaunch.
///
/// Blocking, and — like `check_for_update` — only ever called from a
/// spawned `std::thread`: this one downloads several megabytes, so calling
/// it on the UI thread would freeze the overlay for the length of the
/// download rather than merely for a request round-trip.
///
/// Does *not* relaunch or exit. The process lifecycle belongs to the UI
/// thread (`ui::OverlayApp::poll_update_check`), which relaunches through
/// `relaunch` and then asks the viewport to close, so this function stays a
/// pure "put the new file where the old one was" step with a `Result` a
/// caller can render.
pub fn install_update(asset_url: &str, current_version: &str) -> Result<PathBuf, String> {
    let (host, path) = split_download_url(asset_url)?;
    let exe = std::env::current_exe()
        .map_err(|err| format!("couldn't find the running executable's own path: {err}"))?;
    let paths = update_paths(&exe);
    let bytes = crate::platform::http_get_bytes(&host, &path, &user_agent(current_version))?;
    stage_downloaded_executable(&paths, &bytes)?;
    swap_in_staged_executable(&paths)?;
    Ok(paths.target)
}

/// Starts the just-installed executable. Called from the UI thread the
/// frame the install lands, immediately before the viewport is asked to
/// close.
///
/// The child is spawned and deliberately not waited on — this process is
/// about to exit, and the whole point is for the new one to outlive it. It
/// inherits this process's elevated token, so the `requireAdministrator`
/// manifest costs the user no second UAC prompt.
///
/// `current_dir` is pinned to the executable's own folder so the new
/// process starts where the old one's *file* lives rather than wherever the
/// old one's working directory happened to point.
///
/// Issue #277 made "not waited on" something the child has to be told about:
/// this process still holds the single-instance lock, and goes on holding it
/// through window teardown and four thread joins, so the child would reach
/// `single_instance::acquire` first and be refused as a second copy — the
/// update would read as the app closing itself. `HANDOFF_VAR` marks the
/// child as this process's successor, which makes it wait for the slot
/// rather than refuse it. Set here, on the one spawn that has the right to
/// claim it, and never in the environment generally.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    let mut command = std::process::Command::new(exe);
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    command.env(crate::single_instance::HANDOFF_VAR, "1");
    command
        .spawn()
        .map(|_child| ())
        .map_err(|err| format!("couldn't relaunch {}: {err}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own, in the same
    /// `std::env::temp_dir()` + pid + counter shape `settings.rs`,
    /// `dump.rs` and `history/mod.rs` already use — the crate has no
    /// `tempfile` dev-dependency and this needs no more than they do.
    fn scratch_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bpsr-update-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("couldn't create the scratch directory");
        dir
    }

    /// A byte string that passes `looks_like_windows_executable`, standing
    /// in for a real PE image — the swap logic never looks past the header.
    fn fake_exe(marker: &str) -> Vec<u8> {
        let mut bytes = b"MZ\x90\x00".to_vec();
        bytes.extend_from_slice(marker.as_bytes());
        bytes
    }

    fn release(tag: &str, url: &str, asset_url: Option<&str>) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: tag.to_string(),
            html_url: url.to_string(),
            asset_url: asset_url.map(str::to_string),
        }
    }

    fn asset(name: &str, url: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: url.to_string(),
        }
    }

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

    // -- select_asset_url (issue #250) ----------------------------------

    #[test]
    fn select_asset_url_picks_the_windows_x64_exe() {
        let assets = [
            asset(
                "checksums.txt",
                "https://github.com/x/y/releases/download/v1/checksums.txt",
            ),
            asset(
                "ShinraMeter-BPSR-v0.3.0-windows-x64.exe",
                "https://github.com/x/y/releases/download/v1/ShinraMeter-BPSR-v0.3.0-windows-x64.exe",
            ),
        ];
        assert_eq!(
            select_asset_url(&assets).as_deref(),
            Some(
                "https://github.com/x/y/releases/download/v1/ShinraMeter-BPSR-v0.3.0-windows-x64.exe"
            )
        );
    }

    /// Upload order must not decide which architecture gets installed, so
    /// the `windows-x64` preference beats "first `.exe` in the array".
    #[test]
    fn select_asset_url_prefers_windows_x64_over_upload_order() {
        let assets = [
            asset(
                "ShinraMeter-BPSR-v0.3.0-windows-arm64.exe",
                "https://example/arm64.exe",
            ),
            asset(
                "ShinraMeter-BPSR-v0.3.0-windows-x64.exe",
                "https://example/x64.exe",
            ),
        ];
        assert_eq!(
            select_asset_url(&assets).as_deref(),
            Some("https://example/x64.exe")
        );
    }

    /// A release whose one executable doesn't follow the #249 naming (a
    /// hand-uploaded build, say) still updates rather than falling back to
    /// the browser link.
    #[test]
    fn select_asset_url_falls_back_to_any_exe() {
        let assets = [asset("ShinraMeter-BPSR.exe", "https://example/plain.exe")];
        assert_eq!(
            select_asset_url(&assets).as_deref(),
            Some("https://example/plain.exe")
        );
    }

    #[test]
    fn select_asset_url_matches_the_extension_case_insensitively() {
        let assets = [asset("ShinraMeter-BPSR.EXE", "https://example/shouty.EXE")];
        assert_eq!(
            select_asset_url(&assets).as_deref(),
            Some("https://example/shouty.EXE")
        );
    }

    /// Every release tagged before issue #249 published a zip. Those must
    /// resolve to "no asset" so the UI keeps offering the plain link.
    #[test]
    fn select_asset_url_ignores_a_pre_249_zip_only_release() {
        let assets = [asset(
            "ShinraMeter-BPSR-v0.2.5-windows-x64.zip",
            "https://example/old.zip",
        )];
        assert_eq!(select_asset_url(&assets), None);
    }

    #[test]
    fn select_asset_url_is_none_for_a_release_with_no_assets() {
        assert_eq!(select_asset_url(&[]), None);
    }

    /// A name that merely *contains* `.exe` somewhere isn't an executable;
    /// only the actual extension counts.
    #[test]
    fn select_asset_url_ignores_a_name_that_only_mentions_exe() {
        let assets = [asset(
            "how-to-run-the.exe.txt",
            "https://example/readme.txt",
        )];
        assert_eq!(select_asset_url(&assets), None);
    }

    // -- parse_release_response -----------------------------------------

    #[test]
    fn parse_release_response_reads_tag_url_and_asset() {
        // Shaped like a real `/releases/latest` body, trimmed to the fields
        // this module reads plus a few it deliberately ignores.
        let json = r#"{
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0",
            "id": 12345,
            "name": "v0.3.0",
            "prerelease": false,
            "assets": [
                {
                    "name": "ShinraMeter-BPSR-v0.3.0-windows-x64.exe",
                    "size": 41234567,
                    "content_type": "application/x-msdownload",
                    "browser_download_url": "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/ShinraMeter-BPSR-v0.3.0-windows-x64.exe"
                }
            ]
        }"#;
        assert_eq!(
            parse_release_response(json),
            Ok(release(
                "v0.3.0",
                "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/tag/v0.3.0",
                Some(
                    "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/ShinraMeter-BPSR-v0.3.0-windows-x64.exe"
                ),
            ))
        );
    }

    #[test]
    fn parse_release_response_tolerates_a_response_with_no_assets_field() {
        let json = r#"{"tag_name": "v0.3.0", "html_url": "https://example.com"}"#;
        assert_eq!(
            parse_release_response(json),
            Ok(release("v0.3.0", "https://example.com", None))
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
            decide("0.2.0", &release("v0.2.0", "https://example.com", None)),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn decide_reports_up_to_date_when_remote_is_older() {
        assert_eq!(
            decide("0.2.0", &release("v0.1.9", "https://example.com", None)),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn decide_carries_the_asset_url_through_when_remote_is_newer() {
        assert_eq!(
            decide(
                "0.2.0",
                &release(
                    "v0.3.0",
                    "https://example.com/releases/tag/v0.3.0",
                    Some("https://github.com/x/y/releases/download/v0.3.0/app.exe"),
                )
            ),
            Ok(CheckOutcome::UpdateAvailable {
                tag: "v0.3.0".to_string(),
                url: "https://example.com/releases/tag/v0.3.0".to_string(),
                asset_url: Some(
                    "https://github.com/x/y/releases/download/v0.3.0/app.exe".to_string()
                ),
            })
        );
    }

    /// The link-only fallback the UI needs for a pre-#249 release.
    #[test]
    fn decide_reports_an_update_with_no_asset_when_the_release_has_none() {
        assert_eq!(
            decide("0.2.0", &release("v0.3.0", "https://example.com", None)),
            Ok(CheckOutcome::UpdateAvailable {
                tag: "v0.3.0".to_string(),
                url: "https://example.com".to_string(),
                asset_url: None,
            })
        );
    }

    #[test]
    fn decide_errors_on_a_malformed_remote_tag_instead_of_guessing() {
        assert!(
            decide(
                "0.2.0",
                &release("not-a-version", "https://example.com", None)
            )
            .is_err()
        );
    }

    #[test]
    fn decide_errors_on_a_malformed_current_version() {
        assert!(decide("garbage", &release("v0.2.0", "https://example.com", None)).is_err());
    }

    // -- split_download_url (issue #250) --------------------------------

    #[test]
    fn split_download_url_splits_a_real_asset_url() {
        assert_eq!(
            split_download_url(
                "https://github.com/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/ShinraMeter-BPSR-v0.3.0-windows-x64.exe"
            ),
            Ok((
                "github.com".to_string(),
                "/NguyenJus/ShinraMeter-BPSR/releases/download/v0.3.0/ShinraMeter-BPSR-v0.3.0-windows-x64.exe"
                    .to_string()
            ))
        );
    }

    #[test]
    fn split_download_url_defaults_a_pathless_url_to_root() {
        assert_eq!(
            split_download_url("https://github.com"),
            Ok(("github.com".to_string(), "/".to_string()))
        );
    }

    #[test]
    fn split_download_url_rejects_plain_http() {
        assert!(split_download_url("http://github.com/x/y.exe").is_err());
    }

    #[test]
    fn split_download_url_rejects_a_url_with_no_scheme() {
        assert!(split_download_url("github.com/x/y.exe").is_err());
    }

    #[test]
    fn split_download_url_rejects_an_empty_host() {
        assert!(split_download_url("https:///x/y.exe").is_err());
    }

    /// `http_get_bytes` always connects on 443, so a URL naming another
    /// port would be silently downloaded from the wrong place.
    #[test]
    fn split_download_url_rejects_an_explicit_port() {
        assert!(split_download_url("https://github.com:8443/x/y.exe").is_err());
    }

    /// `https://github.com@evil.example/...` is a host of `evil.example`.
    /// Rejecting userinfo outright means the host check can't be fooled by
    /// one.
    #[test]
    fn split_download_url_rejects_userinfo() {
        assert!(split_download_url("https://github.com@evil.example/x/y.exe").is_err());
    }

    /// The whole point of the host pin: this executable is about to be run
    /// as Administrator.
    #[test]
    fn split_download_url_rejects_a_host_outside_github() {
        assert!(split_download_url("https://evil.example/x/y.exe").is_err());
    }

    #[test]
    fn split_download_url_rejects_a_lookalike_host() {
        assert!(split_download_url("https://github.com.evil.example/x/y.exe").is_err());
    }

    #[test]
    fn is_trusted_download_host_accepts_github_and_its_subdomains() {
        assert!(is_trusted_download_host("github.com"));
        assert!(is_trusted_download_host("GitHub.com"));
        assert!(is_trusted_download_host("release-assets.github.com"));
        assert!(!is_trusted_download_host("githubXcom"));
        assert!(!is_trusted_download_host("notgithub.com"));
    }

    // -- update_paths ---------------------------------------------------

    #[test]
    fn update_paths_appends_to_the_whole_filename_beside_the_target() {
        let paths = update_paths(Path::new("/opt/meter/ShinraMeter-BPSR.exe"));
        assert_eq!(paths.target, Path::new("/opt/meter/ShinraMeter-BPSR.exe"));
        assert_eq!(
            paths.staged,
            Path::new("/opt/meter/ShinraMeter-BPSR.exe.new")
        );
        assert_eq!(
            paths.backup,
            Path::new("/opt/meter/ShinraMeter-BPSR.exe.old")
        );
    }

    /// Every rename in `swap_in_staged_executable` has to be
    /// same-directory, or it can cross a volume boundary and fail — see the
    /// module doc comment.
    #[test]
    fn update_paths_keeps_every_path_in_the_targets_directory() {
        let paths = update_paths(Path::new("/opt/meter/ShinraMeter-BPSR.exe"));
        assert_eq!(paths.staged.parent(), paths.target.parent());
        assert_eq!(paths.backup.parent(), paths.target.parent());
    }

    /// Neither staged file may itself be a double-clickable `.exe`.
    #[test]
    fn update_paths_leaves_no_second_runnable_exe_beside_the_target() {
        let paths = update_paths(Path::new("/opt/meter/ShinraMeter-BPSR.exe"));
        assert_ne!(paths.staged.extension().unwrap(), "exe");
        assert_ne!(paths.backup.extension().unwrap(), "exe");
    }

    // -- looks_like_windows_executable ----------------------------------

    #[test]
    fn looks_like_windows_executable_accepts_an_mz_header() {
        assert!(looks_like_windows_executable(&fake_exe("body")));
    }

    #[test]
    fn looks_like_windows_executable_rejects_an_html_error_page() {
        assert!(!looks_like_windows_executable(
            b"<html><head><title>502 Bad Gateway"
        ));
    }

    #[test]
    fn looks_like_windows_executable_rejects_an_empty_body() {
        assert!(!looks_like_windows_executable(b""));
    }

    // -- stage_downloaded_executable ------------------------------------

    #[test]
    fn stage_downloaded_executable_writes_the_body_to_the_staged_path() {
        let dir = scratch_dir("stage");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        stage_downloaded_executable(&paths, &fake_exe("new build")).expect("staging failed");
        assert_eq!(std::fs::read(&paths.staged).unwrap(), fake_exe("new build"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_downloaded_executable_replaces_a_half_finished_earlier_download() {
        let dir = scratch_dir("restage");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        std::fs::write(&paths.staged, b"MZ truncated leftover").unwrap();
        stage_downloaded_executable(&paths, &fake_exe("complete")).expect("staging failed");
        assert_eq!(std::fs::read(&paths.staged).unwrap(), fake_exe("complete"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An HTTP 200 carrying a captive-portal page must not be written where
    /// the swap would pick it up.
    #[test]
    fn stage_downloaded_executable_refuses_a_body_that_isnt_an_executable() {
        let dir = scratch_dir("stage-html");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        assert!(stage_downloaded_executable(&paths, b"<html>sign in to the wifi</html>").is_err());
        assert!(!paths.staged.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- swap_in_staged_executable --------------------------------------

    #[test]
    fn swap_in_staged_executable_moves_the_new_build_into_place() {
        let dir = scratch_dir("swap");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        std::fs::write(&paths.target, fake_exe("old build")).unwrap();
        std::fs::write(&paths.staged, fake_exe("new build")).unwrap();

        swap_in_staged_executable(&paths).expect("swap failed");

        assert_eq!(std::fs::read(&paths.target).unwrap(), fake_exe("new build"));
        // The outgoing build survives as the backup: it is still the
        // running process's image at this point on Windows, and only the
        // next launch may delete it.
        assert_eq!(std::fs::read(&paths.backup).unwrap(), fake_exe("old build"));
        assert!(!paths.staged.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second update in the same folder must not trip over the first
    /// one's leftover backup.
    #[test]
    fn swap_in_staged_executable_replaces_a_leftover_backup() {
        let dir = scratch_dir("swap-leftover");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        std::fs::write(&paths.target, fake_exe("current")).unwrap();
        std::fs::write(&paths.staged, fake_exe("next")).unwrap();
        std::fs::write(&paths.backup, fake_exe("ancient")).unwrap();

        swap_in_staged_executable(&paths).expect("swap failed");

        assert_eq!(std::fs::read(&paths.target).unwrap(), fake_exe("next"));
        assert_eq!(std::fs::read(&paths.backup).unwrap(), fake_exe("current"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure that matters: if the second rename can't happen, the
    /// user must still have a working executable where they left it.
    #[test]
    fn swap_in_staged_executable_restores_the_backup_when_the_staged_file_is_gone() {
        let dir = scratch_dir("swap-rollback");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        std::fs::write(&paths.target, fake_exe("old build")).unwrap();
        // No staged file at all — the second rename cannot succeed.

        let err = swap_in_staged_executable(&paths).expect_err("swap should have failed");
        assert!(err.contains("left untouched"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&paths.target).unwrap(), fake_exe("old build"));
        assert!(!paths.backup.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_in_staged_executable_errors_when_there_is_nothing_to_replace() {
        let dir = scratch_dir("swap-no-target");
        let paths = update_paths(&dir.join("ShinraMeter-BPSR.exe"));
        std::fs::write(&paths.staged, fake_exe("new build")).unwrap();
        assert!(swap_in_staged_executable(&paths).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- clean_up_previous_update_for -----------------------------------

    #[test]
    fn clean_up_previous_update_removes_both_leftovers_and_spares_the_executable() {
        let dir = scratch_dir("cleanup");
        let exe = dir.join("ShinraMeter-BPSR.exe");
        let paths = update_paths(&exe);
        std::fs::write(&paths.target, fake_exe("running")).unwrap();
        std::fs::write(&paths.backup, fake_exe("previous")).unwrap();
        std::fs::write(&paths.staged, fake_exe("abandoned")).unwrap();

        assert_eq!(clean_up_previous_update_for(&exe), Vec::<String>::new());

        assert!(!paths.backup.exists());
        assert!(!paths.staged.exists());
        assert_eq!(std::fs::read(&paths.target).unwrap(), fake_exe("running"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_up_previous_update_is_a_no_op_with_nothing_to_clean() {
        let dir = scratch_dir("cleanup-empty");
        let exe = dir.join("ShinraMeter-BPSR.exe");
        std::fs::write(&exe, fake_exe("running")).unwrap();
        assert_eq!(clean_up_previous_update_for(&exe), Vec::<String>::new());
        assert!(exe.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- install_update -------------------------------------------------

    /// The URL check runs before `current_exe`, the network, or any file
    /// write, so a bad asset URL can never reach the download at all. This
    /// is the one assertion about `install_update` that holds on a
    /// non-Windows host — everything past the URL check needs a real
    /// WinHTTP request, which `platform::http_get_bytes` stubs out here.
    #[test]
    fn install_update_refuses_an_untrusted_asset_url_before_touching_anything() {
        let err = install_update("https://evil.example/payload.exe", "0.2.0")
            .expect_err("an untrusted host must be refused");
        assert!(err.contains("github.com"), "unexpected error: {err}");
    }

    #[test]
    fn install_update_refuses_a_non_https_asset_url() {
        assert!(install_update("http://github.com/x/y.exe", "0.2.0").is_err());
    }
}
