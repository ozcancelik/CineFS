//! Focus-pixel *probe* (diagnostic spike, not yet part of the DNG pipeline).
//!
//! Phase-detect (PDAF) pixels on 650D / 700D / 100D / EOS M are half-masked, so they read
//! systematically darker (or different) than their same-colour neighbours **in every frame,
//! whatever the scene**. This module finds such pixels purely from the data:
//!
//! 1. For every pixel of every sampled frame, compare it with the median of its 8 same-colour
//!    neighbours (±2 px, which stays on the same Bayer channel). Textured areas are skipped.
//! 2. Count in how many frames it was clearly darker / brighter than that median.
//! 3. A pixel that is "off" in nearly all frames is a candidate. Real focus pixels sit on a
//!    periodic lattice, which [`lattice_report`] makes visible.

use crate::mlv::MlvClip;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    /// Black level subtracted before comparing.
    pub black: f32,
    /// Ratio below which a pixel counts as "dark" (e.g. 0.88).
    pub low: f32,
    /// Ratio above which a pixel counts as "bright" (e.g. 1.12).
    pub high: f32,
    /// Neighbour median (black-subtracted) must exceed this, or the pixel is skipped (noise).
    pub min_signal: f32,
}

impl ProbeConfig {
    /// Sensible defaults for a clip with `bpp` bits per sample and the given black level.
    pub fn for_bits(bpp: u32, black: f32) -> Self {
        let scale = (1u64 << bpp.clamp(8, 16)) as f32 / 16384.0;
        Self { black, low: 0.88, high: 1.12, min_signal: 128.0 * scale }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub x: u32,
    pub y: u32,
    /// Fraction (0..1) of usable frames in which the pixel was off.
    pub persistence: f32,
    /// `true` = darker than neighbours, `false` = brighter.
    pub dark: bool,
}

pub struct ProbeAccum {
    pub w: usize,
    pub h: usize,
    pub frames: u32,
    valid: Vec<u8>,
    low: Vec<u8>,
    high: Vec<u8>,
}

impl ProbeAccum {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            frames: 0,
            valid: vec![0; w * h],
            low: vec![0; w * h],
            high: vec![0; w * h],
        }
    }

    /// Adds one decoded frame (`w*h` host-order samples). Works on all CPU cores.
    pub fn add_frame(&mut self, px: &[u16], cfg: ProbeConfig) {
        let (w, h) = (self.w, self.h);
        assert_eq!(px.len(), w * h, "frame size mismatch");
        if w < 5 || h < 5 || self.frames >= 250 {
            return;
        }
        self.frames += 1;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16);
        let rows_per = h.div_ceil(threads);
        std::thread::scope(|s| {
            let chunks = self
                .valid
                .chunks_mut(rows_per * w)
                .zip(self.low.chunks_mut(rows_per * w))
                .zip(self.high.chunks_mut(rows_per * w));
            for (i, ((vc, lc), hc)) in chunks.enumerate() {
                s.spawn(move || process_rows(px, w, h, i * rows_per, vc, lc, hc, cfg));
            }
        });
    }

    /// Pixels that are off in at least `min_persistence` of the frames in which they were usable
    /// (and usable in at least half of all frames).
    pub fn candidates(&self, min_persistence: f32) -> Vec<Candidate> {
        let need_valid = (self.frames / 2).max(1) as u8;
        let mut out = Vec::new();
        for y in 0..self.h {
            for x in 0..self.w {
                let i = y * self.w + x;
                let v = self.valid[i];
                if v < need_valid {
                    continue;
                }
                let (l, hgh) = (self.low[i], self.high[i]);
                let (n, dark) = if l >= hgh { (l, true) } else { (hgh, false) };
                let p = n as f32 / v as f32;
                if p >= min_persistence {
                    out.push(Candidate { x: x as u32, y: y as u32, persistence: p, dark });
                }
            }
        }
        out
    }

    /// Persistence map for visualisation: `(value 0..1, dark)` per pixel.
    pub fn persistence_at(&self, x: usize, y: usize) -> (f32, bool) {
        let i = y * self.w + x;
        let v = self.valid[i];
        if v == 0 {
            return (0.0, true);
        }
        let (l, h) = (self.low[i], self.high[i]);
        if l >= h {
            (l as f32 / v as f32, true)
        } else {
            (h as f32 / v as f32, false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_rows(
    px: &[u16],
    w: usize,
    h: usize,
    y0: usize,
    vc: &mut [u8],
    lc: &mut [u8],
    hc: &mut [u8],
    cfg: ProbeConfig,
) {
    let rows = vc.len() / w;
    for ry in 0..rows {
        let y = y0 + ry;
        if y < 2 || y + 2 >= h {
            continue;
        }
        for x in 2..w - 2 {
            let at = |xx: usize, yy: usize| (px[yy * w + xx] as f32 - cfg.black).max(0.0);
            let mut n = [
                at(x - 2, y),
                at(x + 2, y),
                at(x, y - 2),
                at(x, y + 2),
                at(x - 2, y - 2),
                at(x + 2, y - 2),
                at(x - 2, y + 2),
                at(x + 2, y + 2),
            ];
            n.sort_unstable_by(f32::total_cmp);
            let med = (n[3] + n[4]) * 0.5;
            if med < cfg.min_signal || (n[7] - n[0]) > 0.6 * med {
                continue; // too dark (noise) or textured (edge): no reliable verdict
            }
            let r = at(x, y) / med;
            let i = ry * w + x;
            vc[i] = vc[i].saturating_add(1);
            if r < cfg.low {
                lc[i] = lc[i].saturating_add(1);
            } else if r > cfg.high {
                hc[i] = hc[i].saturating_add(1);
            }
        }
    }
}

/// Text summary of the spatial structure of `cands`: counts by position modulo a few periods and
/// the most common spacing between neighbouring candidates in the same row / column.
pub fn lattice_report(cands: &[Candidate]) -> String {
    use std::collections::{BTreeMap, HashMap};
    let mut s = String::new();
    s.push_str(&format!("candidates: {}\n", cands.len()));
    if cands.is_empty() {
        return s;
    }
    let dark = cands.iter().filter(|c| c.dark).count();
    s.push_str(&format!("  darker than neighbours: {dark}, brighter: {}\n", cands.len() - dark));

    for m in [2u32, 4, 8, 16] {
        let mut hx = vec![0usize; m as usize];
        let mut hy = vec![0usize; m as usize];
        for c in cands {
            hx[(c.x % m) as usize] += 1;
            hy[(c.y % m) as usize] += 1;
        }
        s.push_str(&format!("  x mod {m:>2}: {hx:?}\n  y mod {m:>2}: {hy:?}\n"));
    }

    let mut rows: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut cols: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for c in cands {
        rows.entry(c.y).or_default().push(c.x);
        cols.entry(c.x).or_default().push(c.y);
    }
    let spacing = |groups: &BTreeMap<u32, Vec<u32>>| {
        let mut hist: HashMap<u32, usize> = HashMap::new();
        for v in groups.values() {
            let mut v = v.clone();
            v.sort_unstable();
            for p in v.windows(2) {
                let d = p[1] - p[0];
                if d <= 64 {
                    *hist.entry(d).or_default() += 1;
                }
            }
        }
        let mut top: Vec<_> = hist.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top.truncate(6);
        top
    };
    s.push_str(&format!("  spacing along a row    (dx, count): {:?}\n", spacing(&rows)));
    s.push_str(&format!("  spacing along a column (dy, count): {:?}\n", spacing(&cols)));

    // Distinct rows that contain candidates, and their spacing, shows the vertical period.
    let mut row_ids: Vec<u32> = rows.keys().copied().collect();
    row_ids.sort_unstable();
    let mut dy_hist: HashMap<u32, usize> = HashMap::new();
    for p in row_ids.windows(2) {
        *dy_hist.entry(p[1] - p[0]).or_default() += 1;
    }
    let mut top: Vec<_> = dy_hist.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(6);
    s.push_str(&format!("  spacing between rows containing candidates (dy, count): {top:?}\n"));
    s
}

/// Decodes `frame_index` to host-order `u16` samples (MLV packed / LJ92 / MCRAW / BRAW).
pub fn decode_frame_u16(clip: &MlvClip, frame_index: usize) -> Result<Vec<u16>> {
    use crate::lj92::decode_lj92_to_u16_le;
    use crate::unpack::unpack_mlv_to_u16_le;

    let (w, h) = (clip.width as usize, clip.height as usize);
    let pixels = w * h;
    let entry = clip
        .frames
        .get(frame_index)
        .ok_or_else(|| Error::NotFound(format!("frame {frame_index} out of range")))?;
    let mut raw = vec![0u8; entry.payload_size as usize];
    clip.read_raw_frame_payload(frame_index, &mut raw)?;
    let mut out = vec![0u8; pixels * 2];
    if clip.is_codec_decoded() {
        clip.decode_codec_frame_to_u16_le(&raw, &mut out)?;
    } else if clip.is_lj92 {
        decode_lj92_to_u16_le(&raw, pixels, &mut out)?;
    } else if clip.bits_per_pixel == 16 {
        if raw.len() < pixels * 2 {
            return Err(Error::InvalidFormat("short 16-bit frame".into()));
        }
        out.copy_from_slice(&raw[..pixels * 2]);
    } else {
        if (raw.len() as u64) * 8 < pixels as u64 * clip.bits_per_pixel as u64 {
            return Err(Error::InvalidFormat("short packed frame".into()));
        }
        unpack_mlv_to_u16_le(&raw, pixels, clip.bits_per_pixel, &mut out);
    }
    Ok(out.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smooth, scene-like gradient (shifting per frame) with a dark lattice every 8 px in x and
    /// 6 px in y. Lattice spacing keeps ±2 same-colour neighbours clean.
    fn synthetic(w: usize, h: usize, frame: usize) -> Vec<u16> {
        let mut v = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                let base = 3000 + 12 * x + 9 * y + frame * 211 + ((x * 7 + y * 13 + frame * 5) % 11) * 3;
                let is_fp = x % 8 == 3 && y % 6 == 2;
                v[y * w + x] = if is_fp { (base as f32 * 0.6) as u16 } else { base as u16 };
            }
        }
        v
    }

    #[test]
    fn finds_dark_lattice_and_reports_spacing() {
        let (w, h) = (128, 96);
        let mut acc = ProbeAccum::new(w, h);
        let cfg = ProbeConfig { black: 0.0, low: 0.88, high: 1.12, min_signal: 100.0 };
        for f in 0..8 {
            acc.add_frame(&synthetic(w, h, f), cfg);
        }
        let cands = acc.candidates(0.8);
        let expected: usize = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| x % 8 == 3 && y % 6 == 2 && (2..w - 2).contains(&x) && (2..h - 2).contains(&y))
            .count();
        let hits = cands.iter().filter(|c| c.x % 8 == 3 && c.y % 6 == 2 && c.dark).count();
        let false_pos = cands.len() - hits;
        assert_eq!(hits, expected, "every lattice pixel must be found");
        assert!(false_pos * 20 <= expected, "too many false positives: {false_pos}");
        let report = lattice_report(&cands);
        assert!(report.contains("(8,"), "row spacing 8 expected in:\n{report}");
    }

    #[test]
    fn clean_image_has_no_candidates() {
        let (w, h) = (64, 64);
        let mut acc = ProbeAccum::new(w, h);
        let cfg = ProbeConfig { black: 0.0, low: 0.88, high: 1.12, min_signal: 100.0 };
        for f in 0..4 {
            let mut img = synthetic(w, h, f);
            for y in 0..h {
                for x in 0..w {
                    if x % 8 == 3 && y % 6 == 2 {
                        img[y * w + x] = (3000 + 12 * x + 9 * y + f * 211 + ((x * 7 + y * 13 + f * 5) % 11) * 3) as u16;
                    }
                }
            }
            acc.add_frame(&img, cfg);
        }
        assert!(acc.candidates(0.8).is_empty());
    }
}
