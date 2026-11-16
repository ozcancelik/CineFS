//! Blackmagic RAW (`.braw`) reader and decoder — **experimental**.
//!
//! The container is plain QuickTime
//! ('moov' / 'trak' / 'stbl'), walked by hand here. Each sample is a 'bmdf'
//! tag/length/value metadata block followed by a 'braw' picture: tiled 16x8
//! macroblocks of 8x8 + 8x4 DCT blocks with a fixed Huffman table (tables and
//! scan orders as reverse-engineered by Paul B Mahol for FFmpeg's unmerged
//! `braw.c`). The output is 12-bit RGGB Bayer.
//!
//! What is *not* known, and is therefore approximated:
//! - Black / white level (64 / 3572) were converged on by eye against the SDK.
//! - White balance is grey-world, measured on the first frame — the camera's
//!   Kelvin/tint cannot account for the sensor's own channel bias.
//! - The colour matrix treats the sensor as Blackmagic Wide Gamut (Gen4/5).

use crate::color_matrices::ILLUMINANT_D65;
use crate::mlv::{CfaPattern, FrameDynamicMeta, MlvClip, MlvFrameEntry, RtciTime};
use crate::{read_exact_at, Error, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::OnceLock;

pub const BRAW_BLACK_LEVEL: i32 = 64;
pub const BRAW_WHITE_LEVEL: i32 = 3572;

/// XYZ(D65) -> Blackmagic Wide Gamut Gen4/5 linear RGB, normalised so D65 maps to (1,1,1).
const BMD_WIDE_GAMUT_XYZ_TO_CAM: [f64; 9] = [
    1.8662, -0.5182, -0.2346, -0.6004, 1.3782, 0.1767, 0.0024, 0.0864, 0.8368,
];

const BRAW_FOURCC: [&[u8; 4]; 6] = [b"brxq", b"brhq", b"brst", b"brlt", b"brvl", b"brvm"];

// =============================================================================
// QuickTime box walking
// =============================================================================

#[derive(Clone, Copy)]
struct BoxRef {
    typ: [u8; 4],
    payload_start: u64,
    payload_end: u64,
}

fn box_at(file: &File, offset: u64, limit: u64) -> Option<BoxRef> {
    if offset + 8 > limit {
        return None;
    }
    let mut head = [0u8; 16];
    let n = if offset + 16 <= limit { 16 } else { 8 };
    read_exact_at(file, &mut head[..n], offset).ok()?;
    let typ = [head[4], head[5], head[6], head[7]];
    let mut size = u32::from_be_bytes(head[0..4].try_into().unwrap()) as u64;
    let mut header = 8u64;
    if size == 1 {
        if n < 16 {
            return None;
        }
        size = u64::from_be_bytes(head[8..16].try_into().unwrap());
        header = 16;
    } else if size == 0 {
        size = limit - offset;
    }
    if size < header || offset + size > limit {
        return None;
    }
    Some(BoxRef { typ, payload_start: offset + header, payload_end: offset + size })
}

fn children(file: &File, start: u64, end: u64) -> Vec<BoxRef> {
    let mut out = Vec::new();
    let mut off = start;
    while off < end && out.len() < 100_000 {
        match box_at(file, off, end) {
            Some(b) => {
                off = b.payload_end;
                out.push(b);
            }
            None => break,
        }
    }
    out
}

fn find(list: &[BoxRef], typ: &[u8; 4]) -> Option<BoxRef> {
    list.iter().find(|b| &b.typ == typ).copied()
}

fn read_box(file: &File, b: &BoxRef) -> Result<Vec<u8>> {
    let len = (b.payload_end - b.payload_start) as usize;
    if len > 256 * 1024 * 1024 {
        return Err(Error::InvalidFormat("BRAW: sample table box is implausibly large".into()));
    }
    let mut v = vec![0u8; len];
    read_exact_at(file, &mut v, b.payload_start)?;
    Ok(v)
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|s| u16::from_be_bytes(s.try_into().unwrap()))
}

fn corrupt(what: &str) -> Error {
    Error::CorruptData(format!("BRAW: malformed '{what}' box"))
}

fn read_sample_sizes(d: &[u8]) -> Result<Vec<u32>> {
    let uniform = be32(d, 4).ok_or_else(|| corrupt("stsz"))?;
    let count = be32(d, 8).ok_or_else(|| corrupt("stsz"))? as usize;
    if uniform != 0 {
        return Ok(vec![uniform; count]);
    }
    (0..count).map(|i| be32(d, 12 + i * 4).ok_or_else(|| corrupt("stsz"))).collect()
}

fn read_chunk_offsets(d: &[u8], wide: bool) -> Result<Vec<u64>> {
    let count = be32(d, 4).ok_or_else(|| corrupt("stco"))? as usize;
    (0..count)
        .map(|i| {
            if wide {
                d.get(8 + i * 8..16 + i * 8)
                    .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
                    .ok_or_else(|| corrupt("co64"))
            } else {
                be32(d, 8 + i * 4).map(|v| v as u64).ok_or_else(|| corrupt("stco"))
            }
        })
        .collect()
}

/// `stsc` runs: (first_chunk, samples_per_chunk).
fn read_sample_to_chunk(d: &[u8]) -> Result<Vec<(u32, u32)>> {
    let count = be32(d, 4).ok_or_else(|| corrupt("stsc"))? as usize;
    (0..count)
        .map(|i| {
            let at = 8 + i * 12;
            Ok((
                be32(d, at).ok_or_else(|| corrupt("stsc"))?,
                be32(d, at + 4).ok_or_else(|| corrupt("stsc"))?,
            ))
        })
        .collect()
}

