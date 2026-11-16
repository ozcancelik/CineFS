//! Focus-pixel (PDAF) correction for Canon bodies whose sensor has phase-detect pixels
//! (100D / 650D / 700D / EOS M).
//!
//! The pixel positions are **learned from the clip itself**: [`probe_clip`] samples some frames,
//! finds pixels that deviate from their same-colour neighbours in nearly every frame
//! ([`crate::focus_probe`]), then [`fit_lattice`] recognises that those pixels sit on a periodic
//! lattice (e.g. 8 px × 30 rows, several offsets per tile) inside a rectangular AF region.
//! The correction then covers the *whole* lattice in that region, including pixels the probe
//! missed because they were hidden by scene texture, and replaces each one with an
//! edge-aware average of its same-colour neighbours. It must run on raw sensor data, *before*
//! any stretch / resampling.

use crate::focus_probe::{decode_frame_u16, Candidate, ProbeAccum, ProbeConfig};
use crate::mlv::MlvClip;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

static FOCUS_FIX: AtomicBool = AtomicBool::new(true);

/// Enables / disables the focus-pixel correction for clips opened afterwards (default: on).
pub fn set_focus_fix(on: bool) {
    FOCUS_FIX.store(on, Ordering::Relaxed);
}

pub fn focus_fix_enabled() -> bool {
    FOCUS_FIX.load(Ordering::Relaxed)
}

/// Bodies whose sensors have focus pixels visible in raw video. Matched against the IDNT name.
pub fn camera_has_focus_pixels(camera_name: &str) -> bool {
    let n = camera_name.to_ascii_uppercase();
    ["EOS M", "650D", "700D", "100D", "REBEL T4I", "REBEL T5I", "REBEL SL1", "KISS X6I", "KISS X7I", "KISS X7"]
        .iter()
        .any(|k| n.contains(k))
}

/// Rectangular region (inclusive) in which a periodic focus-pixel lattice lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatticeFix {
    pub period_x: u32,
    pub period_y: u32,
    /// Lattice cells as `(x mod period_x, y mod period_y)`.
    pub cells: Vec<(u32, u32)>,
    pub x0: u32,
    pub x1: u32,
    pub y0: u32,
    pub y1: u32,
    lut: Vec<bool>,
}

impl LatticeFix {
    pub fn new(period_x: u32, period_y: u32, cells: Vec<(u32, u32)>, x0: u32, x1: u32, y0: u32, y1: u32) -> Self {
        let mut lut = vec![false; (period_x * period_y) as usize];
        for &(cx, cy) in &cells {
            lut[(cy * period_x + cx) as usize] = true;
        }
        Self { period_x, period_y, cells, x0, x1, y0, y1, lut }
    }

    #[inline]
    pub fn is_defect(&self, x: u32, y: u32) -> bool {
        x >= self.x0
            && x <= self.x1
            && y >= self.y0
            && y <= self.y1
            && self.lut[((y % self.period_y) * self.period_x + x % self.period_x) as usize]
    }

    /// Number of pixels that will be corrected.
    pub fn pixel_count(&self) -> usize {
        let mut n = 0;
        self.for_each_defect(|_, _| n += 1);
        n
    }

    fn for_each_defect(&self, mut f: impl FnMut(u32, u32)) {
        for y in self.y0..=self.y1 {
            let ry = y % self.period_y;
            for &(cx, cy) in &self.cells {
                if cy != ry {
                    continue;
                }
                // first x >= x0 with x % period_x == cx
                let mut x = self.x0 - self.x0 % self.period_x + cx;
                if x < self.x0 {
                    x += self.period_x;
                }
                while x <= self.x1 {
                    f(x, y);
                    x += self.period_x;
                }
            }
        }
    }

