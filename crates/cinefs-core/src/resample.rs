//! Raw-domain (Bayer-preserving) anamorphic stretch.
//!
//! crop_rec modes bin/skip sensor lines, so the stored raw frame has non-square pixels
//! (e.g. EOS M 3x1: 1408x2538 where every column is 3 sensor columns wide). To hand a DNG with
//! square pixels to *any* application we resample the raw mosaic itself. The CFA pattern must
//! survive, so every output sample is interpolated **only between source samples of the same
//! colour** (same row/column parity), linearly. Output dimensions along the stretched axis are
//! even (multiples of 4 from the caller), so the 2x2 CFA tiling stays aligned.

use std::thread;

const FP: u32 = 16;
const ONE: u64 = 1 << FP;

/// `(i0, i1, w1)` per output index: `out = src[i0]*(1-w1) + src[i1]*w1` (w1 in 16.16 fixed point),
/// with `i0`/`i1` having the same parity as the output index.
fn axis_taps(src_n: usize, out_n: usize) -> Vec<(u32, u32, u32)> {
    let f = out_n as f64 / src_n as f64;
    let mut taps = Vec::with_capacity(out_n);
    for j in 0..out_n {
        let p = (j & 1) as i64;
        // largest source index with parity p
        let max_c: i64 = if ((src_n - 1) as i64 & 1) == p {
            (src_n - 1) as i64
        } else {
            (src_n as i64 - 2).max(p)
        };
        let x = (j as f64 + 0.5) / f - 0.5; // source-space centre of this output sample
        let k = ((x - p as f64) / 2.0).floor() as i64;
        let c0 = p + 2 * k;
        let t = ((x - c0 as f64) / 2.0).clamp(0.0, 1.0);
        let i0 = c0.clamp(p, max_c);
        let i1 = (c0 + 2).clamp(p, max_c);
        taps.push((i0 as u32, i1 as u32, (t * ONE as f64) as u32));
    }
    taps
}

#[inline]
fn blend(a: u16, b: u16, w1: u32) -> u16 {
    let w1 = w1 as u64;
    ((a as u64 * (ONE - w1) + b as u64 * w1 + (ONE >> 1)) >> FP) as u16
}

fn worker_count(pixels: usize) -> usize {
    if pixels < 1_000_000 {
        1
    } else {
        thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8)
    }
}

fn horizontal(src: &[u16], sw: usize, sh: usize, ow: usize) -> Vec<u16> {
    let taps = axis_taps(sw, ow);
    let mut out = vec![0u16; ow * sh];
    let workers = worker_count(ow * sh);
    let rows_per = (sh + workers - 1) / workers;
    thread::scope(|s| {
        for (ci, chunk) in out.chunks_mut(rows_per * ow).enumerate() {
            let taps = &taps;
            s.spawn(move || {
                let r0 = ci * rows_per;
                for (ri, orow) in chunk.chunks_mut(ow).enumerate() {
                    let srow = &src[(r0 + ri) * sw..(r0 + ri + 1) * sw];
                    for (j, o) in orow.iter_mut().enumerate() {
                        let (i0, i1, w) = taps[j];
                        *o = blend(srow[i0 as usize], srow[i1 as usize], w);
                    }
                }
            });
        }
    });
    out
}

fn vertical(src: &[u16], w: usize, sh: usize, oh: usize) -> Vec<u16> {
    let taps = axis_taps(sh, oh);
    let mut out = vec![0u16; w * oh];
    let workers = worker_count(w * oh);
    let rows_per = (oh + workers - 1) / workers;
    thread::scope(|s| {
        for (ci, chunk) in out.chunks_mut(rows_per * w).enumerate() {
            let taps = &taps;
            s.spawn(move || {
                let r0 = ci * rows_per;
                for (ri, orow) in chunk.chunks_mut(w).enumerate() {
                    let (i0, i1, wt) = taps[r0 + ri];
                    let a = &src[i0 as usize * w..(i0 as usize + 1) * w];
                    let b = &src[i1 as usize * w..(i1 as usize + 1) * w];
                    for x in 0..w {
                        orow[x] = blend(a[x], b[x], wt);
                    }
                }
            });
        }
    });
    out
}

/// Stretches a little-endian `u16` Bayer mosaic of `sw x sh` samples to `ow x oh` (either axis may
/// stay unchanged). Returns little-endian `u16` bytes (`ow * oh * 2`).
pub fn stretch_bayer_u16(src_le: &[u8], sw: usize, sh: usize, ow: usize, oh: usize) -> Vec<u8> {
    let mut cur: Vec<u16> = src_le[..sw * sh * 2]
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let (mut w, mut h) = (sw, sh);
    if ow != w {
        cur = horizontal(&cur, w, h, ow);
        w = ow;
    }
    if oh != h {
        cur = vertical(&cur, w, h, oh);
        h = oh;
    }
    let mut out = Vec::with_capacity(w * h * 2);
    for v in cur {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_le(v: &[u16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
    fn from_le(b: &[u8]) -> Vec<u16> {
        b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn constant_image_stays_constant_and_dims_match() {
        let src = to_le(&vec![1234u16; 8 * 6]);
        let out = from_le(&stretch_bayer_u16(&src, 8, 6, 24, 6));
        assert_eq!(out.len(), 24 * 6);
        assert!(out.iter().all(|&v| v == 1234));
        let out = from_le(&stretch_bayer_u16(&src, 8, 6, 8, 12));
        assert_eq!(out.len(), 8 * 12);
        assert!(out.iter().all(|&v| v == 1234));
    }

    #[test]
    fn horizontal_stretch_keeps_cfa_column_parity() {
        // even columns = 100 (colour A), odd columns = 900 (colour B)
        let sw = 16;
        let sh = 4;
        let mut px = vec![0u16; sw * sh];
        for r in 0..sh {
            for c in 0..sw {
                px[r * sw + c] = if c % 2 == 0 { 100 } else { 900 };
            }
        }
        let out = from_le(&stretch_bayer_u16(&to_le(&px), sw, sh, sw * 3, sh));
        for r in 0..sh {
            for j in 0..sw * 3 {
                let expect = if j % 2 == 0 { 100 } else { 900 };
                assert_eq!(out[r * sw * 3 + j], expect, "row {r} col {j}");
            }
        }
    }

    #[test]
    fn vertical_stretch_keeps_cfa_row_parity() {
        let (sw, sh) = (4, 12);
        let mut px = vec![0u16; sw * sh];
        for r in 0..sh {
            for c in 0..sw {
                px[r * sw + c] = if r % 2 == 0 { 50 } else { 700 };
            }
        }
        let oh = 20; // x1.67
        let out = from_le(&stretch_bayer_u16(&to_le(&px), sw, sh, sw, oh));
        for r in 0..oh {
            for c in 0..sw {
                let expect = if r % 2 == 0 { 50 } else { 700 };
                assert_eq!(out[r * sw + c], expect, "row {r}");
            }
        }
    }

    #[test]
    fn ramp_is_monotonic_per_colour() {
        let sw = 32;
        let px: Vec<u16> = (0..sw).map(|c| (c * 100) as u16).collect();
        let out = from_le(&stretch_bayer_u16(&to_le(&px), sw, 1, sw * 3, 1));
        for parity in 0..2 {
            let col: Vec<u16> = out.iter().enumerate().filter(|(i, _)| i % 2 == parity).map(|(_, v)| *v).collect();
            assert!(col.windows(2).all(|w| w[0] <= w[1]), "parity {parity} not monotonic");
        }
    }
}
