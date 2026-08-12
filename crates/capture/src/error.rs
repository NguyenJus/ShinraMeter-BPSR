//! Capture error types shared by the (cfg-gated) platform backends.

use thiserror::Error;

/// Why Windows refused to load the WinDivert driver.
///
/// These are distinct failures with distinct fixes, and the user-facing text
/// says which one happened: they used to collapse into a single "place the
/// driver files next to the exe" message, which sent people looking for files
/// that (now that the runtime is embedded) they do not own in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverRejection {
    /// `ERROR_FILE_NOT_FOUND` — the DLL could not find `WinDivert64.sys`
    /// where it looked for it.
    SysNotFound,
    /// `ERROR_DRIVER_BLOCKED` — something (AV, VPN filter, Memory Integrity)
    /// vetoed the driver.
    Blocked,
    /// `ERROR_INVALID_IMAGE_HASH` — the driver's signature was rejected.
    InvalidSignature,
    /// `ERROR_DRIVER_FAILED_PRIOR_UNLOAD` — a different WinDivert version is
    /// already resident.
    VersionConflict,
    /// `EPT_S_NOT_REGISTERED` — the Base Filtering Engine service is off.
    FilteringEngineDisabled,
}

impl DriverRejection {
    /// The exact text the UI shows for this rejection.
    fn user_message(self) -> &'static str {
        match self {
            DriverRejection::SysNotFound => {
                "Windows could not find the WinDivert driver file. It ships inside this \
                 executable and is unpacked on startup — check that antivirus is not \
                 deleting it."
            }
            DriverRejection::Blocked => {
                "Windows blocked the WinDivert driver. Antivirus, a VPN filter (NordVPN is \
                 a known conflict), or Core Isolation / Memory Integrity is preventing it \
                 from loading."
            }
            DriverRejection::InvalidSignature => {
                "Windows rejected the WinDivert driver's digital signature. Check your \
                 Secure Boot and Memory Integrity policy."
            }
            DriverRejection::VersionConflict => {
                "A different version of the WinDivert driver is already loaded. Close other \
                 packet-capture tools (or reboot) and try again."
            }
            DriverRejection::FilteringEngineDisabled => {
                "The Base Filtering Engine service is disabled. Start it (services.msc → \
                 Base Filtering Engine) and try again."
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("packet capture is not supported on this platform")]
    UnsupportedPlatform,
    #[error("could not unpack the bundled WinDivert runtime: {0}")]
    RuntimeUnpack(String),
    #[error("could not load the bundled WinDivert library: {0}")]
    RuntimeLoad(String),
    #[error("Windows refused the WinDivert driver: {0:?}")]
    DriverRejected(DriverRejection),
    #[error("not running as administrator")]
    NotElevated,
    #[error("failed to open capture device: {0}")]
    Open(String),
}

// Win32 status codes `WinDivertOpen` reports through `GetLastError`, per the
// WinDivert documentation. Kept here, off the Windows-only path, so the
// mapping below stays host-testable — it is the part that was previously
// wrong, and CI only runs tests on Linux.
/// `ERROR_FILE_NOT_FOUND`: the DLL could not find `WinDivert64.sys`.
pub(crate) const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_INVALID_IMAGE_HASH: i32 = 577;
const ERROR_DRIVER_FAILED_PRIOR_UNLOAD: i32 = 654;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_DRIVER_BLOCKED: i32 = 1275;
const EPT_S_NOT_REGISTERED: i32 = 1753;

impl CaptureError {
    /// Classifies a `WinDivertOpen` failure.
    ///
    /// Each rejection keeps its identity all the way to the status banner: the
    /// fixes (elevate, unblock in your AV, start a service, reboot) have
    /// nothing in common, so collapsing them into one message helps nobody.
    pub(crate) fn from_open_error(err: &std::io::Error) -> Self {
        let rejection = match err.raw_os_error() {
            Some(ERROR_ACCESS_DENIED) => return CaptureError::NotElevated,
            Some(ERROR_FILE_NOT_FOUND | ERROR_SERVICE_DOES_NOT_EXIST) => {
                DriverRejection::SysNotFound
            }
            Some(ERROR_DRIVER_BLOCKED) => DriverRejection::Blocked,
            Some(ERROR_INVALID_IMAGE_HASH) => DriverRejection::InvalidSignature,
            Some(ERROR_DRIVER_FAILED_PRIOR_UNLOAD) => DriverRejection::VersionConflict,
            Some(EPT_S_NOT_REGISTERED) => DriverRejection::FilteringEngineDisabled,
            _ => return CaptureError::Open(err.to_string()),
        };
        CaptureError::DriverRejected(rejection)
    }