/// `stts` runs: (sample_count, sample_delta).
fn read_time_to_sample(d: &[u8]) -> Result<Vec<(u32, u32)>> {
    let count = be32(d, 4).ok_or_else(|| corrupt("stts"))? as usize;
    (0..count)
        .map(|i| {
            let at = 8 + i * 8;
            Ok((
                be32(d, at).ok_or_else(|| corrupt("stts"))?,
                be32(d, at + 4).ok_or_else(|| corrupt("stts"))?,
            ))
        })
        .collect()
}

fn sample_offsets(chunks: &[u64], stsc: &[(u32, u32)], sizes: &[u32]) -> Vec<u64> {
    let mut out = Vec::with_capacity(sizes.len());
    for (run, &(first, per_chunk)) in stsc.iter().enumerate() {
        let last = if run + 1 < stsc.len() { stsc[run + 1].0.saturating_sub(1) } else { chunks.len() as u32 };
        let mut chunk = first.max(1);
        while chunk <= last && (chunk as usize) <= chunks.len() {
            let mut off = chunks[chunk as usize - 1];
            for _ in 0..per_chunk {
                if out.len() >= sizes.len() {
                    return out;
                }
                out.push(off);
                off += sizes[out.len() - 1] as u64;
            }
            chunk += 1;
        }
    }
    out
}

// =============================================================================
// 'bmdf' metadata
// =============================================================================

fn read_bmdf_tags(bytes: &[u8]) -> BTreeMap<[u8; 4], &[u8]> {
    let mut tags = BTreeMap::new();
    let mut off = 8usize;
    while off + 8 <= bytes.len() && tags.len() < 4096 {
        let stride = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
        if stride < 8 || off + stride > bytes.len() {
            break;
        }
        let tag = [bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7]];
        tags.insert(tag, &bytes[off + 8..off + stride]);
        off += stride;
    }
    tags
}

fn tag_i32(tags: &BTreeMap<[u8; 4], &[u8]>, name: &[u8; 4]) -> Option<i32> {
    tags.get(name).filter(|v| v.len() >= 4).map(|v| i32::from_be_bytes(v[0..4].try_into().unwrap()))
}

