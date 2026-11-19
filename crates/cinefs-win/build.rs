//! Embeds the Windows resources into `cinefs-win.exe` (shipped as `CineFS.exe`):
//! the app icon (`windows/AppIcon.ico`) and a VERSIONINFO block.
//!
//! Explorer, desktop shortcuts and "Apps & features" read the icon from the exe's
//! resource section, not from what the running app loads at runtime, so without this
//! the exe shows the generic Windows icon.
//!
//! The `.res` file is written here directly (no `rc.exe`, no extra crates) and handed to
//! the MSVC linker, which accepts `.res` inputs natively. That keeps `cargo check
//! --target x86_64-pc-windows-msvc` working from Linux/macOS, where no resource compiler
//! exists (nothing is linked there).

use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const LANG_EN_US: u16 = 0x0409;
const CODEPAGE_UNICODE: u16 = 0x04B0;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_path = manifest_dir.join("../../windows/AppIcon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", ico_path.display());

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let ico = fs::read(&ico_path).expect("windows/AppIcon.ico is missing");
    let mut res = Vec::new();
    // A .res file starts with an empty 32-byte entry.
    push_entry(&mut res, 0, 0, 0, &[]);
    push_icon(&mut res, &ico);
    push_entry(&mut res, RT_VERSION, 1, 0x0030, &version_info());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("cinefs-win.res");
    fs::write(&out, res).expect("failed to write cinefs-win.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// One resource entry with numeric type and name (header is always 32 bytes).
fn push_entry(res: &mut Vec<u8>, ty: u16, name: u16, flags: u16, data: &[u8]) {
    let lang = if ty == 0 { 0 } else { LANG_EN_US };
    res.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&ty.to_le_bytes());
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    res.extend_from_slice(&lang.to_le_bytes()); // LanguageId
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    pad4(res);
}

/// Splits an .ico into one RT_ICON per image plus the RT_GROUP_ICON directory (ID 1,
/// the lowest ID, which is the one Explorer shows).
fn push_icon(res: &mut Vec<u8>, ico: &[u8]) {
    let u16_at = |p: usize| u16::from_le_bytes([ico[p], ico[p + 1]]);
    let u32_at = |p: usize| u32::from_le_bytes([ico[p], ico[p + 1], ico[p + 2], ico[p + 3]]);
    assert!(
        ico.len() >= 6 && u16_at(0) == 0 && u16_at(2) == 1,
        "not an .ico file"
    );
    let count = u16_at(4) as usize;

    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + i * 16;
        let size = u32_at(e + 8) as usize;
        let offset = u32_at(e + 12) as usize;
        let id = (i + 1) as u16;
        push_entry(res, RT_ICON, id, 0x1010, &ico[offset..offset + size]);
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the RT_ICON id.
        group.extend_from_slice(&ico[e..e + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    push_entry(res, RT_GROUP_ICON, 1, 0x1030, &group);
}

fn version_info() -> Vec<u8> {
    let parts: Vec<u16> = env::var("CARGO_PKG_VERSION")
        .unwrap()
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let (major, minor, patch) = (parts[0], parts[1], parts[2]);
    let version = format!("{major}.{minor}.{patch}.0");
    let ms = ((major as u32) << 16) | minor as u32;
    let ls = (patch as u32) << 16;

    let mut fixed = Vec::new();
    for v in [
        0xFEEF_04BDu32, // signature
        0x0001_0000,    // struct version
        ms,
        ls, // file version
        ms,
        ls,          // product version
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        0x0000_0001, // VFT_APP
        0,
        0,
        0,
    ] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }

    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "Özcan Çelik".to_string()),
        ("FileDescription", "CineFS".to_string()),
        ("FileVersion", version.clone()),
        ("InternalName", "CineFS".to_string()),
        (
            "LegalCopyright",
            "Copyright © Özcan Çelik. MIT OR Apache-2.0".to_string(),
        ),
        ("OriginalFilename", "CineFS.exe".to_string()),
        ("ProductName", "CineFS".to_string()),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, v)| {
        let value = utf16z(v);
        block(k, 1, (value.len() / 2) as u16, &value, &[])
    })
    .collect();
    let table_key = format!("{LANG_EN_US:04x}{CODEPAGE_UNICODE:04x}");
    let table = block(&table_key, 1, 0, &[], &strings);
    let string_file_info = block("StringFileInfo", 1, 0, &[], &[table]);

    let mut translation = Vec::new();
    translation.extend_from_slice(&LANG_EN_US.to_le_bytes());
    translation.extend_from_slice(&CODEPAGE_UNICODE.to_le_bytes());
    let var = block("Translation", 0, 4, &translation, &[]);
    let var_file_info = block("VarFileInfo", 1, 0, &[], &[var]);

    block(
        "VS_VERSION_INFO",
        0,
        fixed.len() as u16,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

/// A VERSIONINFO node: wLength, wValueLength, wType, key, padding, value, children.
fn block(key: &str, ty: u16, value_len: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for child in children {
        pad4(&mut b);
        b.extend_from_slice(child);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&value_len.to_le_bytes());
    b[4..6].copy_from_slice(&ty.to_le_bytes());
    b
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}
