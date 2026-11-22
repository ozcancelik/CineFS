//! C ABI (`extern "C"`) bridge for linking `cinefs-core` directly into the macOS SwiftUI app
//! (and future native shells).

use crate::nfs::MountedVolume;
use crate::vfs::CineFs;
use crate::DngPayloadMode;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

/// Opaque handle owning an active virtual filesystem and its mounted volume.
pub struct CineFsMountHandle {
    pub vfs: Arc<CineFs>,
    pub volume: MountedVolume,
    pub summary_json: CString,
}

fn mode_from_u32(mode: u32) -> DngPayloadMode {
    match mode {
        1 => DngPayloadMode::Lj92Passthrough,
        2 => DngPayloadMode::NativePacked,
        3 => DngPayloadMode::Unpacked16,
        _ => DngPayloadMode::Auto,
    }
}

fn write_error(out_error: *mut *mut c_char, msg: &str) {
    if !out_error.is_null() {
        if let Ok(cstr) = CString::new(msg) {
            unsafe {
                *out_error = cstr.into_raw();
            }
        }
    }
}

/// Mounts one or MANY sources into a single volume at `mount_point`.
///
/// `source_paths` is a newline-separated list of `.MLV` / `.mcraw` / `.braw` files and/or folders (a single
/// path works too). Returns an opaque handle, or `NULL` on failure (populating `out_error`).
#[no_mangle]
pub unsafe extern "C" fn cinefs_mount(
    source_paths: *const c_char,
    mount_point: *const c_char,
    mode: u32,
    cache_mb: usize,
    out_error: *mut *mut c_char,
) -> *mut CineFsMountHandle {
    cinefs_mount_with_progress(source_paths, mount_point, mode, cache_mb, out_error, None, ptr::null_mut())
}

/// Progress callback for [`cinefs_mount_with_progress`]: `(ctx, event, index, total, text)`.
///
/// Events: `0` = clip listed (`text` = file path; sent for every clip, in mount order, before
/// indexing starts), `1` = clip is being indexed, `2` = clip ready, `3` = clip failed (`text` =
/// reason; the other clips are still mounted). `text` is only valid during the call. Called on
/// the mounting thread, never concurrently.
pub type CineFsMountProgressFn =
    unsafe extern "C" fn(ctx: *mut std::ffi::c_void, event: u32, index: usize, total: usize, text: *const c_char);

/// Same as [`cinefs_mount`], reporting per-clip progress through `progress` (may be `NULL`).
#[no_mangle]
pub unsafe extern "C" fn cinefs_mount_with_progress(
    source_paths: *const c_char,
    mount_point: *const c_char,
    mode: u32,
    cache_mb: usize,
    out_error: *mut *mut c_char,
    progress: Option<CineFsMountProgressFn>,
    ctx: *mut std::ffi::c_void,
) -> *mut CineFsMountHandle {
    if !out_error.is_null() {
        *out_error = ptr::null_mut();
    }
    if source_paths.is_null() || mount_point.is_null() {
        write_error(out_error, "Null path argument");
        return ptr::null_mut();
    }

    let Ok(src_str) = CStr::from_ptr(source_paths).to_str() else {
        write_error(out_error, "Invalid UTF-8 in source_paths");
        return ptr::null_mut();
    };
    let Ok(mp_str) = CStr::from_ptr(mount_point).to_str() else {
        write_error(out_error, "Invalid UTF-8 in mount_point");
        return ptr::null_mut();
    };
    let sources: Vec<PathBuf> = src_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    crate::log::write(
        "INFO",
        &format!("cinefs_mount: {} source(s) -> '{mp_str}' (mode {mode})", sources.len()),
    );

    let t0 = Instant::now();
    let cache_bytes = cache_mb.max(64) * 1024 * 1024;
    let mut vfs = CineFs::new(cache_bytes);
    let files = crate::summary::expand_sources(&sources);
    let total = files.len();
    let report = |event: u32, index: usize, text: &str| {
        if let Some(cb) = progress {
            let c = CString::new(text.replace('\0', "")).unwrap_or_default();
            cb(ctx, event, index, total, c.as_ptr());
        }
    };
    for (i, f) in files.iter().enumerate() {
        report(0, i, &f.to_string_lossy());
    }
    let result = crate::summary::mount_files_with_progress(&mut vfs, &files, mode_from_u32(mode), |i, state| {
        match state {
            crate::summary::ClipLoad::Loading => report(1, i, ""),
            crate::summary::ClipLoad::Ready => report(2, i, ""),
            crate::summary::ClipLoad::Failed(msg) => report(3, i, &msg),
        }
    });
    if let Err(e) = result {
        crate::log::write("ERROR", &format!("mount failed: {e}"));
        write_error(out_error, &e.to_string());
        return ptr::null_mut();
    }
    let index_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let vfs_arc = Arc::new(vfs);
    let volume = match MountedVolume::mount(Arc::clone(&vfs_arc), Path::new(mp_str)) {
        Ok(v) => v,
        Err(e) => {
            crate::log::write("ERROR", &format!("volume mount failed: {e}"));
            write_error(out_error, &e.to_string());
            return ptr::null_mut();
        }
    };

    let summary = crate::summary::MountSummary::build(
        &vfs_arc,
        sources.iter().map(|p| p.to_string_lossy().to_string()).collect(),
        volume.mount_point.to_string_lossy().to_string(),
        volume.port(),
        index_ms,
    );
    crate::log::write(
        "INFO",
        &format!(
            "mounted {} clip(s) at '{}' in {:.1} ms",
            summary.clips.len(),
            summary.mount_point,
            index_ms
        ),
    );

    let summary_json = CString::new(summary.to_json()).unwrap_or_default();
    Box::into_raw(Box::new(CineFsMountHandle {
        vfs: vfs_arc,
        volume,
        summary_json,
    }))
}

