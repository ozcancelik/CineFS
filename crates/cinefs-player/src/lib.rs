//! GPU RAW player for CineFS (wgpu): parallel frame decode on the CPU, debayer + colour on the GPU.

pub mod audio;
pub mod engine;
pub mod ffi;
pub mod gpu;
pub mod player;

// Re-exported so native shells use exactly the versions the player was built with.
pub use raw_window_handle;
pub use wgpu;

pub use cinefs_core::ffi::*; // keep the core C ABI inside this static library (one lib for the Mac app)

pub(crate) fn log_warn(msg: &str) {
    cinefs_core::log::write("WARN", msg);
}

pub(crate) fn log_info(msg: &str) {
    cinefs_core::log::write("INFO", msg);
}