fn tag_str(tags: &BTreeMap<[u8; 4], &[u8]>, name: &[u8; 4]) -> Option<String> {
    let v = tags.get(name)?;
    let end = v.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    let s = String::from_utf8_lossy(&v[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// First run of digits/dot in a string such as "f2.8" or "35mm".
fn leading_number(s: &str) -> Option<f64> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let rest = &s[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// =============================================================================
// 'braw' picture header
// =============================================================================

struct BrawHeader {
    nb_tiles_w: usize,
    nb_tiles_h: usize,
    width: usize,
    height: usize,
    tile_size_h: usize,
    tile_widths: Vec<usize>,
    /// Column-major, relative to the start of the 'braw' block.
    tile_offsets: Vec<usize>,
}

/// `frame` is the whole sample ('bmdf' included); 0x180 / 0x1180 are absolute
/// seeks from its start because a 'bmdf' block is always padded to 256 bytes.
fn read_braw_header(frame: &[u8], braw_start: usize) -> Option<BrawHeader> {
    let b = frame.get(braw_start..)?;
    if b.len() < 0x40 || &b[0..4] != b"braw" {
        return None;
    }
    if b[8] != 1 && b[8] != 2 {
        return None;
    }
    let nb_tiles_w = b[10] as usize;
    let nb_tiles_h = b[11] as usize;
    if nb_tiles_w == 0 || nb_tiles_h == 0 || nb_tiles_w * nb_tiles_h > 1024 {
        return None;
    }
    let width = be16(b, 12)? as usize;
    let height = be16(b, 14)? as usize;
    let tile_size_h = be16(b, 16)? as usize;
    if tile_size_h == 0 || tile_size_h & 7 != 0 {
        return None;
    }
    let mut tile_widths = Vec::with_capacity(nb_tiles_w);
    for i in 0..nb_tiles_w {
        let w = be16(b, 0x40 + i * 2)? as usize;
        if w == 0 || w % 16 != 0 {
            return None;
        }
        tile_widths.push(w);
    }
    // The last tile row may be shorter (e.g. 30 rows of 88 covering 2624 lines).
    if tile_widths.iter().sum::<usize>() != width
        || height > tile_size_h * nb_tiles_h
        || height <= tile_size_h * (nb_tiles_h - 1)
        || height % 8 != 0
    {
        return None;
    }
    let mut tile_offsets = Vec::with_capacity(nb_tiles_w * nb_tiles_h);
    for i in 0..nb_tiles_w * nb_tiles_h {
        tile_offsets.push(be32(frame, 0x180 + i * 4)? as usize);
    }
    Some(BrawHeader { nb_tiles_w, nb_tiles_h, width, height, tile_size_h, tile_widths, tile_offsets })
}

// =============================================================================
// Entropy coding tables
// =============================================================================

const DC_BITS: [u8; 16] = [2, 3, 3, 3, 3, 3, 5, 5, 6, 12, 12, 12, 12, 5, 12, 13];
const DC_CODES: [u32; 16] = [
    0x0, 0x2, 0x3, 0x4, 0x5, 0x6, 0x1C, 0x1D, 0x3E, 0xFF4, 0xFF5, 0xFF7, 0xFED, 0x1E, 0xFFE, 0x1FFE,
];
/// [extra bits, sign bits, index into DC_VAL_TAB]
const DC_TABLE: [[u8; 3]; 16] = [
    [0, 0, 15], [0, 1, 0], [1, 1, 1], [2, 1, 2], [3, 1, 3], [4, 1, 4], [5, 1, 5], [6, 1, 6],
    [7, 1, 7], [8, 1, 8], [9, 1, 9], [10, 1, 10], [11, 1, 11], [12, 1, 12], [13, 1, 13], [14, 1, 14],
];
const DC_VAL_TAB: [[i32; 16]; 2] = [
    [-1, -3, -7, -15, -31, -63, -127, -255, -511, -1023, -2047, -4095, -8191, -16383, -32767, 0],
    [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 0],
];

const AC_BITS: [u8; 194] = [
    2, 2, 3, 4, 4, 4, 5, 5, 5, 6, 6, 7, 7, 7, 7, 8, 8, 8, 9, 9, 9, 9, 9, 10, 10,
    10, 10, 10, 11, 11, 12, 12, 12, 12, 12, 12, 13, 13, 13, 13, 15, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 18, 18,
    18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
    18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18,
];
const AC_CODES: [u32; 194] = [
    0x000000, 0x000001, 0x000004, 0x00000B, 0x00000C, 0x00000A,
    0x00001A, 0x00001B, 0x00001C, 0x00003A, 0x00003B, 0x000078,
    0x000079, 0x00007A, 0x00007B, 0x0000F8, 0x0000F9, 0x0000FA,
    0x0001F6, 0x0001F7, 0x0001F8, 0x0001F9, 0x0001FA, 0x0003F6,
    0x0003F7, 0x0003F8, 0x0003F9, 0x0003FA, 0x0007F8, 0x0007F9,
    0x000FED, 0x000FF4, 0x000FF5, 0x000FF7, 0x000FEC, 0x000FF6,
    0x001FDC, 0x001FDD, 0x001FDE, 0x001FDF, 0x007FC0, 0x00FF84,
    0x00FF85, 0x00FF86, 0x00FF87, 0x00FF88, 0x00FF89, 0x00FF8A,
    0x00FF8B, 0x00FF8C, 0x00FF8D, 0x00FF8E, 0x00FF8F, 0x00FF90,
    0x00FF91, 0x00FF92, 0x00FF93, 0x00FF94, 0x00FF95, 0x00FF96,
    0x00FF97, 0x00FF98, 0x00FF99, 0x00FF9A, 0x00FF9B, 0x00FF9C,
    0x00FF9D, 0x00FF9E, 0x00FF9F, 0x00FFA0, 0x00FFA1, 0x00FFA2,
    0x00FFA3, 0x00FFA4, 0x00FFA5, 0x00FFA6, 0x00FFA7, 0x00FFA8,
    0x00FFA9, 0x00FFAA, 0x00FFAB, 0x00FFAC, 0x00FFAE, 0x00FFAF,
    0x00FFB0, 0x00FFB1, 0x00FFB2, 0x00FFB3, 0x00FFB4, 0x00FFB6,
    0x00FFB7, 0x00FFB8, 0x00FFB9, 0x00FFBA, 0x00FFBB, 0x00FFBC,
    0x00FFBE, 0x00FFBF, 0x00FFC0, 0x00FFC1, 0x00FFC2, 0x00FFC3,
    0x00FFC4, 0x00FFC5, 0x00FFC7, 0x00FFC8, 0x00FFC9, 0x00FFCA,
    0x00FFCB, 0x00FFCC, 0x00FFCD, 0x00FFCE, 0x00FFD0, 0x00FFD1,
    0x00FFD2, 0x00FFD3, 0x00FFD4, 0x00FFD5, 0x00FFD6, 0x00FFD7,
    0x00FFD9, 0x00FFDA, 0x00FFDB, 0x00FFDC, 0x00FFDD, 0x00FFDE,
    0x00FFDF, 0x00FFE0, 0x00FFE2, 0x00FFE3, 0x00FFE4, 0x00FFE5,
    0x00FFE6, 0x00FFE7, 0x00FFE8, 0x00FFE9, 0x00FFEB, 0x00FFEC,
    0x00FFED, 0x00FFEE, 0x00FFEF, 0x00FFF0, 0x00FFF1, 0x00FFF2,
    0x00FFF3, 0x00FFF5, 0x00FFF6, 0x00FFF7, 0x00FFF8, 0x00FFF9,
    0x00FFFA, 0x00FFFB, 0x00FFFC, 0x00FFFD, 0x03FEB5, 0x03FEB6,
    0x03FEB7, 0x03FED5, 0x03FED6, 0x03FED7, 0x03FEF5, 0x03FEF6,
    0x03FEF7, 0x03FF19, 0x03FEB4, 0x03FF1A, 0x03FF1B, 0x03FED4,
    0x03FF3D, 0x03FF3E, 0x03FEF4, 0x03FF3F, 0x03FF61, 0x03FF18,
    0x03FF62, 0x03FF63, 0x03FF3C, 0x03FF85, 0x03FF86, 0x03FF60,
    0x03FF87, 0x03FFA9, 0x03FF84, 0x03FFAA, 0x03FFAB, 0x03FFA8,
    0x03FFD1, 0x03FFD2, 0x03FFD0, 0x03FFD3, 0x03FFF9, 0x03FFF8,
    0x03FFFA, 0x03FFFB,
];
/// [zero-run to skip (255 = end of block), extra bits]
const AC_TABLE: [[u8; 2]; 194] = [
    [0, 1], [0, 2], [0, 3], [0, 4], [1, 1], [255, 0],
    [0, 5], [1, 2], [2, 1], [3, 1], [4, 1], [0, 6],
    [1, 3], [5, 1], [6, 1], [0, 7], [2, 2], [7, 1],
    [1, 4], [3, 2], [8, 1], [9, 1], [10, 1], [0, 8],
    [2, 3], [4, 2], [11, 1], [12, 1], [13, 1], [15, 0],
    [0, 12], [0, 9], [0, 10], [0, 11], [1, 5], [6, 2],
    [2, 4], [3, 3], [5, 2], [7, 2], [8, 2], [1, 6],
    [1, 7], [1, 8], [1, 9], [1, 10], [2, 5], [2, 6],
    [2, 7], [2, 8], [2, 9], [2, 10], [3, 4], [3, 5],
    [3, 6], [3, 7], [3, 8], [3, 9], [3, 10], [4, 3],
    [4, 4], [4, 5], [4, 6], [4, 7], [4, 8], [4, 9],
    [4, 10], [5, 3], [5, 4], [5, 5], [5, 6], [5, 7],
    [5, 8], [5, 9], [5, 10], [6, 3], [6, 4], [6, 5],
    [6, 6], [6, 7], [6, 8], [6, 9], [7, 3], [7, 4],
    [7, 5], [7, 6], [7, 7], [7, 8], [7, 9], [8, 3],
    [8, 4], [8, 5], [8, 6], [8, 7], [8, 8], [8, 9],
    [9, 2], [9, 3], [9, 4], [9, 5], [9, 6], [9, 7],
    [9, 8], [9, 9], [10, 2], [10, 3], [10, 4], [10, 5],
    [10, 6], [10, 7], [10, 8], [10, 9], [11, 2], [11, 3],
    [11, 4], [11, 5], [11, 6], [11, 7], [11, 8], [11, 9],
    [12, 2], [12, 3], [12, 4], [12, 5], [12, 6], [12, 7],
    [12, 8], [12, 9], [13, 2], [13, 3], [13, 4], [13, 5],
    [13, 6], [13, 7], [13, 8], [13, 9], [14, 1], [14, 2],
    [14, 3], [14, 4], [14, 5], [14, 6], [14, 7], [14, 8],
    [14, 9], [15, 1], [15, 2], [15, 3], [15, 4], [15, 5],
    [15, 6], [15, 7], [15, 8], [15, 9], [1, 11], [1, 12],
    [2, 11], [2, 12], [3, 11], [3, 12], [4, 11], [4, 12],
    [5, 11], [5, 12], [6, 10], [6, 11], [6, 12], [7, 10],
    [7, 11], [7, 12], [8, 10], [8, 11], [8, 12], [9, 10],
    [9, 11], [9, 12], [10, 10], [10, 11], [10, 12], [11, 10],
    [11, 11], [11, 12], [12, 10], [12, 11], [12, 12], [13, 10],
    [13, 11], [13, 12], [14, 10], [14, 11], [14, 12], [15, 10],
    [15, 11], [15, 12],
];

const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const ZIGZAG_8X4: [usize; 32] = [
    0, 1, 2, 3, 8, 9, 16, 17, 10, 11, 4, 5, 6, 7, 12, 13, 18,
    19, 24, 25, 26, 27, 20, 21, 14, 15, 22, 23, 28, 29, 30, 31,
];

/// Flat prefix lookup: (symbol, code length); symbol -1 = invalid.
struct Vlc {
    sym: Vec<i16>,
    len: Vec<u8>,
    bits: u32,
}

fn build_vlc(bits: &[u8], codes: &[u32], table_bits: u32) -> Vlc {
    let size = 1usize << table_bits;
    let mut sym = vec![-1i16; size];
    let mut len = vec![0u8; size];
    for (s, (&l, &c)) in bits.iter().zip(codes).enumerate() {
        let shift = table_bits - l as u32;
        let base = (c as usize) << shift;
        for k in 0..(1usize << shift) {
            sym[base + k] = s as i16;
            len[base + k] = l;
        }
    }
    Vlc { sym, len, bits: table_bits }
}

fn dc_vlc() -> &'static Vlc {
    static V: OnceLock<Vlc> = OnceLock::new();
    V.get_or_init(|| build_vlc(&DC_BITS, &DC_CODES, 13))
}

fn ac_vlc() -> &'static Vlc {
    static V: OnceLock<Vlc> = OnceLock::new();
    V.get_or_init(|| build_vlc(&AC_BITS, &AC_CODES, 18))
}

/// MSB-first bit reader; reads past the end yield zeros.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, buf: 0, bits: 0 }
    }

    #[inline]
    fn fill(&mut self) {
        while self.bits <= 56 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0) as u64;
            self.pos += 1;
            self.buf |= byte << (56 - self.bits);
            self.bits += 8;
        }
    }

    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.bits < n {
            self.fill();
        }
        (self.buf >> (64 - n)) as u32
    }

    #[inline]
    fn skip(&mut self, n: u32) {
        if self.bits < n {
            self.fill();
        }
        self.buf <<= n;
        self.bits -= n;
    }

    #[inline]
    fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }

    #[inline]
    fn vlc(&mut self, v: &Vlc) -> Result<usize> {
        let p = self.peek(v.bits) as usize;
        let s = v.sym[p];
        if s < 0 {
            return Err(Error::CorruptData("BRAW: invalid VLC code in tile bitstream".into()));
        }
        self.skip(v.len[p] as u32);
        Ok(s as usize)
    }
}