    /// Same correction as [`apply_le`](Self::apply_le) for host-order `u16` samples (GPU player path).
    pub fn apply_u16(&self, buf: &mut [u16], w: usize, h: usize) {
        if buf.len() < w * h || w < 9 || h < 9 {
            return;
        }
        self.for_each_defect(|dx, dy| {
            let (x, y) = (dx as usize, dy as usize);
            if x >= w || y >= h {
                return;
            }
            let mut best: Option<(i32, i32)> = None;
            for d in [2usize, 4] {
                let mut consider = |a: Option<(usize, usize)>, b: Option<(usize, usize)>| {
                    if let (Some((ax, ay)), Some((bx, by))) = (a, b) {
                        if !self.is_defect(ax as u32, ay as u32) && !self.is_defect(bx as u32, by as u32) {
                            let (va, vb) = (buf[ay * w + ax] as i32, buf[by * w + bx] as i32);
                            let cand = ((va - vb).abs(), (va + vb + 1) / 2);
                            if best.map_or(true, |b| cand.0 < b.0) {
                                best = Some(cand);
                            }
                        }
                    }
                };
                consider(x.checked_sub(d).map(|nx| (nx, y)), (x + d < w).then_some((x + d, y)));
                consider(y.checked_sub(d).map(|ny| (x, ny)), (y + d < h).then_some((x, y + d)));
                if best.is_some() {
                    break;
                }
            }
            if let Some((_, avg)) = best {
                buf[y * w + x] = avg.clamp(0, 65535) as u16;
            }
        });
    }

    /// Corrects the lattice pixels of one frame (`w*h` little-endian `u16` samples) in place.
    pub fn apply_le(&self, buf: &mut [u8], w: usize, h: usize) {
        if buf.len() < w * h * 2 || w < 9 || h < 9 {
            return;
        }
        let get = |b: &[u8], x: usize, y: usize| -> i32 {
            let i = (y * w + x) * 2;
            u16::from_le_bytes([b[i], b[i + 1]]) as i32
        };
        // x/y are validated against the frame below.
        self.for_each_defect(|dx, dy| {
            let (x, y) = (dx as usize, dy as usize);
            if x >= w || y >= h {
                return;
            }
            // Same-colour neighbours sit 2 (then 4) pixels away; skip any that are defects too.
            let mut best: Option<(i32, i32)> = None; // (|difference|, average)
            for d in [2usize, 4] {
                let mut consider = |a: Option<(usize, usize)>, b: Option<(usize, usize)>| {
                    if let (Some((ax, ay)), Some((bx, by))) = (a, b) {
                        if !self.is_defect(ax as u32, ay as u32) && !self.is_defect(bx as u32, by as u32) {
                            let (va, vb) = (get(buf, ax, ay), get(buf, bx, by));
                            let cand = ((va - vb).abs(), (va + vb + 1) / 2);
                            if best.map_or(true, |b| cand.0 < b.0) {
                                best = Some(cand);
                            }
                        }
                    }
                };
                consider(
                    x.checked_sub(d).map(|nx| (nx, y)),
                    (x + d < w).then_some((x + d, y)),
                );
                consider(
                    y.checked_sub(d).map(|ny| (x, ny)),
                    (y + d < h).then_some((x, y + d)),
                );
                if best.is_some() {
                    break;
                }
            }
            if let Some((_, avg)) = best {
                let i = (y * w + x) * 2;
                buf[i..i + 2].copy_from_slice(&(avg.clamp(0, 65535) as u16).to_le_bytes());
            }
        });
    }
}

/// Smallest period along one axis under which the candidate set maps onto itself.
fn find_period(set: &HashSet<(u32, u32)>, cands: &[Candidate], vertical: bool, max_p: u32) -> Option<u32> {
    let mut scores = vec![0usize; max_p as usize + 1];
    for p in 2..=max_p {
        scores[p as usize] = cands
            .iter()
            .filter(|c| {
                let k = if vertical { (c.x, c.y + p) } else { (c.x + p, c.y) };
                set.contains(&k)
            })
            .count();
    }
    let max = *scores.iter().max()?;
    if (max as f32) < 0.05 * cands.len() as f32 || max == 0 {
        return None;
    }
    (2..=max_p).find(|&p| scores[p as usize] as f32 >= 0.5 * max as f32)
}

