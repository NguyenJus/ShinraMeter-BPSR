pub mod backoff;
pub mod backpressure;
// pub rather than pub(crate) because the only in-crate caller is cfg(windows),
// so pub(crate) is dead_code on every other host.
pub mod clock;
pub mod detect;
pub mod error;
pub mod install;
pub mod owner;
pub mod restart;
pub mod tcp;
pub mod throughput;

#[cfg(windows)]
mod driver;
#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod win;

pub use restart::CaptureRestart;

#[cfg(not(windows))]
pub use stub::{CaptureHandle, start_capture};
#[cfg(windows)]
pub use win::{CaptureHandle, start_capture};
