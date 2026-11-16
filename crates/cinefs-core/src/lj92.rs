//! Lossless JPEG (ITU-T T.81 Process 14 / LJ92) header inspector and decoder.
//!
//! CineFS uses this module in two ways:
//! 1. **Zero-decode Passthrough Check (`inspect_lj92`)**: Reads only the first ~128 bytes
//!    of a `VIDF` payload to parse the `SOF3` and `SOS` markers. If the stream is a
//!    2-component (`frame_w * 2 == width`, `frame_h == height`) predictor-1 Lossless JPEG,
//!    it is 100% compliant with Adobe DNG `Compression = 7` and can be served directly
//!    from the `.MLV` file with zero pixel decoding!
//! 2. **Software Decoder (`decode_lj92_to_u16_le`)**: Fast 12-bit L1 lookup-table + 16-bit
//!    fallback decoder for `Unpacked16` export mode or non-standard Magic Lantern LJ92
//!    streams whose `SOF3` geometry differs from DNG's 2-component convention.

use crate::{Error, Result};

/// Parsed metadata from an LJ92 (Lossless JPEG) bitstream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lj92HeaderInfo {
    pub bits: u8,
    pub sof_width: u16,
    pub sof_height: u16,
    pub components: u8,
    pub predictor: u8,
    pub point_transform: u8,
}

impl Lj92HeaderInfo {
    /// Returns `true` if this LJ92 stream can be wrapped directly as a single-strip
    /// DNG `Compression = 7` payload for a Bayer frame of `raw_width x raw_height`.
    ///
    /// DNG `Compression = 7` for CFA requires 2 interleaved components of half the
    /// raw width (`sof_width * 2 == raw_width`, `sof_height == raw_height`), predictor 1,
    /// and point transform 0.
    pub fn is_dng_cfa_passthrough_compatible(&self, raw_width: u32, raw_height: u32) -> bool {
        self.components == 2
            && (self.sof_width as u32) * 2 == raw_width
            && (self.sof_height as u32) == raw_height
            && self.predictor == 1
            && self.point_transform == 0
    }
}