/// Recognises a periodic lattice in `cands`. Returns `None` when the candidates do not form one
/// (so noise or scene detail is never mistaken for focus pixels).
pub fn fit_lattice(cands: &[Candidate], w: usize, h: usize) -> Option<LatticeFix> {
    if cands.len() < 150 {
        return None;
    }
    let set: HashSet<(u32, u32)> = cands.iter().map(|c| (c.x, c.y)).collect();
    let px = find_period(&set, cands, false, 64)?;
    let py = find_period(&set, cands, true, 128)?;

    let mut hist = vec![0usize; (px * py) as usize];
    for c in cands {
        hist[((c.y % py) * px + c.x % px) as usize] += 1;
    }
    let max = *hist.iter().max()?;
    let keep_min = ((0.05 * max as f32) as usize).max(8);
    let cells: Vec<(u32, u32)> = hist
        .iter()
        .enumerate()
        .filter(|(_, &n)| n >= keep_min)
        .map(|(i, _)| (i as u32 % px, i as u32 / px))
        .collect();
    let kept: usize = hist.iter().filter(|&&n| n >= keep_min).sum();
    let coverage = kept as f32 / cands.len() as f32;
    let sparsity = cells.len() as f32 / (px * py) as f32;
    if cells.is_empty() || coverage < 0.95 || sparsity > 0.25 {
        return None;
    }

    let in_cells = |c: &&Candidate| cells.contains(&(c.x % px, c.y % py));
    let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
    for c in cands.iter().filter(in_cells) {
        x0 = x0.min(c.x);
        x1 = x1.max(c.x);
        y0 = y0.min(c.y);
        y1 = y1.max(c.y);
    }
    // One period of margin: pixels at the region border may have been hidden by scene texture.
    let x0 = x0.saturating_sub(px).max(2);
    let y0 = y0.saturating_sub(py).max(2);
    let x1 = (x1 + px).min(w as u32 - 3);
    let y1 = (y1 + py).min(h as u32 - 3);
    Some(LatticeFix::new(px, py, cells, x0, x1, y0, y1))
}

