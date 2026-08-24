pub mod backoff;
pub mod detect;
pub mod error;
pub mod install;
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
