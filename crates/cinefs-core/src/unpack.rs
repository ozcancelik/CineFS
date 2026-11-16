//! Fast bit unpacking and DNG bit-packing routines for 8, 10, 12, 14, and 16-bit raw frames.
//!
//! Magic Lantern `.MLV` stores uncompressed sub-16-bit pixels as an MSB-first bitstream
//! packed into little-endian 16-bit words (`u16`).
//! Adobe DNG (`Compression = 1`, `BitsPerSample < 16`) stores pixels as an MSB-first
//! big-endian byte stream padded to a byte boundary at the end of each row.
//!
//! Key optimization: whenever `(width * bpp) % 16 == 0` (which holds for all standard
//! even-width Bayer sensors at 8, 10, 12, and 14 bits), an MLV packed frame can be
//! transformed into a native-depth DNG strip simply by byte-swapping each 16-bit word
//! (`[b0, b1] -> [b1, b0]`), completely avoiding bit-level unpacking and re-packing!

/// Returns the byte length of one DNG packed row at `bpp` bits per sample.
#[inline]
pub const fn dng_packed_row_bytes(width: usize, bpp: u32) -> usize {
    (width * (bpp as usize) + 7) / 8
}

/// Returns the total byte size of a DNG uncompressed strip at `bpp` bits per sample.
#[inline]
pub const fn dng_strip_byte_count(width: usize, height: usize, bpp: u32) -> usize {
    if bpp == 16 {
        width * height * 2
    } else if bpp == 8 {
        width * height
    } else {
        dng_packed_row_bytes(width, bpp) * height
    }
}

/// Converts an MLV uncompressed packed payload directly into a DNG packed payload at the same `bpp`.
///
/// When `(width * bpp) % 16 == 0`, this is a pure 16-bit byte-swap (`[b0, b1] -> [b1, b0]`)
/// that compilers auto-vectorize with SIMD (`TBL` / `REV16` on NEON, `PSHUFB` on AVX2).
pub fn mlv_packed_to_dng_packed(
    src_mlv: &[u8],
    width: usize,
    height: usize,
    bpp: u32,
    dst_dng: &mut [u8],
) {
    let expected_bytes = dng_strip_byte_count(width, height, bpp);
    let dst = &mut dst_dng[..expected_bytes];

    if bpp == 16 {
        let copy_len = src_mlv.len().min(expected_bytes);
        dst[..copy_len].copy_from_slice(&src_mlv[..copy_len]);
        if copy_len < expected_bytes {
            dst[copy_len..].fill(0);
        }
        return;
    }

    if (width * (bpp as usize)) % 16 == 0 {
        let pair_bytes = src_mlv.len().min(expected_bytes) & !1;
        let src_pairs = &src_mlv[..pair_bytes];
        let dst_pairs = &mut dst[..pair_bytes];
        for (s, d) in src_pairs.chunks_exact(2).zip(dst_pairs.chunks_exact_mut(2)) {
            d[0] = s[1];
            d[1] = s[0];
        }
        if pair_bytes < expected_bytes {
            dst[pair_bytes..].fill(0);
        }
    } else {
        let pixel_count = width * height;
        let mut unpacked = vec![0u8; pixel_count * 2];
        unpack_mlv_to_u16_le(src_mlv, pixel_count, bpp, &mut unpacked);
        pack_u16_le_to_dng_packed(&unpacked, width, height, bpp, dst);
    }
}

/// Unpacks an MLV bitstream (`bpp` bits per sample in little-endian 16-bit words)
/// into little-endian 16-bit samples (`pixel_count * 2` bytes).
pub fn unpack_mlv_to_u16_le(src_mlv: &[u8], pixel_count: usize, bpp: u32, dst_u16_le: &mut [u8]) {
    let out_bytes = pixel_count * 2;
    let dst = &mut dst_u16_le[..out_bytes];

    match bpp {
        16 => {
            let copy_len = src_mlv.len().min(out_bytes);
            dst[..copy_len].copy_from_slice(&src_mlv[..copy_len]);
            if copy_len < out_bytes {
                dst[copy_len..].fill(0);
            }
        }
        14 => unpack_14(src_mlv, pixel_count, dst),
        12 => unpack_12(src_mlv, pixel_count, dst),
        10 => unpack_10(src_mlv, pixel_count, dst),
        8 => unpack_8(src_mlv, pixel_count, dst),
        _ => unpack_generic(src_mlv, pixel_count, bpp, dst),
    }
}