/// Learns the focus-pixel lattice of `clip` from the clip's own frames. `None` when the camera has
/// no focus pixels, the feature is off, or no convincing lattice is found.
pub fn probe_clip(clip: &MlvClip) -> Option<LatticeFix> {
    if !focus_fix_enabled() || clip.is_codec_decoded() || !camera_has_focus_pixels(&clip.camera_name) {
        return None;
    }
    let (w, h) = (clip.width as usize, clip.height as usize);
    let n = clip.frames.len();
    if n == 0 || w < 64 || h < 64 {
        return None;
    }
    let want = n.min(16);
    let picks: Vec<usize> = if want == 1 { vec![0] } else { (0..want).map(|i| i * (n - 1) / (want - 1)).collect() };
    let cfg = ProbeConfig::for_bits(clip.bits_per_pixel, clip.levels.black as f32);
    let mut acc = ProbeAccum::new(w, h);
    for idx in picks {
        match decode_frame_u16(clip, idx) {
            Ok(px) => acc.add_frame(&px, cfg),
            Err(e) => crate::log::write("WARN", &format!("focus probe: frame {idx}: {e}")),
        }
    }
    if acc.frames < 2 {
        return None;
    }
    let cands = acc.candidates(0.8);
    let fit = fit_lattice(&cands, w, h);
    match &fit {
        Some(f) => crate::log::write(
            "INFO",
            &format!(
                "clip '{}': focus pixels: {} candidates -> lattice {}x{} cells {:?}, region x {}..{} y {}..{}, {} pixels corrected",
                clip.base_name, cands.len(), f.period_x, f.period_y, f.cells, f.x0, f.x1, f.y0, f.y1, f.pixel_count()
            ),
        ),
        None => crate::log::write(
            "INFO",
            &format!("clip '{}': focus pixels: {} candidates, no lattice -> no correction", clip.base_name, cands.len()),
        ),
    }
    fit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lattice_cands(w: u32, h: u32, keep: impl Fn(u32, u32) -> bool) -> Vec<Candidate> {
        let cells = [(7u32, 3u32), (0, 6), (4, 18), (2, 20), (6, 20), (3, 21), (1, 25), (5, 25)];
        let mut v = Vec::new();
        for y in 100..h - 100 {
            for x in 40..w - 40 {
                if cells.contains(&(x % 8, y % 30)) && keep(x, y) {
                    v.push(Candidate { x, y, persistence: 1.0, dark: false });
                }
            }
        }
        v
    }

    #[test]
    fn recognises_the_eos_m_lattice_from_partial_detections() {
        // ~35 % of lattice pixels detected, pseudo-randomly, like in a textured scene.
        let c = lattice_cands(400, 600, |x, y| {
            let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            h % 100 < 35
        });
        let f = fit_lattice(&c, 400, 600).expect("lattice");
        assert_eq!((f.period_x, f.period_y), (8, 30));
        assert_eq!(f.cells.len(), 8);
        assert!(f.is_defect(7 + 8 * 10, 3 + 30 * 8)); // a pixel the probe may have missed
        assert!(!f.is_defect(0, 0));
    }

    #[test]
    fn random_candidates_form_no_lattice() {
        let mut v = Vec::new();
        let mut s = 12345u32;
        for _ in 0..800 {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let x = 10 + (s >> 8) % 300;
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let y = 10 + (s >> 8) % 400;
            v.push(Candidate { x, y, persistence: 1.0, dark: false });
        }
        assert!(fit_lattice(&v, 400, 600).is_none());
    }

    #[test]
    fn apply_replaces_defects_and_leaves_the_rest() {
        let (w, h) = (96usize, 96usize);
        let mut buf = vec![0u8; w * h * 2];
        let base = |x: usize, y: usize| (3000 + 5 * x + 4 * y) as u16;
        for y in 0..h {
            for x in 0..w {
                buf[(y * w + x) * 2..][..2].copy_from_slice(&base(x, y).to_le_bytes());
            }
        }
        let fix = LatticeFix::new(8, 30, vec![(7, 3), (0, 6)], 10, 80, 10, 80);
        let mut expected_changed = 0;
        let mut dirty = buf.clone();
        fix.for_each_defect(|x, y| {
            let i = (y as usize * w + x as usize) * 2;
            dirty[i..i + 2].copy_from_slice(&(base(x as usize, y as usize) + 900).to_le_bytes());
            expected_changed += 1;
        });
        assert!(expected_changed > 5);
        fix.apply_le(&mut dirty, w, h);
        // linear gradient -> neighbour average reproduces the original exactly (±1 rounding)
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 2;
                let got = u16::from_le_bytes([dirty[i], dirty[i + 1]]) as i32;
                let want = base(x, y) as i32;
                assert!((got - want).abs() <= 1, "pixel ({x},{y}): {got} vs {want}");
            }
        }
    }

    #[test]
    fn apply_u16_matches_apply_le() {
        let (w, h) = (96usize, 96usize);
        let base = |x: usize, y: usize| (3000 + 5 * x + 4 * y + (x * y) % 7) as u16;
        let mut le = vec![0u8; w * h * 2];
        let mut native = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                let v = base(x, y) + if x % 8 == 7 && y % 30 == 3 { 900 } else { 0 };
                le[(y * w + x) * 2..][..2].copy_from_slice(&v.to_le_bytes());
                native[y * w + x] = v;
            }
        }
        let fix = LatticeFix::new(8, 30, vec![(7, 3), (0, 6)], 10, 80, 10, 80);
        fix.apply_le(&mut le, w, h);
        fix.apply_u16(&mut native, w, h);
        let from_le: Vec<u16> = le.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(from_le, native);
    }

    #[test]
    fn camera_gate() {
        assert!(camera_has_focus_pixels("Canon EOS M"));
        assert!(camera_has_focus_pixels("Canon EOS 700D"));
        assert!(!camera_has_focus_pixels("Canon EOS 5D Mark III"));
        assert!(!camera_has_focus_pixels("CineFS"));
    }

    /// Fits the lattice to a `candidates.csv` written by `cinefs-cli focus-probe`.
    /// `CINEFS_TEST_CANDIDATES=/tmp/fp/candidates.csv CINEFS_TEST_DIMS=1408x2538 cargo test -p cinefs-core real_candidates -- --nocapture`
    #[test]
    fn real_candidates_fit() {
        let (Ok(path), Ok(dims)) = (std::env::var("CINEFS_TEST_CANDIDATES"), std::env::var("CINEFS_TEST_DIMS")) else {
            return;
        };
        let (w, h) = dims.split_once('x').map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap())).unwrap();
        let cands: Vec<Candidate> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                Some(Candidate { x: f[0].parse().ok()?, y: f[1].parse().ok()?, persistence: f[2].parse().ok()?, dark: f[3] == "dark" })
            })
            .collect();
        let fit = fit_lattice(&cands, w, h).expect("a lattice must be found in real data");
        println!(
            "period {}x{} cells {:?} region x {}..{} y {}..{} -> {} pixels ({} candidates)",
            fit.period_x, fit.period_y, fit.cells, fit.x0, fit.x1, fit.y0, fit.y1, fit.pixel_count(), cands.len()
        );
        assert_eq!((fit.period_x, fit.period_y), (8, 30));
        assert_eq!(fit.cells.len(), 8);
    }
}
