mod backoff;
pub mod detect;
pub mod error;
pub mod install;
pub mod tcp;

#[cfg(windows)]
mod driver;
#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
pub use stub::{CaptureHandle, start_capture};
#[cfg(windows)]
pub use win::{CaptureHandle, start_capture};
