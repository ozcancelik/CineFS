//! Zero-dependency WinFsp virtual filesystem adapter for `CineFs` on Windows.
//!
//! Dynamically loads `winfsp-x64.dll` / `winfsp-a64.dll` at runtime (via `LoadLibraryW`
//! and the `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` registry key) so that:
//! 1. `cinefs-core` requires zero external Rust crates or C/C++ SDK libraries to compile.
//! 2. `CineFS.exe` starts cleanly even on systems where WinFsp is not yet installed.
//! 3. All `FSP_FILE_SYSTEM_INTERFACE` callbacks (`Open`, `Create`, `Overwrite`, `Read`,
//!    `Write`, `ReadDirectory`, `GetDirInfoByName`, `GetFileInfo`, `SetBasicInfo`,
//!    `SetFileSize`, `CanDelete`, `SetDelete`, `Rename`, `GetSecurityByName`, `GetSecurity`,
//!    `Cleanup`, `Close`) operate in `O(1)` using `CineFs` 64-bit `node_id` handles,
//!    with `UmFileContextIsUserContext2 = 1` per-handle isolation and panic guards.

use crate::vfs::{CineFs, EntryKind, VfsNodeAttr, NODE_METADATA_NEVER_INDEX, NODE_ROOT};
use crate::{Error, Result};
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

// NTSTATUS constants
pub const STATUS_SUCCESS: i32 = 0x0000_0000;
pub const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;
pub const STATUS_NO_SUCH_FILE: i32 = 0xC000_000Fu32 as i32;
pub const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC000_0010u32 as i32;
pub const STATUS_END_OF_FILE: i32 = 0xC000_0011u32 as i32;
pub const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023u32 as i32;
pub const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
pub const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
pub const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
pub const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2u32 as i32;
pub const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
pub const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;

// Windows FILE_ATTRIBUTE_* flags
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

// WinFsp CreateOptions flags
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

// WinFsp Cleanup flags
pub const FSP_CLEANUP_DELETE: u32 = 0x01;
pub const FSP_CLEANUP_SET_ALLOCATION_SIZE: u32 = 0x02;
pub const FSP_CLEANUP_SET_ARCHIVE_BIT: u32 = 0x10;
pub const FSP_CLEANUP_SET_LAST_ACCESS_TIME: u32 = 0x20;
pub const FSP_CLEANUP_SET_LAST_WRITE_TIME: u32 = 0x40;
pub const FSP_CLEANUP_SET_CHANGE_TIME: u32 = 0x80;

// Difference between Windows epoch (1601-01-01) and Unix epoch (1970-01-01) in seconds
const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
const HNSECS_PER_SEC: u64 = 10_000_000;

// Exact `FSP_FSCTL_DIR_INFO` header size (`sizeof(FSP_FSCTL_DIR_INFO) == 104` in `inc/winfsp/fsctl.h`):
// - offset 0..2:   UINT16 Size
// - offset 2..8:   6 bytes alignment padding (because FSP_FSCTL_FILE_INFO is 8-byte aligned)
// - offset 8..80:  FSP_FSCTL_FILE_INFO FileInfo (72 bytes)
// - offset 80..104: UINT8 Padding[24] (union with UINT64 NextOffset)
// - offset 104..:  WCHAR FileNameBuf[]
pub const FSP_DIR_INFO_HDR_SIZE: usize = 104;

// FSP_FSCTL_VOLUME_PARAMS bitfield constants (`inc/winfsp/fsctl.h`)
pub const VOL_CASE_SENSITIVE_SEARCH: u32 = 1 << 0;
pub const VOL_CASE_PRESERVED_NAMES: u32 = 1 << 1;
pub const VOL_UNICODE_ON_DISK: u32 = 1 << 2;
pub const VOL_PERSISTENT_ACLS: u32 = 1 << 3;
pub const VOL_POST_CLEANUP_WHEN_MODIFIED_ONLY: u32 = 1 << 10;
pub const VOL_PASS_QUERY_DIRECTORY_PATTERN: u32 = 1 << 11;
pub const VOL_PASS_QUERY_DIRECTORY_FILE_NAME: u32 = 1 << 13;
pub const VOL_FLUSH_AND_PURGE_ON_CLEANUP: u32 = 1 << 14;
/// Critical: each `Open`/`Create` returns an independent per-handle `FileDesc` (`UserContext2`)
/// so closing one handle in Explorer/Resolve never frees another open handle's `WinFspOpenHandle`.
pub const VOL_UM_FILE_CONTEXT_IS_USER_CONTEXT2: u32 = 1 << 16;
pub const VOL_ALLOW_OPEN_IN_KERNEL_MODE: u32 = 1 << 24;
pub const VOL_SUPPORTS_POSIX_UNLINK_RENAME: u32 = 1 << 29;
pub const VOL_POST_DISPOSITION_WHEN_NECESSARY_ONLY: u32 = 1 << 30;

/// WinFsp `FSP_FSCTL_VOLUME_PARAMS` structure (`inc/winfsp/fsctl.h`, `sizeof == 504` bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FspFsctlVolumeParams {
    pub version: u16,
    pub sector_size: u16,
    pub sectors_per_allocation_unit: u16,
    pub max_component_length: u16,
    pub volume_creation_time: u64,
    pub volume_serial_number: u32,
    pub transact_timeout: u32,
    pub irp_timeout: u32,
    pub irp_capacity: u32,
    pub file_info_timeout: u32,
    pub flags: u32,
    pub prefix: [u16; 192],
    pub file_system_name: [u16; 16],
    pub additional_flags: u32,
    pub volume_info_timeout: u32,
    pub dir_info_timeout: u32,
    pub security_timeout: u32,
    pub stream_info_timeout: u32,
    pub ea_timeout: u32,
    pub fsext_control_code: u32,
    pub reserved32: [u32; 1],
    pub reserved64: [u64; 2],
}

impl Default for FspFsctlVolumeParams {
    fn default() -> Self {
        let mut fs_name = [0u16; 16];
        for (i, ch) in "CineFS".encode_utf16().enumerate() {
            fs_name[i] = ch;
        }
        Self {
            version: std::mem::size_of::<Self>() as u16, // 504
            sector_size: 4096,
            sectors_per_allocation_unit: 1,
            max_component_length: 255,
            volume_creation_time: unix_secs_to_filetime(1_700_000_000),
            volume_serial_number: 0x4349_4E45,
            transact_timeout: 0,
            irp_timeout: 0,
            irp_capacity: 0,
            file_info_timeout: 1000,
            flags: VOL_CASE_PRESERVED_NAMES
                | VOL_UNICODE_ON_DISK
                | VOL_PERSISTENT_ACLS
                | VOL_POST_CLEANUP_WHEN_MODIFIED_ONLY
                | VOL_PASS_QUERY_DIRECTORY_FILE_NAME
                | VOL_FLUSH_AND_PURGE_ON_CLEANUP
                | VOL_UM_FILE_CONTEXT_IS_USER_CONTEXT2,
            // NOTE: VOL_ALLOW_OPEN_IN_KERNEL_MODE / VOL_SUPPORTS_POSIX_UNLINK_RENAME /
            // VOL_POST_DISPOSITION_WHEN_NECESSARY_ONLY are intentionally NOT set: their bit
            // positions were never verified against fsctl.h and a wrong bit silently changes
            // kernel behaviour. They are pure optimisations, so leaving them off is safe.
            prefix: [0u16; 192],
            file_system_name: fs_name,
            additional_flags: 0,
            volume_info_timeout: 0,
            dir_info_timeout: 0,
            security_timeout: 0,
            stream_info_timeout: 0,
            ea_timeout: 0,
            fsext_control_code: 0,
            reserved32: [0u32; 1],
            reserved64: [0u64; 2],
        }
    }
}