/// Starts file logging (`~/Library/Logs/CineFS/cinefs.log` on macOS). Safe to call repeatedly.
#[no_mangle]
pub unsafe extern "C" fn cinefs_log_init(app_name: *const c_char) {
    let name = if app_name.is_null() {
        "CineFS".to_string()
    } else {
        CStr::from_ptr(app_name).to_string_lossy().to_string()
    };
    crate::log::init(&name);
    crate::log::install_panic_hook();
}

/// Appends one line to the log (`level` e.g. "INFO", "WARN", "ERROR").
#[no_mangle]
pub unsafe extern "C" fn cinefs_log(level: *const c_char, message: *const c_char) {
    if message.is_null() {
        return;
    }
    let lvl = if level.is_null() {
        "INFO".to_string()
    } else {
        CStr::from_ptr(level).to_string_lossy().to_string()
    };
    crate::log::write(&lvl, &CStr::from_ptr(message).to_string_lossy());
}

/// Absolute path of the log file as a newly allocated C string (free with `cinefs_free_string`).
#[no_mangle]
pub extern "C" fn cinefs_log_path() -> *mut c_char {
    let p = crate::log::active_path().unwrap_or_else(crate::log::log_file_path);
    CString::new(p.to_string_lossy().to_string())
        .unwrap_or_default()
        .into_raw()
}

/// Forces the vertical pixel-stretch factor for clips opened afterwards (`0` = automatic from RAWC).
#[no_mangle]
pub extern "C" fn cinefs_set_stretch_override(factor: f32) {
    crate::mlv::set_stretch_override(if factor > 0.0 { Some(factor) } else { None });
}

/// Chooses how anamorphic (crop_rec) stretch is applied to clips opened afterwards:
/// `1` = bake into the DNG pixels (universal), `0` = DefaultScale tag only.
#[no_mangle]
pub extern "C" fn cinefs_set_stretch_bake(on: u32) {
    crate::dng::set_stretch_bake(on != 0);
}

/// Sets focus-pixel correction mode for clips opened afterwards:
/// `0` = Off, `1` = Auto (bundled map first, clip-learned fallback; default), `2` = Map only.
#[no_mangle]
pub extern "C" fn cinefs_set_focus_fix(mode: u32) {
    crate::focus_fix::set_focus_fix_mode(crate::focus_fix::FocusFixMode::from_u8(mode as u8));
}

/// Sets Dual ISO recovery mode for clips opened afterwards:
/// `0` = Off, `1` = Auto (per-clip detection and recovery; default).
#[no_mangle]
pub extern "C" fn cinefs_set_dual_iso(mode: u32) {
    crate::dual_iso::set_dual_iso_mode(crate::dual_iso::DualIsoMode::from_u8(mode as u8));
}

/// Sets static hot/dead pixel repair mode for clips opened afterwards:
/// `0` = Off, `1` = Auto (per-clip static detection and repair; default).
#[no_mangle]
pub extern "C" fn cinefs_set_bad_pixels(mode: u32) {
    crate::bad_pixels::set_bad_pixel_mode(crate::bad_pixels::BadPixelMode::from_u8(mode as u8));
}

/// Imports clip folders (newline-separated absolute paths) into DaVinci Resolve's Media Pool via
/// `fuscript`. Blocks up to `timeout_secs`; call from a background thread.
/// Returns a JSON string `{"status":"imported|imported_nothing|no_project|no_resolve|no_fuscript|failed","count":N,"message":""}`
/// (free with `cinefs_free_string`).
#[no_mangle]
pub unsafe extern "C" fn cinefs_resolve_import(
    fuscript_path: *const c_char,
    clip_paths: *const c_char,
    par_spec: *const c_char,
    timeout_secs: u32,
) -> *mut c_char {
    let outcome = if fuscript_path.is_null() || clip_paths.is_null() {
        crate::resolve::ResolveOutcome::Failed(String::from("Null argument"))
    } else {
        let fuscript = PathBuf::from(CStr::from_ptr(fuscript_path).to_string_lossy().to_string());
        let clips: Vec<PathBuf> = CStr::from_ptr(clip_paths)
            .to_string_lossy()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        // `par_spec`: lines "folder<TAB>pixel_aspect_ratio" for clips needing a PAR fix (may be NULL)
        let pars: Vec<crate::resolve::ClipPar> = if par_spec.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr(par_spec)
                .to_string_lossy()
                .lines()
                .filter_map(|l| {
                    let (f, p) = l.split_once('\t')?;
                    Some(crate::resolve::ClipPar { folder: f.to_string(), par: p.trim().parse().ok()? })
                })
                .collect()
        };
        crate::resolve::run_import_with_par(
            &fuscript,
            &clips,
            &pars,
            std::time::Duration::from_secs(timeout_secs.max(5) as u64),
        )
    };
    crate::log::write("INFO", &format!("Resolve import outcome: {outcome:?}"));
    CString::new(outcome.to_json()).unwrap_or_default().into_raw()
}