/// Decodes one block's coefficients (dequantised) into `dst[..max]`.
#[inline]
fn decode_block(
    r: &mut BitReader,
    quant: &[i64],
    prev_dc: &mut i32,
    max: usize,
    add: i64,
    scan: &[usize],
    dst: &mut [f32],
) -> Result<NonZero> {
    dst[..max].fill(0.0);
    let dc = DC_TABLE[r.vlc(dc_vlc())?];
    let sgn = r.get(dc[1] as u32) as usize;
    let code = r.get(dc[0] as u32) as i32;
    let val = code + *prev_dc + DC_VAL_TAB[sgn][dc[2] as usize];
    *prev_dc = val;
    dst[0] = ((((val as i64) * quant[0] + 0x8000) >> 16) + add).min(32767) as f32;

    let mut nz = NonZero { rows: 1, cols: 1 };
    let mut i = 0usize;
    loop {
        let ac = AC_TABLE[r.vlc(ac_vlc())?];
        if ac[0] == 255 {
            break;
        }
        let len = ac[1] as u32;
        let mut v = r.get(len) as i64;
        i += 1 + ac[0] as usize;
        if i >= max {
            return Err(Error::CorruptData("BRAW: AC run past the end of its block".into()));
        }
        if len > 0 && v < (1i64 << (len - 1)) {
            v -= (1i64 << len) - 1;
        }
        let z = scan[i];
        let c = ((v * quant[z] + 0x8000) >> 16) as f32;
        dst[z] = c;
        if c != 0.0 {
            nz.rows |= 1 << (z >> 3);
            nz.cols |= 1 << (z & 7);
        }
    }
    Ok(nz)
}