#[inline]
fn put_u16_le(dst: &mut [u8], pixel_idx: usize, val: u16) {
    let bytes = val.to_le_bytes();
    let off = pixel_idx * 2;
    dst[off] = bytes[0];
    dst[off + 1] = bytes[1];
}

#[inline]
fn read_word_le(src: &[u8], word_idx: usize) -> u32 {
    let byte_off = word_idx * 2;
    if byte_off + 1 < src.len() {
        u16::from_le_bytes([src[byte_off], src[byte_off + 1]]) as u32
    } else {
        0
    }
}

fn unpack_8(src: &[u8], pixel_count: usize, dst: &mut [u8]) {
    let word_count = src.len() / 2;
    let pairs = (pixel_count / 2).min(word_count);
    let mut wi = 0;
    let mut pi = 0;
    for _ in 0..pairs {
        let w0 = read_word_le(src, wi) as u16;
        put_u16_le(dst, pi, w0 >> 8);
        put_u16_le(dst, pi + 1, w0 & 0xFF);
        wi += 1;
        pi += 2;
    }
    if pi < pixel_count {
        unpack_generic_range(src, pi, pixel_count, 8, dst);
    }
}

fn unpack_10(src: &[u8], pixel_count: usize, dst: &mut [u8]) {
    let word_count = src.len() / 2;
    let groups = (pixel_count / 8).min(word_count / 5);
    let mut wi = 0;
    let mut pi = 0;
    for _ in 0..groups {
        let w0 = read_word_le(src, wi);
        let w1 = read_word_le(src, wi + 1);
        let w2 = read_word_le(src, wi + 2);
        let w3 = read_word_le(src, wi + 3);
        let w4 = read_word_le(src, wi + 4);

        put_u16_le(dst, pi, (w0 >> 6) as u16);
        put_u16_le(dst, pi + 1, (((w0 & 0x3F) << 4) | (w1 >> 12)) as u16);
        put_u16_le(dst, pi + 2, ((w1 >> 2) & 0x3FF) as u16);
        put_u16_le(dst, pi + 3, (((w1 & 0x03) << 8) | (w2 >> 8)) as u16);
        put_u16_le(dst, pi + 4, (((w2 & 0xFF) << 2) | (w3 >> 14)) as u16);
        put_u16_le(dst, pi + 5, ((w3 >> 4) & 0x3FF) as u16);
        put_u16_le(dst, pi + 6, (((w3 & 0x0F) << 6) | (w4 >> 10)) as u16);
        put_u16_le(dst, pi + 7, (w4 & 0x3FF) as u16);

        wi += 5;
        pi += 8;
    }
    if pi < pixel_count {
        unpack_generic_range(src, pi, pixel_count, 10, dst);
    }
}

fn unpack_12(src: &[u8], pixel_count: usize, dst: &mut [u8]) {
    let word_count = src.len() / 2;
    let groups = (pixel_count / 4).min(word_count / 3);
    let mut wi = 0;
    let mut pi = 0;
    for _ in 0..groups {
        let w0 = read_word_le(src, wi);
        let w1 = read_word_le(src, wi + 1);
        let w2 = read_word_le(src, wi + 2);

        put_u16_le(dst, pi, (w0 >> 4) as u16);
        put_u16_le(dst, pi + 1, (((w0 & 0x0F) << 8) | (w1 >> 8)) as u16);
        put_u16_le(dst, pi + 2, (((w1 & 0xFF) << 4) | (w2 >> 12)) as u16);
        put_u16_le(dst, pi + 3, (w2 & 0xFFF) as u16);

        wi += 3;
        pi += 4;
    }
    if pi < pixel_count {
        unpack_generic_range(src, pi, pixel_count, 12, dst);
    }
}