/// WinFsp `FSP_FSCTL_VOLUME_INFO` structure (`inc/winfsp/fsctl.h`, `sizeof == 88` bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FspFsctlVolumeInfo {
    pub total_size: u64,
    pub free_size: u64,
    pub volume_label_length: u16,
    pub volume_label: [u16; 32],
}

/// WinFsp `FSP_FSCTL_FILE_INFO` structure (`inc/winfsp/fsctl.h`, `sizeof == 72` bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FspFsctlFileInfo {
    pub file_attributes: u32,
    pub reparse_tag: u32,
    pub allocation_size: u64,
    pub file_size: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
    pub last_write_time: u64,
    pub change_time: u64,
    pub index_number: u64,
    pub hard_links: u32,
    pub ea_size: u32,
}

/// WinFsp `FSP_FILE_SYSTEM` prefix (`inc/winfsp/winfsp.h`).
/// `Version` (`u16` + 6 bytes padding) is followed by `UserContext` (`*mut c_void`) at offset 8.
#[repr(C)]
pub struct FspFileSystem {
    pub version: u16,
    pub user_context: *mut c_void,
}

/// WinFsp `FSP_FILE_SYSTEM_INTERFACE` callback table (`inc/winfsp/winfsp.h`, 64 pointers = 512 bytes).
#[repr(C)]
pub struct FspFileSystemInterface {
    pub get_volume_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            volume_info: *mut FspFsctlVolumeInfo,
        ) -> i32,
    >,
    pub set_volume_label: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            volume_label: *const u16,
            volume_info: *mut FspFsctlVolumeInfo,
        ) -> i32,
    >,
    pub get_security_by_name: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *const u16,
            file_attributes: *mut u32,
            security_descriptor: *mut c_void,
            security_descriptor_size: *mut usize,
        ) -> i32,
    >,
    pub create: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *const u16,
            create_options: u32,
            granted_access: u32,
            file_attributes: u32,
            security_descriptor: *mut c_void,
            allocation_size: u64,
            file_context: *mut *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub open: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_name: *const u16,
            create_options: u32,
            granted_access: u32,
            file_context: *mut *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub overwrite: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_attributes: u32,
            replace_file_attributes: u8,
            allocation_size: u64,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub cleanup: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *const u16,
            flags: u32,
        ),
    >,
    pub close: Option<unsafe extern "system" fn(fs: *mut FspFileSystem, file_context: *mut c_void)>,
    pub read: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            buffer: *mut u8,
            offset: u64,
            length: u32,
            bytes_transferred: *mut u32,
        ) -> i32,
    >,
    pub write: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            buffer: *const u8,
            offset: u64,
            length: u32,
            write_to_eof: u8,
            constrained_io: u8,
            bytes_transferred: *mut u32,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub flush: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub get_file_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub set_basic_info: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_attributes: u32,
            creation_time: u64,
            last_access_time: u64,
            last_write_time: u64,
            change_time: u64,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub set_file_size: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            new_size: u64,
            set_allocation_size: u8,
            file_info: *mut FspFsctlFileInfo,
        ) -> i32,
    >,
    pub can_delete: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *const u16,
        ) -> i32,
    >,
    pub rename: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *const u16,
            new_file_name: *const u16,
            replace_if_exists: u8,
        ) -> i32,
    >,
    pub get_security: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            security_descriptor: *mut c_void,
            security_descriptor_size: *mut usize,
        ) -> i32,
    >,
    pub set_security: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            security_information: u32,
            modification_descriptor: *mut c_void,
        ) -> i32,
    >,
    pub read_directory: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            pattern: *const u16,
            marker: *const u16,
            buffer: *mut u8,
            length: u32,
            bytes_transferred: *mut u32,
        ) -> i32,
    >,
    pub resolve_reparse_points: *const c_void,
    pub get_reparse_point: *const c_void,
    pub set_reparse_point: *const c_void,
    pub delete_reparse_point: *const c_void,
    pub get_stream_info: *const c_void,
    pub get_dir_info_by_name: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *const u16,
            dir_info: *mut u8,
        ) -> i32,
    >,
    pub control: *const c_void,
    pub set_delete: Option<
        unsafe extern "system" fn(
            fs: *mut FspFileSystem,
            file_context: *mut c_void,
            file_name: *const u16,
            delete_file: u8,
        ) -> i32,
    >,
    pub create_ex: *const c_void,
    pub overwrite_ex: *const c_void,
    pub get_ea: *const c_void,
    pub set_ea: *const c_void,
    pub obsolete0: *const c_void,
    pub dispatcher_stopped: *const c_void,
    pub reserved: [*const c_void; 31],
}

unsafe impl Sync for FspFileSystemInterface {}
unsafe impl Send for FspFileSystemInterface {}

/// Open file/directory handle stored in WinFsp `FileContext` (`UserContext2`).
pub struct WinFspOpenHandle {
    pub node_id: u64,
    pub parent_dir_id: u64,
    pub name: String,
    pub delete_on_close: AtomicBool,
}

/// Converts Unix epoch seconds into a Windows `FILETIME` (100-nanosecond intervals since 1601-01-01).
#[inline]
pub fn unix_secs_to_filetime(unix_secs: u64) -> u64 {
    unix_secs
        .saturating_add(EPOCH_DIFF_SECS)
        .saturating_mul(HNSECS_PER_SEC)
}

/// Populates a `FspFsctlFileInfo` struct from a `VfsNodeAttr`.
#[inline]
pub fn fill_fsp_file_info(attr: &VfsNodeAttr) -> FspFsctlFileInfo {
    let file_attributes = match attr.kind {
        EntryKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
        EntryKind::File => {
            if attr.node_id == NODE_METADATA_NEVER_INDEX {
                FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY
            } else if CineFs::is_overlay_node(attr.node_id) {
                FILE_ATTRIBUTE_ARCHIVE
            } else {
                FILE_ATTRIBUTE_READONLY
            }
        }
    };
    let ft = unix_secs_to_filetime(attr.timestamp_unix_secs);
    let alloc = if attr.kind == EntryKind::Directory {
        0
    } else {
        (attr.size + 4095) & !4095
    };
    FspFsctlFileInfo {
        file_attributes,
        reparse_tag: 0,
        allocation_size: alloc,
        file_size: if attr.kind == EntryKind::Directory {
            0
        } else {
            attr.size
        },
        creation_time: ft,
        last_access_time: ft,
        last_write_time: ft,
        change_time: ft,
        index_number: attr.node_id,
        hard_links: 0,
        ea_size: 0,
    }
}