/// Bitmasks of coefficient rows (v) and columns (u) that hold a non-zero value.
#[derive(Clone, Copy)]
struct NonZero {
    rows: u8,
    cols: u8,
}

// =============================================================================
// IDCT (separable float, per-dimension scale 0.5 — see braw.js idctScale)
// =============================================================================

struct CosTables {
    c8: [f32; 64],
    /// `c8` transposed: `c8t[u * 8 + x] == c8[x * 8 + u]`.
    c8t: [f32; 64],
    c4: [f32; 16],
}

fn cos_tables() -> &'static CosTables {
    static T: OnceLock<CosTables> = OnceLock::new();
    T.get_or_init(|| {
        let basis = |n: usize, out: &mut [f32]| {
            for x in 0..n {
                for u in 0..n {
                    let c = (((2 * x + 1) * u) as f64 * std::f64::consts::PI / (2 * n) as f64).cos();
                    let norm = if u == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
                    // fold C0 and the 0.5 scale into the table
                    out[x * n + u] = (c * norm * 0.5) as f32;
                }
            }
        };
        let mut c8 = [0f32; 64];
        let mut c4 = [0f32; 16];
        basis(8, &mut c8);
        basis(4, &mut c4);
        let mut c8t = [0f32; 64];
        for x in 0..8 {
            for u in 0..8 {
                c8t[u * 8 + x] = c8[x * 8 + u];
            }
        }
        CosTables { c8, c8t, c4 }
    })
}

/// 8 wide, `h` (8 or 4) tall. Columns first, then rows — same order as braw.js.
///
/// Bit-identical to the plain triple loop: every sum is accumulated in the same
/// ascending order, and the terms skipped via `nz` are exact zeros (`s + 0.0 == s`).
/// The loops are ordered so the innermost one runs over 8 contiguous floats (NEON/SSE).
#[inline]
fn idct_8xh(coeffs: &[f32; 64], h: usize, nz: NonZero, out: &mut [f32; 64]) {
    let t = cos_tables();
    let ch: &[f32] = if h == 8 { &t.c8 } else { &t.c4 };

    // Pass 1 (columns): tmp[y][col] = sum_v coeffs[v][col] * ch[y][v]
    let mut tmp = [0f32; 64];
    for y in 0..h {
        let mut acc = [0f32; 8];
        for v in 0..h {
            if nz.rows & (1 << v) == 0 {
                continue;
            }
            let c = ch[y * h + v];
            let src: &[f32; 8] = coeffs[v * 8..v * 8 + 8].try_into().unwrap();
            for col in 0..8 {
                acc[col] += src[col] * c;
            }
        }
        tmp[y * 8..y * 8 + 8].copy_from_slice(&acc);
    }

    // Pass 2 (rows): out[row][x] = sum_u tmp[row][u] * c8[x][u]
    for row in 0..h {
        let mut acc = [0f32; 8];
        for u in 0..8 {
            if nz.cols & (1 << u) == 0 {
                continue;
            }
            let v = tmp[row * 8 + u];
            let basis: &[f32; 8] = t.c8t[u * 8..u * 8 + 8].try_into().unwrap();
            for x in 0..8 {
                acc[x] += v * basis[x];
            }
        }
        out[row * 8..row * 8 + 8].copy_from_slice(&acc);
    }
}

#[inline]
fn clamp12(v: f32) -> u16 {
    v.clamp(0.0, 4095.0) as u16
}

#[inline]
fn round_half_up(v: f32) -> f32 {
    (v + 0.5).floor()
}

// =============================================================================
// Frame decode
// =============================================================================

struct Quant {
    q0: [i64; 64],
    q1: [i64; 32],
}

fn read_quant(data: &[u8], bmdf_len: usize) -> Result<Quant> {
    let err = || Error::CorruptData("BRAW: quantiser tables out of range".into());
    let qscale0 = *data.get(bmdf_len + 9).ok_or_else(err)? as i64;
    let mut q0 = [0i64; 64];
    let scale0 = (qscale0 + 4) * 2048;
    for (n, q) in q0.iter_mut().enumerate() {
        *q = be16(data, 0x1180 + n * 2).ok_or_else(err)? as i64 * scale0;
    }
    let dcq0 = be16(data, bmdf_len + 32).ok_or_else(err)? as i64;
    if q0[0] <= dcq0 {
        q0[0] = dcq0;
    }
    let mut q1 = [0i64; 32];
    let half = (((qscale0 + 4) as f64) * 0.5 + 0.5 + 0.5).floor() as i64; // JS Math.round
    let scale1 = half.max(4) * 2048;
    for (n, q) in q1.iter_mut().enumerate() {
        *q = be16(data, 0x1180 + 128 + n * 2).ok_or_else(err)? as i64 * scale1;
    }
    let dcq1 = be16(data, bmdf_len + 34).ok_or_else(err)? as i64;
    if q1[0] <= dcq1 {
        q1[0] = dcq1;
    }
    Ok(Quant { q0, q1 })
}

