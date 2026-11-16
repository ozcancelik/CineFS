//! Tiny dependency-free file logger shared by every CineFS front-end.
//!
//! The Windows GUI is built with `windows_subsystem = "windows"` (no console), so when something
//! goes wrong the only way to find out *why* is a log file. Everything important (startup
//! environment, WinFsp loading, mount steps, drive visibility checks, Explorer / DaVinci Resolve
//! calls, panics) is written to:
//!
//! * Windows: `%LOCALAPPDATA%\CineFS\logs\cinefs.log`
//! * macOS:   `~/Library/Logs/CineFS/cinefs.log`
//! * other:   `$HOME/.cinefs/cinefs.log` (or the temp dir)
//!
//! Logging is a no-op until [`init`] is called, and never panics or blocks on I/O errors.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

struct LogState {
    file: File,
    path: PathBuf,
}

static LOGGER: Mutex<Option<LogState>> = Mutex::new(None);

/// Directory that holds `cinefs.log`.
pub fn log_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d).join("CineFS").join("logs");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join("Library").join("Logs").join("CineFS");
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(".cinefs");
        }
    }
    std::env::temp_dir().join("CineFS")
}

/// Full path of the active log file (even before [`init`]).
pub fn log_file_path() -> PathBuf {
    log_dir().join("cinefs.log")
}

/// Opens (and rotates, if > 4 MiB) the log file. Safe to call more than once.
pub fn init(app_name: &str) -> Option<PathBuf> {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("cinefs.log");

    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_BYTES {
            let _ = fs::rename(&path, dir.join("cinefs.log.old"));
        }
    }

    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    if let Ok(mut g) = LOGGER.lock() {
        *g = Some(LogState {
            file,
            path: path.clone(),
        });
    }
    write(
        "INFO",
        &format!(
            "================ {app_name} {} started (pid {}) ================",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
    Some(path)
}

/// Path of the log file if logging is active.
pub fn active_path() -> Option<PathBuf> {
    LOGGER
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.path.clone()))
}

/// Writes one line. No-op if [`init`] has not succeeded.
pub fn write(level: &str, msg: &str) {
    let Ok(mut guard) = LOGGER.lock() else {
        return;
    };
    let Some(state) = guard.as_mut() else {
        return;
    };
    let line = format!(
        "{} [{:<5}] [{:?}] {}\r\n",
        timestamp(),
        level,
        std::thread::current().id(),
        msg
    );
    let _ = state.file.write_all(line.as_bytes());
    let _ = state.file.flush();
}

/// Logs panics (message, location and backtrace) to the log file before the default hook runs.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        write("PANIC", &format!("{info}\r\n{bt}"));
        prev(info);
    }));
}

fn timestamp() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, day) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{day:02} {:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days -> (year, month, day) algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::write("INFO", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::write("WARN", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::write("ERROR", &format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_397), (2025, 11, 5));
    }
}