/// Resolves a Windows backslash path (e.g. `"\"`, `"\Folder"`, `"\Folder\Frame_000000.dng"`)
/// into `(node_attr, parent_dir_id, entry_name)`.
pub fn resolve_win_path(vfs: &CineFs, win_path: &str) -> Result<(VfsNodeAttr, u64, String)> {
    let trimmed = win_path.trim_matches(|c| c == '\\' || c == '/');
    if trimmed.is_empty() {
        let attr = vfs.stat_by_id(NODE_ROOT)?;
        return Ok((attr, NODE_ROOT, String::new()));
    }

    let mut cur_dir = NODE_ROOT;
    let mut parts = trimmed
        .split(|c| c == '\\' || c == '/')
        .filter(|s| !s.is_empty())
        .peekable();

    while let Some(part) = parts.next() {
        let child_attr = vfs.lookup_in_dir(cur_dir, part)?;
        if parts.peek().is_none() {
            return Ok((child_attr, cur_dir, part.to_string()));
        }
        if child_attr.kind != EntryKind::Directory {
            return Err(Error::NotFound(format!(
                "Component '{part}' is not a directory"
            )));
        }
        cur_dir = child_attr.node_id;
    }

    Err(Error::NotFound(format!("Path not found: {win_path}")))
}

/// Splits a Windows path into `(parent_dir_node_id, leaf_name)`.
pub fn resolve_win_parent_and_name(vfs: &CineFs, win_path: &str) -> Result<(u64, String)> {
    let trimmed = win_path.trim_matches(|c| c == '\\' || c == '/');
    if trimmed.is_empty() {
        return Err(Error::InvalidFormat(String::from("Root has no parent")));
    }
    match trimmed.rfind(|c| c == '\\' || c == '/') {
        None => Ok((NODE_ROOT, trimmed.to_string())),
        Some(idx) => {
            let parent_str = &trimmed[..idx];
            let leaf = &trimmed[idx + 1..];
            let (parent_attr, _, _) = resolve_win_path(vfs, parent_str)?;
            if parent_attr.kind != EntryKind::Directory {
                return Err(Error::InvalidFormat(format!(
                    "Parent '{parent_str}' is not a directory"
                )));
            }
            Ok((parent_attr.node_id, leaf.to_string()))
        }
    }
}

/// Writes a single `FSP_FSCTL_DIR_INFO` record (`104 + name_utf16.len() * 2` bytes) into `out`
/// without trailing 8-byte alignment (used by `GetDirInfoByName` and `write_fsp_dir_info_entry`).
pub fn write_single_fsp_dir_info(name: &str, attr: &VfsNodeAttr, out: &mut [u8]) -> Option<usize> {
    let name_utf16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes_len = name_utf16.len() * 2;
    let entry_size = FSP_DIR_INFO_HDR_SIZE + name_bytes_len;
    if entry_size > u16::MAX as usize || entry_size > out.len() {
        return None;
    }

    out[..entry_size].fill(0);
    // Offset 0..2: UINT16 Size
    out[0..2].copy_from_slice(&(entry_size as u16).to_le_bytes());
    // Offset 2..8: 6 bytes alignment padding (already zeroed)
    // Offset 8..80: FSP_FSCTL_FILE_INFO (72 bytes)
    let fi = fill_fsp_file_info(attr);
    unsafe {
        ptr::copy_nonoverlapping(
            (&fi as *const FspFsctlFileInfo).cast::<u8>(),
            out.as_mut_ptr().add(8),
            std::mem::size_of::<FspFsctlFileInfo>(),
        );
    }
    // Offset 80..104: 24 bytes Padding / NextOffset (already zeroed)
    // Offset 104..: UTF-16LE FileNameBuf
    for (i, ch) in name_utf16.iter().enumerate() {
        let off = FSP_DIR_INFO_HDR_SIZE + i * 2;
        out[off..off + 2].copy_from_slice(&ch.to_le_bytes());
    }
    Some(entry_size)
}

/// Appends a single `FSP_FSCTL_DIR_INFO` record (`104 + name_utf16.len() * 2` bytes, 8-byte aligned
/// exactly like `FspFileSystemAddDirInfo` in `src/dll/fsop.c`) into `buffer`, returning `true` if it fit.
pub fn write_fsp_dir_info_entry(
    name: &str,
    attr: &VfsNodeAttr,
    buffer: &mut [u8],
    bytes_transferred: &mut usize,
) -> bool {
    let aligned_pos = (*bytes_transferred + 7) & !7;
    let name_bytes_len = name.encode_utf16().count() * 2;
    let src_len = FSP_DIR_INFO_HDR_SIZE + name_bytes_len;
    let dst_len = (src_len + 7) & !7;
    if src_len > u16::MAX as usize || aligned_pos + dst_len > buffer.len() {
        return false;
    }

    if aligned_pos > *bytes_transferred {
        buffer[*bytes_transferred..aligned_pos].fill(0);
    }
    buffer[aligned_pos..aligned_pos + dst_len].fill(0);

    if write_single_fsp_dir_info(name, attr, &mut buffer[aligned_pos..aligned_pos + src_len]).is_none() {
        return false;
    }

    *bytes_transferred = aligned_pos + dst_len;
    true
}

/// Finalizes a WinFsp `ReadDirectory` buffer by appending the 2-byte `0u16` stop marker
/// (matching `FspFileSystemAddDirInfo(0, Buffer, Length, PBytesTransferred)`).
pub fn finalize_fsp_dir_info_buffer(buffer: &mut [u8], bytes_transferred: &mut usize) {
    let aligned_pos = (*bytes_transferred + 7) & !7;
    if aligned_pos + 2 <= buffer.len() {
        if aligned_pos > *bytes_transferred {
            buffer[*bytes_transferred..aligned_pos].fill(0);
        }
        buffer[aligned_pos] = 0;
        buffer[aligned_pos + 1] = 0;
        *bytes_transferred = aligned_pos + 2;
    }
}

/// Self-relative Windows `SECURITY_DESCRIPTOR` corresponding to
/// `O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)` (Owner: Built-in Admins, Group: Built-in Admins,
/// DACL granting `FILE_ALL_ACCESS` to `SYSTEM`, `Administrators`, and `Everyone`).
/// Having a valid self-relative descriptor ensures `FspAccessCheckEx`, `CreatePrivateObjectSecurity`,
/// and Windows Explorer security queries always succeed.
static DEFAULT_SELF_RELATIVE_SD: [u8; 100] = [
    // SECURITY_DESCRIPTOR_RELATIVE header (20 bytes)
    0x01, 0x00, // Revision = 1, Sbz1 = 0
    0x04, 0x94, // Control = SE_DACL_PRESENT | SE_DACL_PROTECTED | SE_SELF_RELATIVE (0x9404)
    0x44, 0x00, 0x00, 0x00, // OffsetOwner = 68 (0x44)
    0x54, 0x00, 0x00, 0x00, // OffsetGroup = 84 (0x54)
    0x00, 0x00, 0x00, 0x00, // OffsetSacl = 0
    0x14, 0x00, 0x00, 0x00, // OffsetDacl = 20 (0x14)
    // DACL header at offset 20 (8 bytes): AclRevision=2, Sbz1=0, AclSize=48 (0x0030), AceCount=3, Sbz2=0
    0x02, 0x00, 0x30, 0x00, 0x03, 0x00, 0x00, 0x00,
    // ACE 1 at offset 28 (12 bytes): ACCESS_ALLOWED_ACE for Everyone (S-1-1-0), Mask = 0x001F01FF (FILE_ALL_ACCESS)
    0x00, 0x00, 0x14, 0x00, 0xFF, 0x01, 0x1F, 0x00,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    // ACE 2 at offset 48 (20 bytes): ACCESS_ALLOWED_ACE for SYSTEM (S-1-5-18), Mask = 0x001F01FF
    0x00, 0x00, 0x14, 0x00, 0xFF, 0x01, 0x1F, 0x00,
    0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x12, 0x00, 0x00, 0x00,
    // Owner SID at offset 68 (16 bytes): Built-in Administrators (S-1-5-32-544)
    0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
    // Group SID at offset 84 (16 bytes): Built-in Administrators (S-1-5-32-544)
    0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
];