/// Decodes one tile into its own `tile_w * tile_h` Bayer buffer.
fn decode_tile(bits: &[u8], q: &Quant, tile_w: usize, tile_h: usize, out: &mut [u16]) -> Result<()> {
    let mut r = BitReader::new(bits);
    let mut prev_dc = [0i32; 3];
    let mut b0 = [0f32; 64];
    let mut b1 = [0f32; 64];
    let mut b2 = [0f32; 64];
    let mut b3 = [0f32; 64];
    let mut i0 = [0f32; 64];
    let mut i1 = [0f32; 64];
    let mut i2 = [0f32; 64];
    let mut i3 = [0f32; 64];

    for by in 0..tile_h / 8 {
        let py = by * 8;
        for bx in 0..tile_w / 16 {
            let px = bx * 16;
            let n0 = decode_block(&mut r, &q.q0, &mut prev_dc[0], 64, 16384, &ZIGZAG_8X8, &mut b0)?;
            let n1 = decode_block(&mut r, &q.q0, &mut prev_dc[0], 64, 16384, &ZIGZAG_8X8, &mut b1)?;
            let n2 = decode_block(&mut r, &q.q1, &mut prev_dc[1], 32, 8192, &ZIGZAG_8X4, &mut b2)?;
            let n3 = decode_block(&mut r, &q.q1, &mut prev_dc[2], 32, 8192, &ZIGZAG_8X4, &mut b3)?;
            r.skip(8); // per-macroblock byte-alignment stuffing

            idct_8xh(&b0, 8, n0, &mut i0);
            idct_8xh(&b1, 8, n1, &mut i1);
            idct_8xh(&b2, 4, n2, &mut i2);
            idct_8xh(&b3, 4, n3, &mut i3);

            // Base picture: two 8x8 halves.
            for y in 0..8 {
                let row = (py + y) * tile_w + px;
                for x in 0..8 {
                    out[row + x] = clamp12(round_half_up(i0[y * 8 + x]));
                    out[row + 8 + x] = clamp12(round_half_up(i1[y * 8 + x]));
                }
            }
            // Decorrelation: block3 onto R sites, block2 onto B sites, midpoint 1024.
            for y in 0..4 {
                for x in 0..8 {
                    let a = (py + y * 2) * tile_w + px + x * 2;
                    let b = (py + y * 2 + 1) * tile_w + px + x * 2 + 1;
                    out[a] = clamp12(out[a] as f32 + i3[y * 8 + x] - 1024.0);
                    out[b] = clamp12(out[b] as f32 + i2[y * 8 + x] - 1024.0);
                }
            }
        }
    }
    Ok(())
}

/// Decodes one BRAW sample (the whole sample, 'bmdf' included) to 12-bit RGGB samples.
pub fn decode_braw_frame_to_u16(data: &[u8], width: usize, height: usize) -> Result<Vec<u16>> {
    let bmdf_len = be32(data, 0).ok_or_else(|| Error::CorruptData("BRAW: empty frame".into()))? as usize;
    let h = read_braw_header(data, bmdf_len)
        .ok_or_else(|| Error::CorruptData("BRAW: picture header could not be read".into()))?;
    if h.width != width || h.height != height {
        return Err(Error::CorruptData(format!(
            "BRAW: frame is {}x{}, clip is {width}x{height}",
            h.width, h.height
        )));
    }
    let q = read_quant(data, bmdf_len)?;
    let braw = &data[bmdf_len..];

    // Tiles are independent bitstreams: decode them in parallel.
    struct Job {
        x0: usize,
        y0: usize,
        w: usize,
        h: usize,
        start: usize,
        end: usize,
    }
    let mut jobs = Vec::with_capacity(h.tile_offsets.len());
    let mut x0 = 0;
    for tx in 0..h.nb_tiles_w {
        for ty in 0..h.nb_tiles_h {
            let idx = tx * h.nb_tiles_h + ty;
            let start = h.tile_offsets[idx];
            let end = h.tile_offsets.get(idx + 1).copied().unwrap_or(braw.len()).min(braw.len());
            if start > end {
                return Err(Error::CorruptData("BRAW: tile offsets out of order".into()));
            }
            let y0 = ty * h.tile_size_h;
            let th = h.tile_size_h.min(h.height - y0);
            jobs.push(Job { x0, y0, w: h.tile_widths[tx], h: th, start, end });
        }
        x0 += h.tile_widths[tx];
    }

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(jobs.len()).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out = vec![0u16; width * height];
    let out_ptr = SendPtr(out.as_mut_ptr());

    std::thread::scope(|s| -> Result<()> {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (jobs, next, q, out_ptr) = (&jobs, &next, &q, &out_ptr);
                s.spawn(move || -> Result<()> {
                    let mut tile = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(j) = jobs.get(i) else { return Ok(()) };
                        tile.clear();
                        tile.resize(j.w * j.h, 0u16);
                        decode_tile(&braw[j.start..j.end], q, j.w, j.h, &mut tile)?;
                        for y in 0..j.h {
                            // SAFETY: tiles cover disjoint rectangles of `out`.
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    tile.as_ptr().add(y * j.w),
                                    out_ptr.0.add((j.y0 + y) * width + j.x0),
                                    j.w,
                                );
                            }
                        }
                    }
                })
            })
            .collect();
        for hdl in handles {
            hdl.join().map_err(|_| Error::CorruptData("BRAW: decoder thread panicked".into()))??;
        }
        Ok(())
    })?;
    Ok(out)
}

struct SendPtr(*mut u16);
unsafe impl Sync for SendPtr {}
unsafe impl Send for SendPtr {}

/// Same as [`decode_braw_frame_to_u16`] but writes little-endian bytes into `dst`.
pub fn decode_braw_frame_to_u16_le(src: &[u8], width: usize, height: usize, dst: &mut [u8]) -> Result<()> {
    let px = decode_braw_frame_to_u16(src, width, height)?;
    if dst.len() < px.len() * 2 {
        return Err(Error::InvalidFormat("Destination buffer too small for decoded BRAW frame".into()));
    }
    for (d, v) in dst.chunks_exact_mut(2).zip(&px) {
        d.copy_from_slice(&v.to_le_bytes());
    }
    Ok(())
}

