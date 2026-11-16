//! `cinefs-core` — High-performance raw video container reader and virtual CinemaDNG engine.
//!
//! Designed so that metadata/header operations never touch pixel payloads, and
//! pixel payloads use zero-decode LJ92 passthrough or native bit-depth packing
//! whenever possible.

pub mod braw;
pub mod cache;
pub mod color_matrices;
pub mod dng;
pub mod ffi;
pub mod focus_fix;
pub mod focus_probe;
pub mod levels;
pub mod lj92;
pub mod log;
pub mod mcraw;
pub mod mlv;
pub mod nfs;
pub mod resample;
pub mod resolve;
pub mod summary;
pub mod thumbnail;
pub mod unpack;
pub mod vfs;
pub mod wav;
pub mod winfsp;

use std::fmt;
use std::io;

/// Result alias for `cinefs-core` operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by `cinefs-core`.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    InvalidFormat(String),
    CorruptData(String),
    NotFound(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::InvalidFormat(msg) => write!(f, "Invalid format: {msg}"),
            Error::CorruptData(msg) => write!(f, "Corrupt data: {msg}"),
            Error::NotFound(msg) => write!(f, "Not found: {msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// DNG output payload representation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DngPayloadMode {
    /// Automatically pick the fastest representation:
    /// - `Lj92Passthrough` if the source is a compatible 2-component CFA Lossless JPEG
    /// - `NativePacked` if the source is uncompressed 8/10/12/14/16-bit
    Auto,
    /// Pass the source LJ92 bitstream directly as DNG `Compression = 7` without pixel decoding.
    /// Falls back to `NativePacked` if the source is uncompressed.
    Lj92Passthrough,
    /// Uncompressed DNG (`Compression = 1`) at the source's native bit depth (8, 10, 12, 14, or 16-bit).
    NativePacked,
    /// Uncompressed 16-bit DNG (`Compression = 1`, `BitsPerSample = 16`), unpacking any packed or LJ92 frames.
    Unpacked16,
}

/// Helper for thread-safe positioned reads (`pread` on Unix, `seek_read` on Windows).
pub(crate) fn read_exact_at(file: &std::fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = read_at_once(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("unexpected EOF at offset {offset}"),
            ));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at_once(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at_once(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}
