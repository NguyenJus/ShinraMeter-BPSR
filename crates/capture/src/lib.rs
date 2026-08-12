pub mod detect;
pub mod error;
pub mod tcp;

#[cfg(windows)]
mod win;
#[cfg(not(windows))]
mod stub;

#[cfg(windows)]
pub use win::{CaptureHandle, start_capture};
#[cfg(not(windows))]
pub use stub::{CaptureHandle, start_capture};