fn unpack_14(src: &[u8], pixel_count: usize, dst: &mut [u8]) {
    let word_count = src.len() / 2;
    let groups = (pixel_count / 8).min(word_count / 7);
    let mut wi = 0;
    let mut pi = 0;
    for _ in 0..groups {
        let w0 = read_word_le(src, wi);
        let w1 = read_word_le(src, wi + 1);
        let w2 = read_word_le(src, wi + 2);
        let w3 = read_word_le(src, wi + 3);
        let w4 = read_word_le(src, wi + 4);
        let w5 = read_word_le(src, wi + 5);
        let w6 = read_word_le(src, wi + 6);

        put_u16_le(dst, pi, (w0 >> 2) as u16);
        put_u16_le(dst, pi + 1, (((w0 & 0x03) << 12) | (w1 >> 4)) as u16);
        put_u16_le(dst, pi + 2, (((w1 & 0x0F) << 10) | (w2 >> 6)) as u16);
        put_u16_le(dst, pi + 3, (((w2 & 0x3F) << 8) | (w3 >> 8)) as u16);
        put_u16_le(dst, pi + 4, (((w3 & 0xFF) << 6) | (w4 >> 10)) as u16);
        put_u16_le(dst, pi + 5, (((w4 & 0x3FF) << 4) | (w5 >> 12)) as u16);
        put_u16_le(dst, pi + 6, (((w5 & 0xFFF) << 2) | (w6 >> 14)) as u16);
        put_u16_le(dst, pi + 7, (w6 & 0x3FFF) as u16);

        wi += 7;
        pi += 8;
    }
    if pi < pixel_count {
        unpack_generic_range(src, pi, pixel_count, 14, dst);
    }
}

fn unpack_generic(src: &[u8], pixel_count: usize, bpp: u32, dst: &mut [u8]) {
    unpack_generic_range(src, 0, pixel_count, bpp, dst);
}

fn unpack_generic_range(
    src: &[u8],
    start_pixel: usize,
    pixel_count: usize,
    bpp: u32,
    dst: &mut [u8],
) {
    let bpp_usize = bpp as usize;
    let mask = (1u32 << bpp) - 1;
    for pixel_idx in start_pixel..pixel_count {
        let bits_offset = pixel_idx * bpp_usize;
        let word_idx = bits_offset / 16;
        let bit_shift = bits_offset % 16;
        let w0 = read_word_le(src, word_idx);
        let w1 = read_word_le(src, word_idx + 1);
        let combined = (w0 << 16) | w1;
        let shift = 32 - bpp_usize - bit_shift;
        let val = ((combined >> shift) & mask) as u16;
        put_u16_le(dst, pixel_idx, val);
    }
}

