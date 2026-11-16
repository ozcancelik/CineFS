//! Windows shell / process helpers that have nothing to do with drawing: elevation handling,
//! launching Explorer, mount-visibility checks, hand-over of paths between instances, diagnostics.
//! (Everything here is UI-free so any front-end — GDI today, GPUI later — can reuse it.)

use cinefs_core::winfsp::is_winfsp_available;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::Mutex;
use std::time::Instant;

const SW_SHOWNORMAL: i32 = 1;

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(dwProcessId: u32, pSessionId: *mut u32) -> i32;
}

#[link(name = "shell32", kind = "raw-dylib")]
extern "system" {
    fn IsUserAnAdmin() -> i32;
    fn ShellExecuteW(
        hwnd: *mut c_void,
        lpOperation: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShowCmd: i32,
    ) -> isize;
}

fn wstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

pub fn locate_resolve_fuscript() -> Option<PathBuf> {
    let candidates = [
        r"C:\Program Files\Blackmagic Design\DaVinci Resolve\fuscript.exe",
        r"C:\Program Files (x86)\Blackmagic Design\DaVinci Resolve\fuscript.exe",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

pub fn current_session_id() -> u32 {
    let mut sid = 0u32;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut sid);
    }
    sid
}

pub fn is_setup_exe() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()))
        .map(|stem| stem.contains("setup") || stem.contains("install"))
        .unwrap_or(false)
        || std::env::args().any(|a| a == "--setup" || a == "--install")
}

/// Starts `exe` through the (un-elevated) Explorer shell so the new process does NOT inherit
/// this process' administrator token. Drive letters created by WinFsp belong to the logon
/// session of the process that mounted them, and an elevated token has a different logon
/// session than the normal Explorer — so an elevated CineFS creates a `Z:` that Explorer and
/// DaVinci Resolve can't see.
pub fn launch_unelevated(exe: &Path) -> bool {
    match Command::new("explorer.exe").arg(exe).spawn() {
        Ok(_) => {
            cinefs_core::log_info!("Launched '{}' via explorer.exe (un-elevated)", exe.display());
            true
        }
        Err(e) => {
            cinefs_core::log_error!("explorer.exe launch of '{}' failed: {e}", exe.display());
            false
        }
    }
}