/// Returns a borrowed C string pointer containing the JSON summary for `handle`.
#[no_mangle]
pub unsafe extern "C" fn cinefs_mount_summary_json(
    handle: *const CineFsMountHandle,
) -> *const c_char {
    if handle.is_null() {
        return ptr::null();
    }
    let h = &*handle;
    h.summary_json.as_ptr()
}

/// Queries live cache statistics from an active mount handle.
#[no_mangle]
pub unsafe extern "C" fn cinefs_mount_cache_stats(
    handle: *const CineFsMountHandle,
    out_hits: *mut u64,
    out_misses: *mut u64,
    out_prefetched: *mut u64,
    out_bytes: *mut usize,
) {
    if handle.is_null() {
        return;
    }
    let h = &*handle;
    let (hits, misses, prefetched, bytes) = h.vfs.cache.stats();
    if !out_hits.is_null() {
        *out_hits = hits;
    }
    if !out_misses.is_null() {
        *out_misses = misses;
    }
    if !out_prefetched.is_null() {
        *out_prefetched = prefetched;
    }
    if !out_bytes.is_null() {
        *out_bytes = bytes;
    }
}

/// Unmounts the virtual filesystem and frees `handle`.
#[no_mangle]
pub unsafe extern "C" fn cinefs_unmount(handle: *mut CineFsMountHandle) {
    if !handle.is_null() {
        let mut boxed = Box::from_raw(handle);
        let _ = boxed.volume.unmount();
    }
}

/// Frees a heap-allocated C string returned by `cinefs_mount` (`out_error`).
#[no_mangle]
pub unsafe extern "C" fn cinefs_free_string(s: *mut c_char) {
    if !s.is_null() {
        let _ = CString::from_raw(s);
    }
}

/// Renders (and caches) a preview of clip `clip_index` of `handle`, at most `max_width` pixels wide.
/// Returns a malloc'd straight-RGBA buffer of `*out_w * *out_h * 4` bytes (free with
/// `cinefs_free_buffer`), or `NULL` on failure. Blocks for ~50-150 ms the first time: call it from a
/// background thread.
#[no_mangle]
pub unsafe extern "C" fn cinefs_clip_thumbnail(
    handle: *const CineFsMountHandle,
    clip_index: usize,
    max_width: u32,
    out_w: *mut u32,
    out_h: *mut u32,
) -> *mut u8 {
    if handle.is_null() || out_w.is_null() || out_h.is_null() {
        return ptr::null_mut();
    }
    let h = &*handle;
    let Some(clip) = h.vfs.clips.get(clip_index) else {
        return ptr::null_mut();
    };
    let Some(t) = clip.thumbnail(max_width as usize) else {
        return ptr::null_mut();
    };
    *out_w = t.width as u32;
    *out_h = t.height as u32;
    let boxed: Box<[u8]> = t.rgba.clone().into_boxed_slice();
    Box::into_raw(boxed) as *mut u8
}

/// Frees a buffer returned by `cinefs_clip_thumbnail` (`len` = `w * h * 4`).
#[no_mangle]
pub unsafe extern "C" fn cinefs_free_buffer(p: *mut u8, len: usize) {
    if !p.is_null() {
        let _ = Box::from_raw(std::slice::from_raw_parts_mut(p, len));
    }
}

/// Renders frame `frame_index` of clip `clip_index` (not cached; for hover scrubbing). Same
/// contract as `cinefs_clip_thumbnail`.
#[no_mangle]
pub unsafe extern "C" fn cinefs_clip_frame_preview(
    handle: *const CineFsMountHandle,
    clip_index: usize,
    frame_index: usize,
    max_width: u32,
    out_w: *mut u32,
    out_h: *mut u32,
) -> *mut u8 {
    if handle.is_null() || out_w.is_null() || out_h.is_null() {
        return ptr::null_mut();
    }
    let h = &*handle;
    let Some(clip) = h.vfs.clips.get(clip_index) else {
        return ptr::null_mut();
    };
    match crate::thumbnail::render_frame(&clip.clip, frame_index, max_width as usize) {
        Ok(t) => {
            *out_w = t.width as u32;
            *out_h = t.height as u32;
            Box::into_raw(t.rgba.into_boxed_slice()) as *mut u8
        }
        Err(_) => ptr::null_mut(),
    }
}