/// Inspects the JPEG markers at the start of `data` without decoding entropy data.
pub fn inspect_lj92(data: &[u8]) -> Result<Lj92HeaderInfo> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(Error::CorruptData("LJ92 stream missing SOI (0xFFD8)".into()));
    }
    let mut p = 2usize;
    let mut bits = 0u8;
    let mut sof_height = 0u16;
    let mut sof_width = 0u16;
    let mut components = 0u8;
    let mut have_sof3 = false;

    while p + 4 <= data.len() {
        if data[p] != 0xFF {
            return Err(Error::CorruptData(format!(
                "LJ92 expected marker 0xFF at offset {p}, got 0x{:02X}",
                data[p]
            )));
        }
        let marker = data[p + 1];
        if marker == 0xFF {
            p += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0xD9 {
            p += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
        if seg_len < 2 || p + 2 + seg_len > data.len() {
            return Err(Error::CorruptData("LJ92 truncated marker segment".into()));
        }
        let seg = &data[p + 4..p + 2 + seg_len];
        match marker {
            0xC3 => {
                if seg.len() < 6 {
                    return Err(Error::CorruptData("LJ92 SOF3 segment too short".into()));
                }
                bits = seg[0];
                sof_height = u16::from_be_bytes([seg[1], seg[2]]);
                sof_width = u16::from_be_bytes([seg[3], seg[4]]);
                components = seg[5];
                have_sof3 = true;
            }
            0xDA => {
                if !have_sof3 {
                    return Err(Error::CorruptData("LJ92 SOS before SOF3".into()));
                }
                let sos_comps = seg[0] as usize;
                let pred_idx = 1 + 2 * sos_comps;
                if seg.len() < pred_idx + 3 {
                    return Err(Error::CorruptData("LJ92 SOS segment too short".into()));
                }
                let predictor = seg[pred_idx];
                let point_transform = seg[pred_idx + 2] & 0x0F;
                return Ok(Lj92HeaderInfo {
                    bits,
                    sof_width,
                    sof_height,
                    components,
                    predictor,
                    point_transform,
                });
            }
            _ => {}
        }
        p += 2 + seg_len;
    }

    Err(Error::CorruptData("LJ92 SOS marker not found".into()))
}

const FAST_BITS: usize = 12;

#[derive(Clone, Copy, Default)]
struct FastEntry {
    diff: i16,
    len: u8, // bits consumed; 0 = code longer than FAST_BITS
    sym: u8, // 0xFF = `diff` is complete (code + extra bits fit in FAST_BITS)
}

#[derive(Clone)]
struct HuffLut {
    lut16: Vec<u16>,
    fast: Vec<FastEntry>,
}

impl HuffLut {
    fn build(counts: &[u8; 17], values: &[u8]) -> Result<Self> {
        let mut lut16 = vec![0u16; 1 << 16];
        let mut code = 0u32;
        let mut k = 0usize;
        for len in 1..=16usize {
            let cnt = counts[len] as usize;
            for _ in 0..cnt {
                if code >= (1u32 << len) || k >= values.len() {
                    return Err(Error::CorruptData("Invalid LJ92 Huffman table".into()));
                }
                let first = (code << (16 - len)) as usize;
                let span = 1usize << (16 - len);
                let entry = ((len as u16) << 8) | (values[k] as u16);
                lut16[first..first + span].fill(entry);
                k += 1;
                code += 1;
            }
            code <<= 1;
        }

        let mut fast = vec![FastEntry::default(); 1 << FAST_BITS];
        for (w, slot) in fast.iter_mut().enumerate() {
            let entry = lut16[w << (16 - FAST_BITS)];
            let len = (entry >> 8) as usize;
            let sym = (entry & 0xFF) as usize;
            if len > 0 && len <= FAST_BITS {
                slot.len = len as u8;
                slot.sym = sym as u8;
                if sym == 0 {
                    slot.sym = 0xFF;
                    slot.diff = 0;
                } else if sym < 16 && len + sym <= FAST_BITS {
                    let v = ((w >> (FAST_BITS - len - sym)) & ((1usize << sym) - 1)) as i32;
                    let half = 1i32 << (sym - 1);
                    let diff = if v < half { v - ((1i32 << sym) - 1) } else { v };
                    slot.diff = diff as i16;
                    slot.len = (len + sym) as u8;
                    slot.sym = 0xFF;
                }
            }
        }

        Ok(Self { lut16, fast })
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    #[inline]
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            acc: 0,
            bits: 0,
        }
    }

    #[inline]
    fn fill(&mut self) {
        // Fast 64-bit refill when the next 8 bytes contain no 0xFF byte.
        if self.bits <= 56 && self.pos + 8 <= self.data.len() {
            let chunk: [u8; 8] = self.data[self.pos..self.pos + 8].try_into().unwrap();
            let v = u64::from_be_bytes(chunk);
            let inv = !v;
            let has_ff = (inv.wrapping_sub(0x0101_0101_0101_0101) & !inv & 0x8080_8080_8080_8080) != 0;
            if !has_ff {
                let take = ((64 - self.bits) >> 3) as usize;
                let keep = if take == 8 {
                    u64::MAX
                } else {
                    !(!0u64 >> (take * 8))
                };
                self.acc |= (v & keep) >> self.bits;
                self.pos += take;
                self.bits += (take * 8) as u32;
                return;
            }
        }

        while self.bits <= 56 {
            let mut byte = 0u8;
            if self.pos < self.data.len() {
                let b = self.data[self.pos];
                if b == 0xFF {
                    if self.pos + 1 < self.data.len() && self.data[self.pos + 1] == 0 {
                        byte = 0xFF;
                        self.pos += 2;
                    } else {
                        // Marker reached (e.g. EOI 0xFFD9): feed zeros.
                        byte = 0;
                    }
                } else {
                    byte = b;
                    self.pos += 1;
                }
            }
            self.acc |= (byte as u64) << (56 - self.bits);
            self.bits += 8;
        }
    }

    #[inline(always)]
    fn next_diff(&mut self, huff: &HuffLut) -> Result<i32> {
        if self.bits < 32 {
            self.fill();
        }
        let fast_idx = (self.acc >> (64 - FAST_BITS)) as usize;
        let f = huff.fast[fast_idx];
        if f.sym == 0xFF {
            self.acc <<= f.len;
            self.bits -= f.len as u32;
            return Ok(f.diff as i32);
        }
        let (len, s) = if f.len != 0 {
            (f.len as u32, f.sym as u32)
        } else {
            let entry = huff.lut16[(self.acc >> 48) as usize];
            let l = (entry >> 8) as u32;
            if l == 0 {
                return Err(Error::CorruptData("Invalid LJ92 Huffman code in scan".into()));
            }
            (l, (entry & 0xFF) as u32)
        };
        self.acc <<= len;
        self.bits -= len;
        if s == 16 {
            Ok(32768)
        } else if s > 0 {
            let v = (self.acc >> (64 - s)) as i32;
            self.acc <<= s;
            self.bits -= s;
            let half = 1i32 << (s - 1);
            Ok(if v < half { v - ((1i32 << s) - 1) } else { v })
        } else {
            Ok(0)
        }
    }
}

