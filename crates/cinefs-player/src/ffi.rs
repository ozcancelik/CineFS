//! C ABI for the native shells. The Mac app passes an `NSView*`; Windows uses the Rust API directly.

use crate::player::Player;
use cinefs_core::ffi::CineFsMountHandle;
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::raw::c_char;
#[cfg(target_os = "macos")]
use std::ptr;

#[cfg(target_os = "macos")]
fn set_error(out: *mut *mut c_char, msg: &str) {
    if !out.is_null() {
        if let Ok(c) = CString::new(msg) {
            unsafe { *out = c.into_raw() };
        }
    }
}

/// Creates a player for clip `clip_index` of `handle`, drawing into `ns_view` (macOS `NSView*`).
/// Returns `NULL` on failure (message in `out_error`, free with `cinefs_free_string`).
/// The mount `handle` must outlive the player.
#[cfg(target_os = "macos")]
#[no_mangle]
pub unsafe extern "C" fn cinefs_player_create(
    handle: *const CineFsMountHandle,
    clip_index: usize,
    ns_view: *mut c_void,
    width: u32,
    height: u32,
    out_error: *mut *mut c_char,
) -> *mut Player {
    use raw_window_handle::{AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle};
    if !out_error.is_null() {
        *out_error = ptr::null_mut();
    }
    if handle.is_null() || ns_view.is_null() {
        set_error(out_error, "null argument");
        return ptr::null_mut();
    }
    let mount = &*handle;
    let Some(clip) = mount.vfs.clips.get(clip_index).cloned() else {
        set_error(out_error, "clip index out of range");
        return ptr::null_mut();
    };
    let target = wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(RawDisplayHandle::AppKit(AppKitDisplayHandle::new())),
        raw_window_handle: RawWindowHandle::AppKit(AppKitWindowHandle::new(ptr::NonNull::new_unchecked(ns_view))),
    };
    match Player::new(clip, target, width, height) {
        Ok(p) => Box::into_raw(Box::new(p)),
        Err(e) => {
            crate::log_warn(&format!("player create failed: {e}"));
            set_error(out_error, &e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_destroy(p: *mut Player) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_resize(p: *mut Player, width: u32, height: u32) {
    if let Some(p) = p.as_mut() {
        p.resize(width, height);
    }
}

/// Call at display rate from the main thread. Returns 1 if a frame was drawn.
#[no_mangle]
pub unsafe extern "C" fn cinefs_player_tick(p: *mut Player) -> u32 {
    p.as_mut().map_or(0, |p| p.tick() as u32)
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_play(p: *mut Player) {
    if let Some(p) = p.as_ref() {
        p.engine.play();
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_pause(p: *mut Player) {
    if let Some(p) = p.as_ref() {
        p.engine.pause();
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_toggle(p: *mut Player) {
    if let Some(p) = p.as_ref() {
        p.engine.toggle();
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_seek(p: *mut Player, frame: f64) {
    if let Some(p) = p.as_ref() {
        p.engine.seek(frame);
    }
}

#[no_mangle]
pub unsafe extern "C" fn cinefs_player_set_loop(p: *mut Player, on: u32) {
    if let Some(p) = p.as_ref() {
        p.engine.set_looping(on != 0);
    }
}

/// 1 if the clip has sound and an output device is open (the picture then follows the audio clock).
#[no_mangle]
pub unsafe extern "C" fn cinefs_player_has_audio(p: *const Player) -> u32 {
    p.as_ref().map_or(0, |p| p.has_audio() as u32)
}

/// Output volume, 0.0 (muted) .. 1.0.
#[no_mangle]
pub unsafe extern "C" fn cinefs_player_set_volume(p: *mut Player, volume: f32) {
    if let Some(p) = p.as_ref() {
        p.set_volume(volume);
    }
}

/// Reads the transport state. Any output pointer may be `NULL`.
#[no_mangle]
pub unsafe extern "C" fn cinefs_player_state(
    p: *const Player,
    out_position: *mut f64,
    out_frame_count: *mut u64,
    out_fps: *mut f64,
    out_playing: *mut u32,
    out_dropped: *mut u64,
) {
    let Some(p) = p.as_ref() else { return };
    if !out_position.is_null() {
        *out_position = p.engine.position();
    }
    if !out_frame_count.is_null() {
        *out_frame_count = p.engine.frame_count() as u64;
    }
    if !out_fps.is_null() {
        *out_fps = p.engine.fps();
    }
    if !out_playing.is_null() {
        *out_playing = p.engine.is_playing() as u32;
    }
    if !out_dropped.is_null() {
        *out_dropped = p.stats().dropped;
    }
}

#[allow(dead_code)]
fn _keep(_: *mut c_void, _: *const CineFsMountHandle) {}