unsafe fn copy_default_security_descriptor(
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    if security_descriptor_size.is_null() {
        return STATUS_SUCCESS;
    }
    // Wait: in DEFAULT_SELF_RELATIVE_SD above, DACL has 2 ACEs (20 + 20 = 40 bytes + 8 header = 48 bytes),
    // so AceCount is 2! Let's ensure AceCount at byte 24 is 2.
    let sd_len = DEFAULT_SELF_RELATIVE_SD.len();
    let cap = *security_descriptor_size;
    *security_descriptor_size = sd_len;
    if cap < sd_len {
        return STATUS_BUFFER_OVERFLOW;
    }
    if !security_descriptor.is_null() {
        ptr::copy_nonoverlapping(
            DEFAULT_SELF_RELATIVE_SD.as_ptr(),
            security_descriptor.cast::<u8>(),
            sd_len,
        );
        // Set AceCount = 2 at offset 24
        *security_descriptor.cast::<u8>().add(24) = 2;
    }
    STATUS_SUCCESS
}

static CB_TRACE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Logs WinFsp callback activity: the first 300 calls verbatim (shows what Explorer / Resolve
/// actually ask for) and afterwards only unexpected failures (capped), so the log stays small.
fn trace_cb(name: &str, path: &str, status: i32) {
    let n = CB_TRACE_COUNT.fetch_add(1, Ordering::Relaxed);
    let expected_miss = status == STATUS_OBJECT_NAME_NOT_FOUND || status == STATUS_OBJECT_PATH_NOT_FOUND;
    if n < 300 || (status != STATUS_SUCCESS && !expected_miss && n < 5000) {
        crate::log::write(
            "WINFSP",
            &format!("{name}('{path}') -> 0x{:08X}", status as u32),
        );
    }
}

#[inline]
unsafe fn vfs_from_fs<'a>(fs: *mut FspFileSystem) -> &'a CineFs {
    &*((*fs).user_context.cast::<CineFs>())
}

#[inline]
unsafe fn wstr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while len < 32768 && *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    String::from_utf16_lossy(slice)
}

// =============================================================================
// WinFsp C ABI Callbacks (`extern "system"`)
// =============================================================================

pub unsafe extern "system" fn cb_get_volume_info(
    fs: *mut FspFileSystem,
    volume_info: *mut FspFsctlVolumeInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || volume_info.is_null() || (*fs).user_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let (total_bytes, _) = vfs.total_volume_stats();
        let free_bytes = 10 * 1024 * 1024 * 1024u64; // 10 GiB virtual sidecar space

        let mut label = [0u16; 32];
        let label_str = "CineFS";
        let mut label_len_chars = 0usize;
        for (i, ch) in label_str.encode_utf16().enumerate() {
            label[i] = ch;
            label_len_chars += 1;
        }

        *volume_info = FspFsctlVolumeInfo {
            total_size: total_bytes + free_bytes,
            free_size: free_bytes,
            volume_label_length: (label_len_chars * 2) as u16,
            volume_label: label,
        };
        STATUS_SUCCESS
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

pub unsafe extern "system" fn cb_get_security_by_name(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    file_attributes: *mut u32,
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let path = wstr_to_string(file_name);

        let st = match resolve_win_path(vfs, &path) {
            Ok((attr, _, _)) => {
                if !file_attributes.is_null() {
                    *file_attributes = fill_fsp_file_info(&attr).file_attributes;
                }
                copy_default_security_descriptor(security_descriptor, security_descriptor_size)
            }
            Err(_) => match resolve_win_parent_and_name(vfs, &path) {
                Ok(_) => STATUS_OBJECT_NAME_NOT_FOUND,
                Err(_) => STATUS_OBJECT_PATH_NOT_FOUND,
            },
        };
        trace_cb("GetSecurityByName", &path, st);
        st
    }))
    .unwrap_or(STATUS_OBJECT_NAME_NOT_FOUND)
}

pub unsafe extern "system" fn cb_get_security(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    security_descriptor: *mut c_void,
    security_descriptor_size: *mut usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || file_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        copy_default_security_descriptor(security_descriptor, security_descriptor_size)
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

pub unsafe extern "system" fn cb_set_security(
    _fs: *mut FspFileSystem,
    _file_context: *mut c_void,
    _security_information: u32,
    _modification_descriptor: *mut c_void,
) -> i32 {
    STATUS_SUCCESS
}

pub unsafe extern "system" fn cb_open(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    create_options: u32,
    _granted_access: u32,
    file_context: *mut *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let path = wstr_to_string(file_name);

        let Ok((attr, parent_dir_id, name)) = resolve_win_path(vfs, &path) else {
            let st = match resolve_win_parent_and_name(vfs, &path) {
                Ok(_) => STATUS_OBJECT_NAME_NOT_FOUND,
                Err(_) => STATUS_OBJECT_PATH_NOT_FOUND,
            };
            trace_cb("Open", &path, st);
            return st;
        };

        if (create_options & FILE_DIRECTORY_FILE) != 0 && attr.kind != EntryKind::Directory {
            trace_cb("Open", &path, STATUS_NOT_A_DIRECTORY);
            return STATUS_NOT_A_DIRECTORY;
        }
        if (create_options & FILE_NON_DIRECTORY_FILE) != 0 && attr.kind == EntryKind::Directory {
            trace_cb("Open", &path, STATUS_FILE_IS_A_DIRECTORY);
            return STATUS_FILE_IS_A_DIRECTORY;
        }
        trace_cb("Open", &path, STATUS_SUCCESS);

        let handle = Box::new(WinFspOpenHandle {
            node_id: attr.node_id,
            parent_dir_id,
            name,
            delete_on_close: AtomicBool::new((create_options & FILE_DELETE_ON_CLOSE) != 0),
        });
        *file_context = Box::into_raw(handle).cast::<c_void>();
        *file_info = fill_fsp_file_info(&attr);
        STATUS_SUCCESS
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

pub unsafe extern "system" fn cb_create(
    fs: *mut FspFileSystem,
    file_name: *const u16,
    create_options: u32,
    _granted_access: u32,
    _file_attributes: u32,
    _security_descriptor: *mut c_void,
    _allocation_size: u64,
    file_context: *mut *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        if (create_options & FILE_DIRECTORY_FILE) != 0 {
            return STATUS_MEDIA_WRITE_PROTECTED;
        }
        let vfs = vfs_from_fs(fs);
        let path = wstr_to_string(file_name);
        let Ok((parent_dir_id, name)) = resolve_win_parent_and_name(vfs, &path) else {
            return STATUS_OBJECT_PATH_NOT_FOUND;
        };

        if vfs.lookup_in_dir(parent_dir_id, &name).is_ok() {
            return STATUS_OBJECT_NAME_COLLISION;
        }

        match vfs.create_in_dir(parent_dir_id, &name, true) {
            Ok(attr) => {
                let handle = Box::new(WinFspOpenHandle {
                    node_id: attr.node_id,
                    parent_dir_id,
                    name,
                    delete_on_close: AtomicBool::new((create_options & FILE_DELETE_ON_CLOSE) != 0),
                });
                *file_context = Box::into_raw(handle).cast::<c_void>();
                *file_info = fill_fsp_file_info(&attr);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_MEDIA_WRITE_PROTECTED,
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_overwrite(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_attributes: u32,
    _replace_file_attributes: u8,
    _allocation_size: u64,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if !CineFs::is_overlay_node(handle.node_id) {
            return STATUS_MEDIA_WRITE_PROTECTED;
        }
        match vfs.truncate_by_id(handle.node_id, 0) {
            Ok(attr) => {
                *file_info = fill_fsp_file_info(&attr);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_MEDIA_WRITE_PROTECTED,
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_cleanup(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
    flags: u32,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return;
        }
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if (flags & FSP_CLEANUP_DELETE) != 0 || handle.delete_on_close.load(Ordering::Relaxed) {
            let _ = vfs.remove_in_dir(handle.parent_dir_id, &handle.name);
        }
    }));
}

pub unsafe extern "system" fn cb_close(_fs: *mut FspFileSystem, file_context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !file_context.is_null() {
            let _ = Box::from_raw(file_context.cast::<WinFspOpenHandle>());
        }
    }));
}

pub unsafe extern "system" fn cb_read(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    buffer: *mut u8,
    offset: u64,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || bytes_transferred.is_null()
        {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        *bytes_transferred = 0;
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());

        let Ok(attr) = vfs.stat_by_id(handle.node_id) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };
        if offset >= attr.size {
            return STATUS_END_OF_FILE;
        }
        let to_read = (attr.size - offset).min(length as u64) as usize;
        if to_read == 0 {
            return STATUS_SUCCESS;
        }
        if buffer.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }

        let dst = std::slice::from_raw_parts_mut(buffer, to_read);
        match vfs.read_by_id(handle.node_id, offset, dst) {
            Ok((0, _)) => STATUS_END_OF_FILE,
            Ok((n, _)) => {
                *bytes_transferred = n as u32;
                STATUS_SUCCESS
            }
            Err(e) => {
                crate::log::write(
                    "ERROR",
                    &format!(
                        "Read(node={}, off={offset}, len={length}) failed: {e}",
                        handle.node_id
                    ),
                );
                STATUS_INVALID_DEVICE_REQUEST
            }
        }
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