/// Grey-world WB gains `[r, g, b]` (g = 1) from a decoded RGGB frame.
fn gray_world_gains(bayer: &[u16], width: usize, height: usize) -> [f32; 3] {
    let bl = BRAW_BLACK_LEVEL as f64;
    let mx = (width as f64 * 0.05) as usize & !1;
    let my = (height as f64 * 0.05) as usize & !1;
    let (mut sr, mut sg, mut sb, mut n) = (0f64, 0f64, 0f64, 0u64);
    let mut y = my;
    while y + 1 < height.saturating_sub(my) {
        let mut x = mx;
        while x + 1 < width.saturating_sub(mx) {
            let p = |xx: usize, yy: usize| (bayer[yy * width + xx] as f64 - bl).max(0.0);
            let g = p(x + 1, y) + p(x, y + 1);
            if g >= 512.0 {
                sr += p(x, y);
                sg += g;
                sb += p(x + 1, y + 1);
                n += 1;
            }
            x += 32;
        }
        y += 32;
    }
    if n == 0 {
        return [1.0, 1.0, 1.0];
    }
    let mg = sg / (2 * n) as f64;
    let c = |v: f64| v.clamp(0.3, 4.5) as f32;
    [c(mg / (sr / n as f64).max(1.0)), 1.0, c(mg / (sb / n as f64).max(1.0))]
}

// =============================================================================
// Opening a clip
// =============================================================================