    /// The exact text the UI shows for this error.
    pub fn user_message(&self) -> &'static str {
        match self {
            CaptureError::UnsupportedPlatform => "Packet capture is only supported on Windows.",
            CaptureError::RuntimeUnpack(_) => {
                "Could not unpack the bundled WinDivert driver. Check that antivirus is not \
                 blocking writes to your AppData folder."
            }
            CaptureError::RuntimeLoad(_) => {
                "Could not load the bundled WinDivert library after unpacking it."
            }
            CaptureError::DriverRejected(rejection) => rejection.user_message(),
            CaptureError::NotElevated => "Run as Administrator.",
            CaptureError::Open(_) => "Failed to open the capture device.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(code: i32) -> CaptureError {
        CaptureError::from_open_error(&std::io::Error::from_raw_os_error(code))
    }

    #[test]
    fn access_denied_maps_to_not_elevated() {
        assert!(matches!(
            classify(ERROR_ACCESS_DENIED),
            CaptureError::NotElevated
        ));
    }

    /// The specific conflation that used to send users hunting for files:
    /// a blocked driver and a missing one are different problems.
    #[test]
    fn blocked_and_missing_do_not_collapse_together() {
        assert!(matches!(
            classify(ERROR_DRIVER_BLOCKED),
            CaptureError::DriverRejected(DriverRejection::Blocked)
        ));
        assert!(matches!(
            classify(ERROR_FILE_NOT_FOUND),
            CaptureError::DriverRejected(DriverRejection::SysNotFound)
        ));
    }

    #[test]
    fn each_documented_code_maps_to_its_own_rejection() {
        let cases = [
            (ERROR_INVALID_IMAGE_HASH, DriverRejection::InvalidSignature),
            (
                ERROR_DRIVER_FAILED_PRIOR_UNLOAD,
                DriverRejection::VersionConflict,
            ),
            (
                EPT_S_NOT_REGISTERED,
                DriverRejection::FilteringEngineDisabled,
            ),
            (ERROR_SERVICE_DOES_NOT_EXIST, DriverRejection::SysNotFound),
        ];
        for (code, want) in cases {
            match classify(code) {
                CaptureError::DriverRejected(got) => assert_eq!(got, want, "code {code}"),
                other => panic!("code {code} classified as {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_codes_fall_through_to_open() {
        // ERROR_INVALID_PARAMETER: a bad filter string, not a driver problem.
        assert!(matches!(classify(87), CaptureError::Open(_)));
    }

    #[test]
    fn user_message_for_not_elevated_mentions_administrator() {
        let e = CaptureError::NotElevated;
        assert!(e.user_message().contains("Administrator"));
    }

    /// The regression this whole taxonomy exists to prevent: every rejection
    /// reason must produce its *own* sentence, so the banner never blames the
    /// wrong thing.
    #[test]
    fn every_driver_rejection_has_a_distinct_message() {
        let all = [
            DriverRejection::SysNotFound,
            DriverRejection::Blocked,
            DriverRejection::InvalidSignature,
            DriverRejection::VersionConflict,
            DriverRejection::FilteringEngineDisabled,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(
                    a.user_message(),
                    b.user_message(),
                    "{a:?} and {b:?} share a message"
                );
            }
        }
    }

    #[test]
    fn blocked_message_names_the_usual_culprits() {
        let e = CaptureError::DriverRejected(DriverRejection::Blocked);
        let msg = e.user_message();
        assert!(msg.contains("Antivirus"));
        assert!(msg.contains("Memory Integrity"));
    }

    /// The old message told users to place files next to the exe. The runtime
    /// is embedded now, so that instruction would be actively misleading.
    #[test]
    fn no_message_asks_the_user_to_supply_driver_files() {
        let errors = [
            CaptureError::UnsupportedPlatform,
            CaptureError::RuntimeUnpack(String::new()),
            CaptureError::RuntimeLoad(String::new()),
            CaptureError::DriverRejected(DriverRejection::SysNotFound),
            CaptureError::NotElevated,
            CaptureError::Open(String::new()),
        ];
        for e in &errors {
            assert!(
                !e.user_message().contains("next to the exe"),
                "{e:?} still asks for manually-placed files"
            );
        }
    }
}