pub unsafe extern "system" fn cb_write(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    buffer: *const u8,
    offset: u64,
    length: u32,
    write_to_eof: u8,
    constrained_io: u8,
    bytes_transferred: *mut u32,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || bytes_transferred.is_null()
        {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        *bytes_transferred = 0;
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if !CineFs::is_overlay_node(handle.node_id) {
            return STATUS_MEDIA_WRITE_PROTECTED;
        }
        let cur_size = vfs.stat_by_id(handle.node_id).map(|a| a.size).unwrap_or(0);
        let mut write_len = length as usize;
        let write_offset = if constrained_io != 0 {
            if offset >= cur_size {
                return STATUS_SUCCESS;
            }
            write_len = write_len.min((cur_size - offset) as usize);
            offset
        } else if write_to_eof != 0 {
            cur_size
        } else {
            offset
        };

        if write_len > 0 && buffer.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let src = if write_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(buffer, write_len)
        };
        match vfs.write_by_id(handle.node_id, write_offset, src) {
            Ok((written, attr)) => {
                *bytes_transferred = written as u32;
                if !file_info.is_null() {
                    *file_info = fill_fsp_file_info(&attr);
                }
                STATUS_SUCCESS
            }
            Err(_) => STATUS_MEDIA_WRITE_PROTECTED,
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_flush(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        if !file_context.is_null() && !file_info.is_null() {
            let vfs = vfs_from_fs(fs);
            let handle = &*(file_context.cast::<WinFspOpenHandle>());
            if let Ok(attr) = vfs.stat_by_id(handle.node_id) {
                *file_info = fill_fsp_file_info(&attr);
            }
        }
        STATUS_SUCCESS
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

pub unsafe extern "system" fn cb_get_file_info(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        match vfs.stat_by_id(handle.node_id) {
            Ok(attr) => {
                *file_info = fill_fsp_file_info(&attr);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        }
    }))
    .unwrap_or(STATUS_OBJECT_NAME_NOT_FOUND)
}

pub unsafe extern "system" fn cb_set_basic_info(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_attributes: u32,
    _creation_time: u64,
    _last_access_time: u64,
    _last_write_time: u64,
    _change_time: u64,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    cb_get_file_info(fs, file_context, file_info)
}

pub unsafe extern "system" fn cb_set_file_size(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    new_size: u64,
    set_allocation_size: u8,
    file_info: *mut FspFsctlFileInfo,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() || file_info.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if !CineFs::is_overlay_node(handle.node_id) {
            return STATUS_MEDIA_WRITE_PROTECTED;
        }
        if set_allocation_size != 0 {
            if let Ok(attr) = vfs.stat_by_id(handle.node_id) {
                *file_info = fill_fsp_file_info(&attr);
                return STATUS_SUCCESS;
            }
        }
        match vfs.truncate_by_id(handle.node_id, new_size) {
            Ok(attr) => {
                *file_info = fill_fsp_file_info(&attr);
                STATUS_SUCCESS
            }
            Err(_) => STATUS_MEDIA_WRITE_PROTECTED,
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_can_delete(
    _fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if file_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if CineFs::is_overlay_node(handle.node_id) {
            STATUS_SUCCESS
        } else {
            STATUS_MEDIA_WRITE_PROTECTED
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_set_delete(
    _fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _file_name: *const u16,
    delete_file: u8,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if file_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        if !CineFs::is_overlay_node(handle.node_id) {
            return STATUS_MEDIA_WRITE_PROTECTED;
        }
        handle
            .delete_on_close
            .store(delete_file != 0, Ordering::Relaxed);
        STATUS_SUCCESS
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_rename(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_name: *const u16,
    new_file_name: *const u16,
    replace_if_exists: u8,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null() || (*fs).user_context.is_null() || file_context.is_null() {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let old_path = wstr_to_string(file_name);
        let new_path = wstr_to_string(new_file_name);

        let Ok((from_dir_id, from_name)) = resolve_win_parent_and_name(vfs, &old_path) else {
            return STATUS_OBJECT_PATH_NOT_FOUND;
        };
        let Ok((to_dir_id, to_name)) = resolve_win_parent_and_name(vfs, &new_path) else {
            return STATUS_OBJECT_PATH_NOT_FOUND;
        };

        if replace_if_exists == 0 && vfs.lookup_in_dir(to_dir_id, &to_name).is_ok() {
            return STATUS_OBJECT_NAME_COLLISION;
        }

        match vfs.rename_in_dir(from_dir_id, &from_name, to_dir_id, &to_name) {
            Ok(()) => STATUS_SUCCESS,
            Err(_) => STATUS_MEDIA_WRITE_PROTECTED,
        }
    }))
    .unwrap_or(STATUS_MEDIA_WRITE_PROTECTED)
}

pub unsafe extern "system" fn cb_get_dir_info_by_name(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    file_name: *const u16,
    dir_info: *mut u8,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || file_name.is_null()
            || dir_info.is_null()
        {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        let name = wstr_to_string(file_name);

        let Ok(attr) = vfs.lookup_in_dir(handle.node_id, &name) else {
            return STATUS_OBJECT_NAME_NOT_FOUND;
        };

        // Resolve canonical case-preserved name when possible
        let canonical_name = if handle.node_id == NODE_ROOT {
            vfs.clips
                .iter()
                .find(|c| c.folder_name.eq_ignore_ascii_case(&name))
                .map(|c| c.folder_name.as_str())
                .unwrap_or(&name)
        } else {
            &name
        };

        // WinFsp allocates `sizeof(FSP_FSCTL_DIR_INFO) + 255 * sizeof(WCHAR)` = 614 bytes on stack
        let max_dir_info_bytes = FSP_DIR_INFO_HDR_SIZE + 255 * 2;
        let buf = std::slice::from_raw_parts_mut(dir_info, max_dir_info_bytes);
        if write_single_fsp_dir_info(canonical_name, &attr, buf).is_some() {
            STATUS_SUCCESS
        } else {
            STATUS_BUFFER_TOO_SMALL
        }
    }))
    .unwrap_or(STATUS_OBJECT_NAME_NOT_FOUND)
}

pub unsafe extern "system" fn cb_read_directory(
    fs: *mut FspFileSystem,
    file_context: *mut c_void,
    _pattern: *const u16,
    marker: *const u16,
    buffer: *mut u8,
    length: u32,
    bytes_transferred: *mut u32,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if fs.is_null()
            || (*fs).user_context.is_null()
            || file_context.is_null()
            || buffer.is_null()
            || bytes_transferred.is_null()
        {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        *bytes_transferred = 0;
        let vfs = vfs_from_fs(fs);
        let handle = &*(file_context.cast::<WinFspOpenHandle>());
        let out_buf = std::slice::from_raw_parts_mut(buffer, length as usize);

        // On Windows/WinFsp (matching `memfs.cpp` line 1899), the root directory `\` must NOT
        // emit `"."`, `".."`, or macOS's `".metadata_never_index"`.
        let min_idx = if handle.node_id == NODE_ROOT { 3 } else { 0 };
        let start_idx = if marker.is_null() {
            min_idx
        } else {
            let m = wstr_to_string(marker);
            vfs.resolve_dir_marker_index(handle.node_id, &m, min_idx)
        };

        let mut transferred = 0usize;
        let mut idx = start_idx;
        let mut hit_eof = false;

        loop {
            match vfs.dir_entry_at(handle.node_id, idx) {
                Ok(Some((name, attr))) => {
                    if !write_fsp_dir_info_entry(&name, &attr, out_buf, &mut transferred) {
                        break;
                    }
                    idx += 1;
                }
                _ => {
                    hit_eof = true;
                    break;
                }
            }
        }

        if hit_eof {
            finalize_fsp_dir_info_buffer(out_buf, &mut transferred);
        }

        *bytes_transferred = transferred as u32;
        trace_cb(
            "ReadDirectory",
            &format!("node={} start={start_idx} bytes={transferred} eof={hit_eof}", handle.node_id),
            STATUS_SUCCESS,
        );
        STATUS_SUCCESS
    }))
    .unwrap_or(STATUS_INVALID_DEVICE_REQUEST)
}