/// Packs little-endian 16-bit samples (`src_u16_le`) into a DNG uncompressed strip (`dst_dng`)
/// at `bpp` bits per sample (8, 10, 12, 14, or 16).
pub fn pack_u16_le_to_dng_packed(
    src_u16_le: &[u8],
    width: usize,
    height: usize,
    bpp: u32,
    dst_dng: &mut [u8],
) {
    if bpp == 16 {
        let len = width * height * 2;
        dst_dng[..len].copy_from_slice(&src_u16_le[..len]);
        return;
    }
    if bpp == 8 {
        let pixel_count = width * height;
        for i in 0..pixel_count {
            let v = u16::from_le_bytes([src_u16_le[i * 2], src_u16_le[i * 2 + 1]]);
            dst_dng[i] = (v.min(255)) as u8;
        }
        return;
    }

    if bpp == 10 && width % 4 == 0 {
        let pixel_count = width * height;
        let groups = pixel_count / 4;
        for g in 0..groups {
            let s = &src_u16_le[g * 8..g * 8 + 8];
            let p0 = (u16::from_le_bytes([s[0], s[1]])).min(0x3FF);
            let p1 = (u16::from_le_bytes([s[2], s[3]])).min(0x3FF);
            let p2 = (u16::from_le_bytes([s[4], s[5]])).min(0x3FF);
            let p3 = (u16::from_le_bytes([s[6], s[7]])).min(0x3FF);
            let d = &mut dst_dng[g * 5..g * 5 + 5];
            d[0] = (p0 >> 2) as u8;
            d[1] = (((p0 & 0x03) << 6) | (p1 >> 4)) as u8;
            d[2] = (((p1 & 0x0F) << 4) | (p2 >> 6)) as u8;
            d[3] = (((p2 & 0x3F) << 2) | (p3 >> 8)) as u8;
            d[4] = (p3 & 0xFF) as u8;
        }
        return;
    }

    if bpp == 12 && width % 2 == 0 {
        let pixel_count = width * height;
        let groups = pixel_count / 2;
        for g in 0..groups {
            let s = &src_u16_le[g * 4..g * 4 + 4];
            let p0 = (u16::from_le_bytes([s[0], s[1]])).min(0xFFF);
            let p1 = (u16::from_le_bytes([s[2], s[3]])).min(0xFFF);
            let d = &mut dst_dng[g * 3..g * 3 + 3];
            d[0] = (p0 >> 4) as u8;
            d[1] = (((p0 & 0x0F) << 4) | (p1 >> 8)) as u8;
            d[2] = (p1 & 0xFF) as u8;
        }
        return;
    }

    if bpp == 14 && width % 4 == 0 {
        let pixel_count = width * height;
        let groups = pixel_count / 4;
        for g in 0..groups {
            let s = &src_u16_le[g * 8..g * 8 + 8];
            let p0 = (u16::from_le_bytes([s[0], s[1]])).min(0x3FFF);
            let p1 = (u16::from_le_bytes([s[2], s[3]])).min(0x3FFF);
            let p2 = (u16::from_le_bytes([s[4], s[5]])).min(0x3FFF);
            let p3 = (u16::from_le_bytes([s[6], s[7]])).min(0x3FFF);
            let d = &mut dst_dng[g * 7..g * 7 + 7];
            d[0] = (p0 >> 6) as u8;
            d[1] = (((p0 & 0x3F) << 2) | (p1 >> 12)) as u8;
            d[2] = ((p1 >> 4) & 0xFF) as u8;
            d[3] = (((p1 & 0x0F) << 4) | (p2 >> 10)) as u8;
            d[4] = ((p2 >> 2) & 0xFF) as u8;
            d[5] = (((p2 & 0x03) << 6) | (p3 >> 8)) as u8;
            d[6] = (p3 & 0xFF) as u8;
        }
        return;
    }

    let max_val = (1u32 << bpp) - 1;
    let row_bytes = dng_packed_row_bytes(width, bpp);
    for y in 0..height {
        let mut out_idx = y * row_bytes;
        let mut acc: u64 = 0;
        let mut acc_bits: u32 = 0;
        let row_start = y * width;
        for x in 0..width {
            let p = row_start + x;
            let raw = u16::from_le_bytes([src_u16_le[p * 2], src_u16_le[p * 2 + 1]]) as u32;
            let v = raw.min(max_val) as u64;
            acc = (acc << bpp) | v;
            acc_bits += bpp;
            while acc_bits >= 8 {
                acc_bits -= 8;
                dst_dng[out_idx] = (acc >> acc_bits) as u8;
                out_idx += 1;
            }
        }
        if acc_bits > 0 {
            dst_dng[out_idx] = (acc << (8 - acc_bits)) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that packs u16 values into MLV's MSB-first little-endian 16-bit word format.
    fn pack_mlv_reference(samples: &[u16], bpp: u32) -> Vec<u8> {
        let mut acc: u64 = 0;
        let mut acc_bits: u32 = 0;
        let mask = (1u64 << bpp) - 1;
        let mut out = Vec::new();
        for &s in samples {
            acc = (acc << bpp) | ((s as u64) & mask);
            acc_bits += bpp;
            while acc_bits >= 16 {
                acc_bits -= 16;
                let w = ((acc >> acc_bits) & 0xFFFF) as u16;
                out.extend_from_slice(&w.to_le_bytes());
                acc &= (1u64 << acc_bits) - 1;
            }
        }
        if acc_bits > 0 {
            let w = ((acc << (16 - acc_bits)) & 0xFFFF) as u16;
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn test_unpack_and_fast_dng_swap_equivalence() {
        let width = 64;
        let height = 8;
        let count = width * height;

        for bpp in [8u32, 10, 12, 14] {
            let max_v = (1u32 << bpp) - 1;
            let samples: Vec<u16> = (0..count)
                .map(|i| ((i as u32 * 73 + 19) & max_v) as u16)
                .collect();

            let mlv_packed = pack_mlv_reference(&samples, bpp);

            // 1. Test unpack back to u16
            let mut unpacked_bytes = vec![0u8; count * 2];
            unpack_mlv_to_u16_le(&mlv_packed, count, bpp, &mut unpacked_bytes);
            for (i, &expected) in samples.iter().enumerate() {
                let got = u16::from_le_bytes([unpacked_bytes[i * 2], unpacked_bytes[i * 2 + 1]]);
                assert_eq!(got, expected, "mismatch at bpp={bpp} pixel={i}");
            }

            // 2. Compare fast 16-bit byte-swap path vs full unpack + pack_dng path
            let strip_len = dng_strip_byte_count(width, height, bpp);
            let mut dng_fast = vec![0u8; strip_len];
            let mut dng_slow = vec![0u8; strip_len];

            mlv_packed_to_dng_packed(&mlv_packed, width, height, bpp, &mut dng_fast);
            pack_u16_le_to_dng_packed(&unpacked_bytes, width, height, bpp, &mut dng_slow);

            assert_eq!(
                dng_fast, dng_slow,
                "fast byte-swap DNG pack differed from slow unpack+repack at bpp={bpp}"
            );
        }
    }
}
