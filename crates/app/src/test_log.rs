//! Process-wide log capture shared by unit tests in this crate.

use std::sync::{Mutex, Once};

use log::{Level, LevelFilter, Log, Metadata, Record};

static CAPTURED: Mutex<Vec<CapturedRecord>> = Mutex::new(Vec::new());
static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

struct CapturedRecord {
    level: Level,
    message: String,
}

struct CaptureLogger;

impl Log for CaptureLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        CAPTURED
            .lock()
            .expect("test log capture mutex poisoned")
            .push(CapturedRecord {
                level: record.level(),
                message: record.args().to_string(),
            });
    }

    fn flush(&self) {}
}

/// Installs the crate's sole test logger. Safe to call from concurrent tests.
pub(crate) fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&CAPTURE_LOGGER).expect("install shared test logger");
        log::set_max_level(LevelFilter::Trace);
    });
}

/// Whether a captured record at `level` contains every supplied substring.
///
/// Log assertions should include a value unique to the calling test so they
/// remain valid while the process-wide capture buffer is shared concurrently.
pub(crate) fn logged(level: Level, needles: &[&str]) -> bool {
    CAPTURED
        .lock()
        .expect("test log capture mutex poisoned")
        .iter()
        .any(|record| {
            record.level == level && needles.iter().all(|needle| record.message.contains(needle))
        })
}

/// Returns captured records for assertion failure messages.
pub(crate) fn captured() -> Vec<String> {
    CAPTURED
        .lock()
        .expect("test log capture mutex poisoned")
        .iter()
        .map(|record| format!("{}: {}", record.level, record.message))
        .collect()
}