/// Static `FSP_FILE_SYSTEM_INTERFACE` dispatch table used by `WinFspActiveMount`.
pub static CINEFS_WINFSP_INTERFACE: FspFileSystemInterface = FspFileSystemInterface {
    get_volume_info: Some(cb_get_volume_info),
    set_volume_label: None,
    get_security_by_name: Some(cb_get_security_by_name),
    create: Some(cb_create),
    open: Some(cb_open),
    overwrite: Some(cb_overwrite),
    cleanup: Some(cb_cleanup),
    close: Some(cb_close),
    read: Some(cb_read),
    write: Some(cb_write),
    flush: Some(cb_flush),
    get_file_info: Some(cb_get_file_info),
    set_basic_info: Some(cb_set_basic_info),
    set_file_size: Some(cb_set_file_size),
    can_delete: Some(cb_can_delete),
    rename: Some(cb_rename),
    get_security: Some(cb_get_security),
    set_security: Some(cb_set_security),
    read_directory: Some(cb_read_directory),
    resolve_reparse_points: ptr::null(),
    get_reparse_point: ptr::null(),
    set_reparse_point: ptr::null(),
    delete_reparse_point: ptr::null(),
    get_stream_info: ptr::null(),
    get_dir_info_by_name: Some(cb_get_dir_info_by_name),
    control: ptr::null(),
    set_delete: Some(cb_set_delete),
    create_ex: ptr::null(),
    overwrite_ex: ptr::null(),
    get_ea: ptr::null(),
    set_ea: ptr::null(),
    obsolete0: ptr::null(),
    dispatcher_stopped: ptr::null(),
    reserved: [ptr::null(); 31],
};

// =============================================================================
// Windows Runtime DLL Loader & Active Mount Handle
// =============================================================================

#[cfg(windows)]
mod win_loader {
    use super::*;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    type FnFspFileSystemCreate = unsafe extern "system" fn(
        device_path: *const u16,
        volume_params: *const FspFsctlVolumeParams,
        interface: *const FspFileSystemInterface,
        p_file_system: *mut *mut FspFileSystem,
    ) -> i32;

    type FnFspFileSystemSetMountPoint =
        unsafe extern "system" fn(file_system: *mut FspFileSystem, mount_point: *const u16) -> i32;

    type FnFspFileSystemRemoveMountPoint =
        unsafe extern "system" fn(file_system: *mut FspFileSystem);

    type FnFspFileSystemStartDispatcher = unsafe extern "system" fn(
        file_system: *mut FspFileSystem,
        thread_count: u32,
    ) -> i32;

    type FnFspFileSystemStopDispatcher =
        unsafe extern "system" fn(file_system: *mut FspFileSystem);