pub fn data_dir() -> PathBuf {
    let d = cinefs_core::log::log_dir();
    d.parent().map(|p| p.to_path_buf()).unwrap_or(d)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// If running elevated (and not the installer), relaunches CineFS un-elevated through Explorer
/// and returns `true` so the caller exits. Paths passed on the command line are handed over in
/// `pending_mounts.txt` because `explorer.exe` can't forward arguments.
pub fn maybe_relaunch_unelevated(args: &[String]) -> bool {
    if !is_elevated() {
        return false;
    }
    let dir = data_dir();
    let _ = fs::create_dir_all(&dir);
    let stamp = dir.join("deelevate.stamp");
    if let Some(prev) = fs::read_to_string(&stamp)
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
    {
        if unix_now().saturating_sub(prev) < 20 {
            cinefs_core::log_warn!(
                "Still elevated after an un-elevate attempt (UAC disabled / built-in Administrator?). \
                 Continuing elevated: the mounted drive may not be visible to a normal Explorer window."
            );
            return false;
        }
    }
    let _ = fs::write(&stamp, unix_now().to_string());

    let pending: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
    if !pending.is_empty() {
        let body = pending.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n");
        let _ = fs::write(dir.join("pending_mounts.txt"), body);
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    cinefs_core::log_warn!(
        "CineFS was started ELEVATED (administrator). Relaunching without elevation so the virtual drive is visible to Explorer / DaVinci Resolve."
    );
    launch_unelevated(&exe)
}

pub fn take_pending_mounts() -> Vec<PathBuf> {
    let f = data_dir().join("pending_mounts.txt");
    let fresh = fs::metadata(&f)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e.as_secs() < 120)
        .unwrap_or(false);
    let body = fs::read_to_string(&f).unwrap_or_default();
    let _ = fs::remove_file(&f);
    if !fresh {
        return Vec::new();
    }
    body.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn log_startup_diagnostics() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let args: Vec<String> = std::env::args().collect();
    let mask = unsafe { GetLogicalDrives() };
    let used: String = (b'A'..=b'Z')
        .filter(|l| mask & (1u32 << (l - b'A')) != 0)
        .map(|l| format!("{}:", l as char))
        .collect::<Vec<_>>()
        .join(" ");
    cinefs_core::log_info!("exe = {exe}");
    cinefs_core::log_info!("args = {args:?}");
    cinefs_core::log_info!(
        "elevated = {}, session id = {}, setup mode = {}",
        is_elevated(),
        current_session_id(),
        is_setup_exe()
    );
    cinefs_core::log_info!("drive letters in use: {used}");
    cinefs_core::log_info!("WinFsp available = {}", is_winfsp_available());
    cinefs_core::log_info!("DaVinci Resolve fuscript = {:?}", locate_resolve_fuscript());
    cinefs_core::log_info!(
        "USERNAME = {:?}, PROCESSOR_ARCHITECTURE = {:?}",
        std::env::var("USERNAME").ok(),
        std::env::var("PROCESSOR_ARCHITECTURE").ok()
    );
}

/// Opens a path in Windows Explorer. Uses `ShellExecuteW`; if that reports failure
/// (return value <= 32) falls back to `explorer.exe <path>`. Every attempt is logged.
pub unsafe fn open_path_in_explorer(path: &Path, source: &str) {
    static LAST_OPEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    let key = path.to_string_lossy().trim_end_matches('\\').to_ascii_lowercase();
    if let Ok(mut g) = LAST_OPEN.lock() {
        if let Some((k, t)) = g.as_ref() {
            if *k == key && t.elapsed().as_millis() < 2000 {
                cinefs_core::log_info!("Open in Explorer '{}' [{source}] ignored: duplicate within 2 s", path.display());
                return;
            }
        }
        *g = Some((key, Instant::now()));
    }
    cinefs_core::log_info!("Open in Explorer requested [{source}]: '{}'", path.display());
    let s = path.to_string_lossy();
    let normalized = if s.len() == 2 && s.as_bytes()[1] == b':' {
        format!("{s}\\")
    } else {
        s.to_string()
    };
    let visible = Path::new(&normalized).exists();
    // explorer.exe is the most reliable way: ShellExecuteW "open" returns success (>32) even
    // when no window appears if the calling thread has no COM apartment.
    match Command::new("explorer.exe").arg(&normalized).spawn() {
        Ok(_) => cinefs_core::log_info!(
            "Open in Explorer '{normalized}': visible_to_this_process={visible}, explorer.exe started"
        ),
        Err(e) => {
            let op = wstr("open");
            let file = wstr(&normalized);
            let rc = ShellExecuteW(
                ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            );
            cinefs_core::log_warn!(
                "explorer.exe spawn failed ({e}); ShellExecuteW('{normalized}') -> {rc}"
            );
        }
    }
}

/// Waits (up to `timeout_ms`) for the freshly mounted volume root to be enumerable by this
/// process, logging what is found. Returns the number of root entries or an error string.
pub fn verify_mount_visible(root: &Path, timeout_ms: u64) -> std::result::Result<usize, String> {
    let t0 = Instant::now();
    let mut last_err;
    loop {
        match fs::read_dir(root) {
            Ok(rd) => {
                let names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                cinefs_core::log_info!(
                    "Mount verify OK: '{}' lists {} entries after {} ms: {:?}",
                    root.display(),
                    names.len(),
                    t0.elapsed().as_millis(),
                    names
                );
                return Ok(names.len());
            }
            Err(e) => last_err = e.to_string(),
        }
        if t0.elapsed().as_millis() as u64 >= timeout_ms {
            cinefs_core::log_error!(
                "Mount verify FAILED: '{}' not readable after {} ms: {last_err}",
                root.display(),
                timeout_ms
            );
            return Err(last_err);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}