/// Opens a Blackmagic RAW `.braw` file and indexes its frames into an `MlvClip`.
pub fn open_braw_clip(primary_path: impl AsRef<Path>) -> Result<MlvClip> {
    let primary_path = primary_path.as_ref().to_path_buf();
    let base_name = primary_path.file_stem().and_then(|s| s.to_str()).unwrap_or("BRAW").to_string();
    let file = File::open(&primary_path)?;
    let file_len = file.metadata()?.len();

    let top = children(&file, 0, file_len);
    let moov = find(&top, b"moov").ok_or_else(|| Error::InvalidFormat("BRAW: no 'moov' box".into()))?;

    let mut video: Option<(BoxRef, BoxRef, [u8; 4])> = None; // (mdia, stbl, fourcc)
    for trak in children(&file, moov.payload_start, moov.payload_end).iter().filter(|b| &b.typ == b"trak") {
        let Some(mdia) = find(&children(&file, trak.payload_start, trak.payload_end), b"mdia") else { continue };
        let mdia_kids = children(&file, mdia.payload_start, mdia.payload_end);
        let Some(hdlr) = find(&mdia_kids, b"hdlr") else { continue };
        if read_box(&file, &hdlr)?.get(8..12) != Some(b"vide") {
            continue;
        }
        let Some(minf) = find(&mdia_kids, b"minf") else { continue };
        let Some(stbl) = find(&children(&file, minf.payload_start, minf.payload_end), b"stbl") else { continue };
        let Some(stsd) = find(&children(&file, stbl.payload_start, stbl.payload_end), b"stsd") else { continue };
        let d = read_box(&file, &stsd)?;
        let Some(fcc) = d.get(12..16) else { continue };
        let fourcc: [u8; 4] = fcc.try_into().unwrap();
        if BRAW_FOURCC.iter().any(|f| **f == fourcc) {
            video = Some((mdia, stbl, fourcc));
            break;
        }
    }
    let (mdia, stbl, fourcc) =
        video.ok_or_else(|| Error::InvalidFormat("BRAW: no Blackmagic RAW video track".into()))?;

    let mdia_kids = children(&file, mdia.payload_start, mdia.payload_end);
    let timescale = match find(&mdia_kids, b"mdhd") {
        Some(b) => {
            let d = read_box(&file, &b)?;
            if d.first() == Some(&1) { be32(&d, 20) } else { be32(&d, 12) }.unwrap_or(0)
        }
        None => 0,
    };

    let kids = children(&file, stbl.payload_start, stbl.payload_end);
    let need = |t: &[u8; 4]| find(&kids, t);
    let (Some(stsz), Some(stsc), Some(stts)) = (need(b"stsz"), need(b"stsc"), need(b"stts")) else {
        return Err(Error::InvalidFormat("BRAW: video track is missing a sample table".into()));
    };
    let (co, wide) = match (need(b"co64"), need(b"stco")) {
        (Some(b), _) => (b, true),
        (None, Some(b)) => (b, false),
        _ => return Err(Error::InvalidFormat("BRAW: video track has no chunk offsets".into())),
    };
    let sizes = read_sample_sizes(&read_box(&file, &stsz)?)?;
    let offsets = sample_offsets(
        &read_chunk_offsets(&read_box(&file, &co)?, wide)?,
        &read_sample_to_chunk(&read_box(&file, &stsc)?)?,
        &sizes,
    );
    let tts = read_time_to_sample(&read_box(&file, &stts)?)?;
    if offsets.is_empty() {
        return Err(Error::InvalidFormat("BRAW: video track has no samples".into()));
    }

    // Frame rate from the (normally single) stts run.
    let delta = tts.first().map_or(0, |r| r.1);
    let (fps_nom, fps_denom) = if timescale > 0 && delta > 0 { (timescale, delta) } else { (24000, 1000) };

    let mut frames = Vec::with_capacity(offsets.len());
    let mut ticks = 0u64;
    let mut run = 0usize;
    let mut left_in_run = tts.first().map_or(0, |r| r.0);
    for (i, (&off, &size)) in offsets.iter().zip(&sizes).enumerate() {
        if off + size as u64 > file_len {
            break; // truncated recording: keep what is complete
        }
        let timestamp_us = if timescale > 0 { ticks * 1_000_000 / timescale as u64 } else { 0 };
        frames.push(MlvFrameEntry {
            chunk_idx: 0,
            frame_number: i as u32,
            timestamp_us,
            payload_offset: off,
            payload_size: size,
            meta_idx: 0,
        });
        while left_in_run == 0 && run + 1 < tts.len() {
            run += 1;
            left_in_run = tts[run].0;
        }
        ticks += tts.get(run).map_or(delta, |r| r.1) as u64;
        left_in_run = left_in_run.saturating_sub(1);
    }
    if frames.is_empty() {
        return Err(Error::InvalidFormat("BRAW: no complete frames".into()));
    }

    // First frame: metadata, picture header, grey-world WB.
    let mut first = vec![0u8; frames[0].payload_size as usize];
    read_exact_at(&file, &mut first, frames[0].payload_offset)?;
    let bmdf_len = be32(&first, 0).unwrap_or(0) as usize;
    if first.get(4..8) != Some(b"bmdf") || bmdf_len > first.len() {
        return Err(Error::InvalidFormat("BRAW: first frame has no 'bmdf' metadata block".into()));
    }
    let header = read_braw_header(&first, bmdf_len).ok_or_else(|| {
        Error::InvalidFormat("BRAW: picture essence is not laid out the way this reader expects".into())
    })?;
    let (width, height) = (header.width, header.height);

    let tags = read_bmdf_tags(&first[..bmdf_len]);
    let mut meta0 = FrameDynamicMeta::default();
    if let Some(iso) = tag_i32(&tags, b"isoe").filter(|v| *v > 0) {
        meta0.iso_value = iso as u32;
    }
    if let Some(k) = tag_i32(&tags, b"wkel").filter(|v| *v > 0) {
        meta0.wb_kelvin = k as u32;
    }
    let aperture = tag_str(&tags, b"aptr").and_then(|s| leading_number(&s));
    let focal = tag_str(&tags, b"fcln").and_then(|s| leading_number(&s));
    if let Some(f) = focal {
        meta0.focal_length_mm = f.round() as u16;
    }
    if let Some(a) = aperture {
        meta0.aperture_x100 = (a * 100.0).round() as u16;
    }
    meta0.lens_name = tag_str(&tags, b"lnsm").unwrap_or_else(|| match (focal, aperture) {
        (Some(f), Some(a)) => format!("{f:.0}mm f/{a:.1}"),
        _ => String::new(),
    });
    if let Some(angle) = tag_str(&tags, b"shtv").and_then(|s| leading_number(&s)) {
        let fps = fps_nom as f64 / fps_denom as f64;
        if angle > 0.0 && fps > 0.0 {
            meta0.shutter_us = ((angle / 360.0) / fps * 1_000_000.0).round().max(1.0) as u64;
        }
    }
    // An unbalanced picture is a better failure than an unopenable clip.
    if let Ok(bayer) = decode_braw_frame_to_u16(&first, width, height) {
        let [r, g, b] = gray_world_gains(&bayer, width, height);
        meta0.wb_gain_r = r;
        meta0.wb_gain_g = g;
        meta0.wb_gain_b = b;
    }

    let camera_name = tag_str(&tags, b"cmdl")
        .map(|m| if m.to_ascii_lowercase().contains("blackmagic") { m } else { format!("Blackmagic {m}") })
        .unwrap_or_else(|| String::from("Blackmagic"));

    let mut block_counts = BTreeMap::new();
    block_counts.insert(String::from("BRAW_VID"), frames.len());

    Ok(MlvClip {
        base_name,
        chunk_paths: vec![primary_path],
        chunks: vec![file],
        version_string: format!("BRAW ({}) experimental", String::from_utf8_lossy(&fourcc)),
        video_guid: 0,
        video_class: 0,
        audio_class: 0,
        fps_nom,
        fps_denom,
        width: width as u32,
        height: height as u32,
        bits_per_pixel: 12,
        black_level: BRAW_BLACK_LEVEL,
        white_level: BRAW_WHITE_LEVEL,
        levels: crate::levels::ResolvedLevels::from_file(BRAW_BLACK_LEVEL, BRAW_WHITE_LEVEL),
        focus_fix: None,
        active_area: [0, 0, height as i32, width as i32],
        cfa_pattern: CfaPattern::Rggb,
        camera_name,
        camera_model_id: 0,
        rotation_degrees: 0,
        calibration_illuminant1: ILLUMINANT_D65,
        color_matrix1: BMD_WIDE_GAMUT_XYZ_TO_CAM,
        color_matrix1_source: "BRAW BMD Wide Gamut (approx.)",
        calibration_illuminant2: None,
        color_matrix2: None,
        forward_matrix1: None,
        forward_matrix2: None,
        rtci: RtciTime::default(),
        is_lj92: false,
        is_mcraw: false,
        is_braw: true,
        mcraw_compression_type: 0,
        mcraw_stored_format: 0,
        lj92_info: None,
        dynamic_metas: vec![meta0],
        frames,
        audio_info: None,
        audio_entries: Vec::new(),
        block_counts,
        rawc_sampling: None,
        stretch_y: 1.0,
        stretch_source: "none",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_prefix_free(bits: &[u8], codes: &[u32]) {
        for i in 0..bits.len() {
            for j in 0..bits.len() {
                if i == j || bits[i] > bits[j] {
                    continue;
                }
                let shift = bits[j] - bits[i];
                assert_ne!(codes[j] >> shift, codes[i], "code {i} is a prefix of code {j}");
            }
        }
    }

    #[test]
    fn huffman_tables_are_prefix_free_and_aligned() {
        assert_eq!(AC_BITS.len(), AC_CODES.len());
        assert_eq!(AC_BITS.len(), AC_TABLE.len());
        assert_prefix_free(&DC_BITS, &DC_CODES);
        assert_prefix_free(&AC_BITS, &AC_CODES);
    }

    /// Decodes the first frame of the clip named by `CINEFS_BRAW_SAMPLE`, if set.
    #[test]
    fn decodes_sample_clip_when_available() {
        let Ok(path) = std::env::var("CINEFS_BRAW_SAMPLE") else { return };
        let clip = open_braw_clip(&path).expect("open");
        let px = crate::focus_probe::decode_frame_u16(&clip, 0).expect("decode");
        assert_eq!(px.len(), (clip.width * clip.height) as usize);
        let zeros = px.iter().filter(|&&v| v == 0).count();
        assert!(zeros * 100 < px.len(), "too many zero samples: {zeros}");
    }
}