/// Decodes an LJ92 (Lossless JPEG Process 14, Predictor 1) frame into little-endian `u16` samples.
///
/// Supports both 2-component CFA streams (used by Magic Lantern, MLV App, and DNG) and
/// generic `N`-component predictor-1 streams (used by some Canon Magic Lantern builds).
pub fn decode_lj92_to_u16_le(data: &[u8], expected_pixels: usize, dst_u16_le: &mut [u8]) -> Result<()> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(Error::CorruptData("LJ92 missing SOI marker".into()));
    }
    let mut p = 2usize;
    let mut bits = 0u8;
    let mut sof_h = 0usize;
    let mut sof_w = 0usize;
    let mut comps = 0usize;
    let mut huff_tables: [Option<HuffLut>; 4] = [None, None, None, None];
    let mut comp_table_idx = [0usize; 4];
    let mut scan_offset = 0usize;

    while p + 4 <= data.len() {
        if data[p] != 0xFF {
            return Err(Error::CorruptData("LJ92 invalid marker prefix".into()));
        }
        let marker = data[p + 1];
        if marker == 0xFF {
            p += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0xD9 {
            p += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([data[p + 2], data[p + 3]]) as usize;
        if seg_len < 2 || p + 2 + seg_len > data.len() {
            return Err(Error::CorruptData("LJ92 segment out of bounds".into()));
        }
        let seg = &data[p + 4..p + 2 + seg_len];
        match marker {
            0xC3 => {
                bits = seg[0];
                sof_h = u16::from_be_bytes([seg[1], seg[2]]) as usize;
                sof_w = u16::from_be_bytes([seg[3], seg[4]]) as usize;
                comps = seg[5] as usize;
                if comps == 0 || comps > 4 {
                    return Err(Error::CorruptData(format!("Unsupported LJ92 components: {comps}")));
                }
            }
            0xC4 => {
                let mut idx = 0usize;
                while idx + 17 <= seg.len() {
                    let tc_th = seg[idx];
                    let th = (tc_th & 0x0F) as usize;
                    let mut counts = [0u8; 17];
                    let mut total = 0usize;
                    for i in 1..=16 {
                        counts[i] = seg[idx + i];
                        total += counts[i] as usize;
                    }
                    idx += 17;
                    if idx + total > seg.len() || th >= 4 {
                        return Err(Error::CorruptData("Invalid LJ92 DHT segment".into()));
                    }
                    huff_tables[th] = Some(HuffLut::build(&counts, &seg[idx..idx + total])?);
                    idx += total;
                }
            }
            0xDA => {
                let sos_comps = seg[0] as usize;
                if sos_comps != comps || seg.len() < 1 + 2 * sos_comps + 3 {
                    return Err(Error::CorruptData("Invalid LJ92 SOS segment".into()));
                }
                for c in 0..sos_comps {
                    let td = (seg[1 + 2 * c + 1] >> 4) as usize;
                    comp_table_idx[c] = td.min(3);
                }
                scan_offset = p + 2 + seg_len;
                break;
            }
            _ => {}
        }
        p += 2 + seg_len;
    }

    let stride = sof_w * comps;
    let total_samples = stride * sof_h;
    if scan_offset == 0 || total_samples == 0 || total_samples > expected_pixels {
        return Err(Error::CorruptData(format!(
            "LJ92 geometry mismatch: sof_w={sof_w} sof_h={sof_h} comps={comps} expected_pixels={expected_pixels}"
        )));
    }

    let mut reader = BitReader::new(&data[scan_offset..]);
    let init_pred = 1i32 << (bits.saturating_sub(1));

    // Fast path when all components share Huffman table 0 (true for almost all ML files).
    let single_table = (0..comps).all(|c| comp_table_idx[c] == comp_table_idx[0]);
    if single_table {
        let huff = huff_tables[comp_table_idx[0]]
            .as_ref()
            .ok_or_else(|| Error::CorruptData("Missing LJ92 Huffman table".into()))?;

        let mut prev_row = vec![0u16; stride];
        let mut cur_row = vec![0u16; stride];

        for y in 0..sof_h {
            let row_byte_off = y * stride * 2;
            for c in 0..comps {
                let diff = reader.next_diff(huff)?;
                let pred = if y == 0 { init_pred } else { prev_row[c] as i32 };
                let val = (pred + diff) as u16;
                cur_row[c] = val;
                let b = val.to_le_bytes();
                dst_u16_le[row_byte_off + c * 2] = b[0];
                dst_u16_le[row_byte_off + c * 2 + 1] = b[1];
            }
            for x in comps..stride {
                let diff = reader.next_diff(huff)?;
                let val = (cur_row[x - comps] as i32 + diff) as u16;
                cur_row[x] = val;
                let b = val.to_le_bytes();
                dst_u16_le[row_byte_off + x * 2] = b[0];
                dst_u16_le[row_byte_off + x * 2 + 1] = b[1];
            }
            std::mem::swap(&mut prev_row, &mut cur_row);
        }
    } else {
        let mut prev_row = vec![0u16; stride];
        let mut cur_row = vec![0u16; stride];

        for y in 0..sof_h {
            let row_byte_off = y * stride * 2;
            for c in 0..comps {
                let huff = huff_tables[comp_table_idx[c]]
                    .as_ref()
                    .ok_or_else(|| Error::CorruptData("Missing LJ92 component Huffman table".into()))?;
                let diff = reader.next_diff(huff)?;
                let pred = if y == 0 { init_pred } else { prev_row[c] as i32 };
                let val = (pred + diff) as u16;
                cur_row[c] = val;
                let b = val.to_le_bytes();
                dst_u16_le[row_byte_off + c * 2] = b[0];
                dst_u16_le[row_byte_off + c * 2 + 1] = b[1];
            }
            for x in comps..stride {
                let c = x % comps;
                let huff = huff_tables[comp_table_idx[c]].as_ref().unwrap();
                let diff = reader.next_diff(huff)?;
                let val = (cur_row[x - comps] as i32 + diff) as u16;
                cur_row[x] = val;
                let b = val.to_le_bytes();
                dst_u16_le[row_byte_off + x * 2] = b[0];
                dst_u16_le[row_byte_off + x * 2 + 1] = b[1];
            }
            std::mem::swap(&mut prev_row, &mut cur_row);
        }
    }

    Ok(())
}