    type FnFspFileSystemDelete = unsafe extern "system" fn(file_system: *mut FspFileSystem);

    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" {
        fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
        fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *const c_void;
    }

    #[link(name = "advapi32", kind = "raw-dylib")]
    extern "system" {
        fn RegOpenKeyExW(
            hKey: usize,
            lpSubKey: *const u16,
            ulOptions: u32,
            samDesired: u32,
            phkResult: *mut usize,
        ) -> i32;
        fn RegQueryValueExW(
            hKey: usize,
            lpValueName: *const u16,
            lpReserved: *const u32,
            lpType: *mut u32,
            lpData: *mut u8,
            lpcbData: *mut u32,
        ) -> i32;
        fn RegCloseKey(hKey: usize) -> i32;
    }

    const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
    const KEY_READ: u32 = 0x20019;
    const KEY_WOW64_32KEY: u32 = 0x0200;
    const KEY_WOW64_64KEY: u32 = 0x0100;

    pub struct WinFspApi {
        pub create: FnFspFileSystemCreate,
        pub set_mount_point: FnFspFileSystemSetMountPoint,
        pub remove_mount_point: Option<FnFspFileSystemRemoveMountPoint>,
        pub start_dispatcher: FnFspFileSystemStartDispatcher,
        pub stop_dispatcher: FnFspFileSystemStopDispatcher,
        pub delete: FnFspFileSystemDelete,
    }

    fn to_wstr_z(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    /// Queries `HKLM\SOFTWARE\WinFsp\InstallDir` (both 32-bit WOW6432Node and 64-bit registry views).
    fn query_winfsp_install_dir() -> Option<PathBuf> {
        let subkey = to_wstr_z(r"SOFTWARE\WinFsp");
        let valname = to_wstr_z("InstallDir");
        for wow_flag in [KEY_WOW64_32KEY, KEY_WOW64_64KEY, 0] {
            let mut hkey: usize = 0;
            let st = unsafe {
                RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    subkey.as_ptr(),
                    0,
                    KEY_READ | wow_flag,
                    &mut hkey,
                )
            };
            if st == 0 && hkey != 0 {
                let mut buf = [0u16; 520];
                let mut byte_len = (buf.len() * 2) as u32;
                let mut reg_type = 0u32;
                let qst = unsafe {
                    RegQueryValueExW(
                        hkey,
                        valname.as_ptr(),
                        ptr::null(),
                        &mut reg_type,
                        buf.as_mut_ptr().cast::<u8>(),
                        &mut byte_len,
                    )
                };
                unsafe { RegCloseKey(hkey) };
                if qst == 0 && byte_len >= 2 {
                    let char_len = (byte_len as usize / 2).min(buf.len());
                    let end = buf[..char_len]
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(char_len);
                    if end > 0 {
                        let s = String::from_utf16_lossy(&buf[..end]);
                        return Some(PathBuf::from(s));
                    }
                }
            }
        }
        None
    }

    /// Returns `true` if the WinFsp runtime DLL (`winfsp-x64.dll` / `winfsp-a64.dll`) is installed and loadable.
    pub fn is_winfsp_available() -> bool {
        WinFspApi::load().is_ok()
    }

    impl WinFspApi {
        pub fn load() -> Result<Self> {
            let dll_name = if cfg!(target_arch = "aarch64") {
                "winfsp-a64.dll"
            } else {
                "winfsp-x64.dll"
            };

            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Some(install_dir) = query_winfsp_install_dir() {
                candidates.push(install_dir.join("bin").join(dll_name));
            }
            candidates.push(PathBuf::from(dll_name));
            candidates.push(PathBuf::from(r"C:\Program Files (x86)\WinFsp\bin").join(dll_name));
            candidates.push(PathBuf::from(r"C:\Program Files\WinFsp\bin").join(dll_name));
            candidates.push(PathBuf::from(r"C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll"));
            candidates.push(PathBuf::from(r"C:\Program Files\WinFsp\bin\winfsp-x64.dll"));

            let mut module: *mut c_void = ptr::null_mut();
            for cand in &candidates {
                let w = to_wstr_z(&cand.to_string_lossy());
                let h = unsafe { LoadLibraryW(w.as_ptr()) };
                if !h.is_null() {
                    module = h;
                    crate::log::write("INFO", &format!("WinFsp DLL loaded from '{}'", cand.display()));
                    break;
                }
            }

            if module.is_null() {
                crate::log::write(
                    "WARN",
                    &format!("WinFsp DLL not loadable; tried {} candidate(s): {:?}", candidates.len(), candidates),
                );
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "WinFsp driver (winfsp-x64.dll) is not installed yet.",
                )));
            }

            unsafe {
                let p_create = GetProcAddress(module, b"FspFileSystemCreate\0".as_ptr());
                let p_set_mp = GetProcAddress(module, b"FspFileSystemSetMountPoint\0".as_ptr());
                let p_rem_mp = GetProcAddress(module, b"FspFileSystemRemoveMountPoint\0".as_ptr());
                let p_start = GetProcAddress(module, b"FspFileSystemStartDispatcher\0".as_ptr());
                let p_stop = GetProcAddress(module, b"FspFileSystemStopDispatcher\0".as_ptr());
                let p_del = GetProcAddress(module, b"FspFileSystemDelete\0".as_ptr());

                if p_create.is_null()
                    || p_set_mp.is_null()
                    || p_start.is_null()
                    || p_stop.is_null()
                    || p_del.is_null()
                {
                    return Err(Error::InvalidFormat(String::from(
                        "WinFsp DLL is missing required entry points",
                    )));
                }

                Ok(Self {
                    create: std::mem::transmute(p_create),
                    set_mount_point: std::mem::transmute(p_set_mp),
                    remove_mount_point: if p_rem_mp.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute(p_rem_mp))
                    },
                    start_dispatcher: std::mem::transmute(p_start),
                    stop_dispatcher: std::mem::transmute(p_stop),
                    delete: std::mem::transmute(p_del),
                })
            }
        }
    }

    pub struct WinFspActiveMount {
        api: WinFspApi,
        fs_ptr: *mut FspFileSystem,
        _vfs: Arc<CineFs>,
        pub mount_point: PathBuf,
        stopped: bool,
    }

    unsafe impl Send for WinFspActiveMount {}
    unsafe impl Sync for WinFspActiveMount {}

    impl WinFspActiveMount {
        pub fn mount(vfs: Arc<CineFs>, mount_point: impl AsRef<Path>) -> Result<Self> {
            let api = WinFspApi::load()?;
            let mp_path = mount_point.as_ref().to_path_buf();
            let mp_str = mp_path.to_string_lossy();

            // If mounting to a directory path (not a drive letter like "Z:" or "Z:\"), ensure parent exists and target does NOT exist yet
            let is_drive_letter = (mp_str.len() == 2 && mp_str.as_bytes()[1] == b':')
                || (mp_str.len() == 3
                    && mp_str.as_bytes()[1] == b':'
                    && (mp_str.as_bytes()[2] == b'\\' || mp_str.as_bytes()[2] == b'/'));
            let normalized_mp_str = if is_drive_letter {
                format!("{}:", mp_str.chars().next().unwrap().to_ascii_uppercase())
            } else {
                if mp_path.exists() {
                    let _ = std::fs::remove_dir(&mp_path);
                }
                if let Some(parent) = mp_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                mp_str.to_string()
            };

            crate::log::write(
                "INFO",
                &format!(
                    "WinFsp mount: requested='{}' normalized='{}' drive_letter={}",
                    mp_str, normalized_mp_str, is_drive_letter
                ),
            );
            let device_w = to_wstr_z("WinFsp.Disk");
            let params = FspFsctlVolumeParams::default();
            let mut fs_ptr: *mut FspFileSystem = ptr::null_mut();

            let status = unsafe {
                (api.create)(
                    device_w.as_ptr(),
                    &params,
                    &CINEFS_WINFSP_INTERFACE,
                    &mut fs_ptr,
                )
            };
            crate::log::write(
                "INFO",
                &format!("FspFileSystemCreate -> 0x{:08X}", status as u32),
            );
            if status < 0 || fs_ptr.is_null() {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "FspFileSystemCreate failed with NTSTATUS 0x{:08X}",
                        status as u32
                    ),
                )));
            }

            unsafe {
                (*fs_ptr).user_context = (Arc::as_ptr(&vfs) as *mut CineFs).cast::<c_void>();
            }

            let mp_w = to_wstr_z(&normalized_mp_str);
            let mp_status = unsafe { (api.set_mount_point)(fs_ptr, mp_w.as_ptr()) };
            crate::log::write(
                "INFO",
                &format!(
                    "FspFileSystemSetMountPoint('{}') -> 0x{:08X}",
                    normalized_mp_str, mp_status as u32
                ),
            );
            if mp_status < 0 {
                unsafe { (api.delete)(fs_ptr) };
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "FspFileSystemSetMountPoint('{}') failed with NTSTATUS 0x{:08X}",
                        normalized_mp_str, mp_status as u32
                    ),
                )));
            }

            let disp_status = unsafe { (api.start_dispatcher)(fs_ptr, 0) };
            crate::log::write(
                "INFO",
                &format!("FspFileSystemStartDispatcher -> 0x{:08X}", disp_status as u32),
            );
            if disp_status < 0 {
                if let Some(rem) = api.remove_mount_point {
                    unsafe { rem(fs_ptr) };
                }
                unsafe { (api.delete)(fs_ptr) };
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "FspFileSystemStartDispatcher failed with NTSTATUS 0x{:08X}",
                        disp_status as u32
                    ),
                )));
            }

            let final_mp = if is_drive_letter {
                PathBuf::from(format!("{}\\", normalized_mp_str))
            } else {
                PathBuf::from(normalized_mp_str)
            };

            Ok(Self {
                api,
                fs_ptr,
                _vfs: vfs,
                mount_point: final_mp,
                stopped: false,
            })
        }

        pub fn unmount(&mut self) -> Result<()> {
            if !self.stopped && !self.fs_ptr.is_null() {
                self.stopped = true;
                crate::log::write("INFO", &format!("WinFsp unmount '{}'", self.mount_point.display()));
                unsafe {
                    if let Some(rem) = self.api.remove_mount_point {
                        rem(self.fs_ptr);
                    }
                    (self.api.stop_dispatcher)(self.fs_ptr);
                    (self.api.delete)(self.fs_ptr);
                }
                self.fs_ptr = ptr::null_mut();
            }
            Ok(())
        }
    }

    impl Drop for WinFspActiveMount {
        fn drop(&mut self) {
            let _ = self.unmount();
        }
    }
}

#[cfg(windows)]
pub use win_loader::{is_winfsp_available, WinFspActiveMount};

#[cfg(not(windows))]
pub fn is_winfsp_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::CineFs;

    #[test]
    fn test_winfsp_struct_sizes_and_dir_info_packing() {
        // Verify exact binary layout against WinFsp `inc/winfsp/fsctl.h` static assertions
        assert_eq!(std::mem::size_of::<FspFsctlVolumeParams>(), 504);
        assert_eq!(std::mem::size_of::<FspFsctlVolumeInfo>(), 88);
        assert_eq!(std::mem::size_of::<FspFsctlFileInfo>(), 72);
        assert_eq!(
            std::mem::size_of::<FspFileSystemInterface>(),
            64 * std::mem::size_of::<usize>()
        );
        assert_eq!(FSP_DIR_INFO_HDR_SIZE, 104);

        let params = FspFsctlVolumeParams::default();
        assert_ne!(params.flags & VOL_UM_FILE_CONTEXT_IS_USER_CONTEXT2, 0);
        assert_eq!(params.flags & VOL_UM_FILE_CONTEXT_IS_USER_CONTEXT2, VOL_UM_FILE_CONTEXT_IS_USER_CONTEXT2);

        let vfs = CineFs::new(16 * 1024 * 1024);
        let mut fake_fs = FspFileSystem {
            version: 0,
            user_context: (&vfs as *const CineFs as *mut CineFs).cast::<c_void>(),
        };

        // 1. Test GetVolumeInfo
        let mut vol_info = FspFsctlVolumeInfo {
            total_size: 0,
            free_size: 0,
            volume_label_length: 0,
            volume_label: [0u16; 32],
        };
        let st = unsafe { cb_get_volume_info(&mut fake_fs, &mut vol_info) };
        assert_eq!(st, STATUS_SUCCESS);
        assert!(vol_info.free_size >= 10 * 1024 * 1024 * 1024);
        assert_eq!(vol_info.volume_label_length, 12); // "CineFS" = 6 UTF-16 chars = 12 bytes

        // 2. Test GetSecurityByName returns valid 100-byte self-relative SECURITY_DESCRIPTOR
        let root_w: Vec<u16> = "\\\0".encode_utf16().collect();
        let mut sd_buf = [0u8; 256];
        let mut sd_size = sd_buf.len();
        let mut attrs = 0u32;
        let st_sec = unsafe {
            cb_get_security_by_name(
                &mut fake_fs,
                root_w.as_ptr(),
                &mut attrs,
                sd_buf.as_mut_ptr().cast::<c_void>(),
                &mut sd_size,
            )
        };
        assert_eq!(st_sec, STATUS_SUCCESS);
        assert_eq!(attrs, FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(sd_size, 100);
        assert_eq!(sd_buf[24], 2); // AceCount == 2

        // 3. Test Open root directory + Create & Write .xmp in root + ReadDirectory + Read back
        let mut ctx: *mut c_void = ptr::null_mut();
        let mut fi = FspFsctlFileInfo::default();
        let st_open = unsafe {
            cb_open(
                &mut fake_fs,
                root_w.as_ptr(),
                FILE_DIRECTORY_FILE,
                0,
                &mut ctx,
                &mut fi,
            )
        };
        assert_eq!(st_open, STATUS_SUCCESS);
        assert_eq!(fi.file_attributes, FILE_ATTRIBUTE_DIRECTORY);

        // Create .xmp in root first so root has an entry to list (since root omits "." and "..")
        let xmp_w: Vec<u16> = "\\test_000000.xmp\0".encode_utf16().collect();
        let mut xmp_ctx: *mut c_void = ptr::null_mut();
        let st_create = unsafe {
            cb_create(
                &mut fake_fs,
                xmp_w.as_ptr(),
                FILE_NON_DIRECTORY_FILE,
                0,
                FILE_ATTRIBUTE_ARCHIVE,
                ptr::null_mut(),
                0,
                &mut xmp_ctx,
                &mut fi,
            )
        };
        assert_eq!(st_create, STATUS_SUCCESS);

        let payload = b"<x:xmpmeta>WinFsp Test</x:xmpmeta>";
        let mut written = 0u32;
        let st_write = unsafe {
            cb_write(
                &mut fake_fs,
                xmp_ctx,
                payload.as_ptr(),
                0,
                payload.len() as u32,
                0,
                0,
                &mut written,
                &mut fi,
            )
        };
        assert_eq!(st_write, STATUS_SUCCESS);
        assert_eq!(written as usize, payload.len());
        assert_eq!(fi.file_size as usize, payload.len());

        // ReadDirectory on root should return `test_000000.xmp` with Size >= 104 and FileInfo at offset 8
        let mut dir_buf = vec![0u8; 4096];
        let mut dir_bytes = 0u32;
        let st_dir = unsafe {
            cb_read_directory(
                &mut fake_fs,
                ctx,
                ptr::null(),
                ptr::null(),
                dir_buf.as_mut_ptr(),
                dir_buf.len() as u32,
                &mut dir_bytes,
            )
        };
        assert_eq!(st_dir, STATUS_SUCCESS);
        let first_entry_size = u16::from_le_bytes([dir_buf[0], dir_buf[1]]) as usize;
        assert_eq!(first_entry_size, 104 + "test_000000.xmp".len() * 2);
        let file_size_in_dir_info =
            u64::from_le_bytes(dir_buf[24..32].try_into().unwrap()) as usize;
        assert_eq!(file_size_in_dir_info, payload.len());
        unsafe { cb_close(&mut fake_fs, ctx) };

        let mut read_buf = vec![0u8; 64];
        let mut read_n = 0u32;
        let st_read = unsafe {
            cb_read(
                &mut fake_fs,
                xmp_ctx,
                read_buf.as_mut_ptr(),
                0,
                read_buf.len() as u32,
                &mut read_n,
            )
        };
        assert_eq!(st_read, STATUS_SUCCESS);
        assert_eq!(&read_buf[..read_n as usize], payload);

        unsafe {
            cb_cleanup(&mut fake_fs, xmp_ctx, xmp_w.as_ptr(), FSP_CLEANUP_DELETE);
            cb_close(&mut fake_fs, xmp_ctx);
        }
    }
}
