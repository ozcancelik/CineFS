//! Static hot-pixel and dead-pixel detection and repair in raw Bayer sensor space.
//!
//! Hardware sensor defects (stuck-bright "hot" pixels and stuck-dark "dead" pixels) stay fixed
//! at the exact same `(x, y)` photosite coordinates across every frame of a clip and are strictly
//! single-photosite isolated (their same-colour Bayer neighbours `2 px` away read normally).
//!
//! At clip open time, [`detect_clip`] scans two anchor frames to collect isolated outlier
//! candidates (zero cost if none exist), then verifies temporal persistence across up to 6 frames.
//! Clean clips with 0 defects return `None`, preserving zero-decode LJ92 passthrough.
//!
//! Per frame, [`BadPixelFix::apply_u16_ex`] / [`BadPixelFix::apply_le_ex`] repairs only the
//! detected coordinates (typically a handful to a few hundred pixels, taking microseconds) using
//! an edge-aware average of same-colour, same-ISO neighbours.

use crate::focus_fix::FocusFix;
use crate::focus_probe::decode_frame_u16;
use crate::mlv::MlvClip;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};

/// Hot/dead pixel correction mode for clips opened afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BadPixelMode {
    /// Disabled.
    Off = 0,
    /// Automatically detect and repair static hot & dead pixels per clip (default).
    Auto = 1,
}

impl BadPixelMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Auto => "Auto",
        }
    }
}

static BAD_PIXEL_MODE: AtomicU8 = AtomicU8::new(BadPixelMode::Auto as u8);

pub fn set_bad_pixel_mode(mode: BadPixelMode) {
    BAD_PIXEL_MODE.store(mode as u8, Ordering::Relaxed);
}

pub fn bad_pixel_mode() -> BadPixelMode {
    BadPixelMode::from_u8(BAD_PIXEL_MODE.load(Ordering::Relaxed))
}

/// Static bad-pixel map detected for a single clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadPixelFix {
    /// Defect coordinates `(x, y)` in raw sensor space, sorted in row-major order.
    pub coords: Vec<(u32, u32)>,
    pub hot_count: usize,
    pub dead_count: usize,
    set: HashSet<(u32, u32)>,
}

impl BadPixelFix {
    pub fn new(mut coords: Vec<(u32, u32)>, hot_count: usize, dead_count: usize) -> Self {
        coords.sort_unstable_by_key(|&(x, y)| (y, x));
        coords.dedup();
        let set: HashSet<(u32, u32)> = coords.iter().copied().collect();
        Self { coords, hot_count, dead_count, set }
    }

    #[inline]
    pub fn total_count(&self) -> usize {
        self.coords.len()
    }

    #[inline]
    pub fn is_defect(&self, x: u32, y: u32) -> bool {
        self.set.contains(&(x, y))
    }

    /// Repairs bad pixels in place on host-order `u16` samples (GPU player / thumbnail path).
    pub fn apply_u16_ex(&self, buf: &mut [u16], w: usize, h: usize, dual_iso: bool) {
        if buf.len() < w * h || w < 9 || h < 9 || self.coords.is_empty() {
            return;
        }
        let vd_base = if dual_iso { 4usize } else { 2usize };
        for &(dx, dy) in &self.coords {
            let (x, y) = (dx as usize, dy as usize);
            if x >= w || y >= h {
                continue;
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
                let vd = if dual_iso { vd_base } else { d };
                consider(y.checked_sub(vd).map(|ny| (x, ny)), (y + vd < h).then_some((x, y + vd)));
                if best.is_some() {
                    break;
                }
            }
            if let Some((_, avg)) = best {
                buf[y * w + x] = avg.clamp(0, 65535) as u16;
            }
        }
    }

    /// Repairs bad pixels in place on little-endian `u16` bytes (DNG path).
    pub fn apply_le_ex(&self, buf: &mut [u8], w: usize, h: usize, dual_iso: bool) {
        if buf.len() < w * h * 2 || w < 9 || h < 9 || self.coords.is_empty() {
            return;
        }
        let get = |b: &[u8], x: usize, y: usize| -> i32 {
            let i = (y * w + x) * 2;
            u16::from_le_bytes([b[i], b[i + 1]]) as i32
        };
        let vd_base = if dual_iso { 4usize } else { 2usize };
        for &(dx, dy) in &self.coords {
            let (x, y) = (dx as usize, dy as usize);
            if x >= w || y >= h {
                continue;
            }
            let mut best: Option<(i32, i32)> = None;
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
                consider(x.checked_sub(d).map(|nx| (nx, y)), (x + d < w).then_some((x + d, y)));
                let vd = if dual_iso { vd_base } else { d };
                consider(y.checked_sub(vd).map(|ny| (x, ny)), (y + vd < h).then_some((x, y + vd)));
                if best.is_some() {
                    break;
                }
            }
            if let Some((_, avg)) = best {
                let i = (y * w + x) * 2;
                buf[i..i + 2].copy_from_slice(&(avg.clamp(0, 65535) as u16).to_le_bytes());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PixelKind {
    Hot,
    Dead,
}

#[inline]
fn classify_pixel(
    px: &[u16],
    w: usize,
    x: usize,
    y: usize,
    vd: usize,
    black: i32,
    hot_thresh: i32,
    dead_thresh: i32,
    dead_margin: i32,
) -> Option<PixelKind> {
    let v = px[y * w + x] as i32;
    let a = px[y * w + (x - 2)] as i32;
    let b = px[y * w + (x + 2)] as i32;
    let c = px[(y - vd) * w + x] as i32;
    let d = px[(y + vd) * w + x] as i32;

    let mut nb = [a, b, c, d];
    nb.sort_unstable();
    let nb_min = nb[0];
    let nb_max = nb[3];
    let nb_med = (nb[1] + nb[2]) / 2;
    let spread = nb_max - nb_min;

    if v > nb_max {
        let diff_max = v - nb_max;
        let diff_med = v - nb_med;
        let need_max = hot_thresh.max((spread * 5) / 2);
        let need_med = ((hot_thresh * 5) / 4).max(spread * 3);
        if diff_max >= need_max && diff_med >= need_med {
            return Some(PixelKind::Hot);
        }
    } else if v <= black + dead_margin && nb_min >= black + dead_thresh {
        let diff_min = nb_min - v;
        let need_min = dead_thresh.max(spread * 2);
        if diff_min >= need_min {
            return Some(PixelKind::Dead);
        }
    }
    None
}

/// Detects static hot and dead pixels from a slice of decoded frames (`w * h` u16 samples each).
///
/// Uses the first and middle frames as anchors to discover isolated outlier candidates, then
/// verifies temporal persistence across all provided frames.
pub fn detect_from_frames(
    frames: &[&[u16]],
    w: usize,
    h: usize,
    black: u32,
    white: u32,
    dual_iso: bool,
    focus_fix: Option<&FocusFix>,
) -> Option<BadPixelFix> {
    if frames.is_empty() || w < 16 || h < 16 || white <= black + 64 {
        return None;
    }
    let vd = if dual_iso { 4usize } else { 2usize };
    if h <= vd * 2 + 2 {
        return None;
    }

    let range = (white - black) as f32;
    let single_frame = frames.len() == 1;
    let mult = if single_frame { 2.0f32 } else { 1.0f32 };
    let hot_thresh = ((range * 0.035).clamp(24.0, 420.0) * mult) as i32;
    let dead_thresh = ((range * 0.035).clamp(24.0, 400.0) * mult) as i32;
    let dead_margin = (range * 0.008).clamp(8.0, 80.0) as i32;
    let black_i = black as i32;

    // Anchor frames to collect candidates quickly without scanning all frames end-to-end.
    let anchor_indices: Vec<usize> = if frames.len() == 1 {
        vec![0]
    } else {
        vec![0, frames.len() / 2]
    };
    let mut candidates: HashMap<(u32, u32), PixelKind> = HashMap::new();
    let max_candidates = ((w * h) / 400).clamp(2000, 16000);

    for &fi in &anchor_indices {
        let px = frames[fi];
        if px.len() < w * h {
            continue;
        }
        for y in vd..(h - vd) {
            for x in 2..(w - 2) {
                if let Some(ff) = focus_fix {
                    if ff.is_defect(x as u32, y as u32) {
                        continue;
                    }
                }
                if let Some(kind) = classify_pixel(px, w, x, y, vd, black_i, hot_thresh, dead_thresh, dead_margin) {
                    candidates.entry((x as u32, y as u32)).or_insert(kind);
                    if candidates.len() > max_candidates {
                        // Too many triggers (e.g. extreme synthetic noise/aliasing) — bail safely.
                        return None;
                    }
                }
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    // Verify temporal persistence across all sampled frames.
    let n_frames = frames.len();
    let min_hits = match n_frames {
        1 => 1,
        2 => 2,
        _ => (n_frames * 4).div_ceil(5), // >= 80% of sampled frames
    };

    let mut coords = Vec::new();
    let mut hot_count = 0usize;
    let mut dead_count = 0usize;

    for (&(cx, cy), &kind) in &candidates {
        let mut hits = 0usize;
        for &px in frames {
            if px.len() < w * h {
                continue;
            }
            if classify_pixel(px, w, cx as usize, cy as usize, vd, black_i, hot_thresh, dead_thresh, dead_margin) == Some(kind) {
                hits += 1;
            }
        }
        if hits >= min_hits {
            coords.push((cx, cy));
            match kind {
                PixelKind::Hot => hot_count += 1,
                PixelKind::Dead => dead_count += 1,
            }
        }
    }

    if coords.is_empty() || coords.len() > 2500 {
        return None;
    }
    Some(BadPixelFix::new(coords, hot_count, dead_count))
}

/// Detects static hot and dead pixels for `clip` when [`BadPixelMode::Auto`] is active.
/// Works across `.MLV`, `.mcraw`, and `.braw` clips.
pub fn detect_clip(clip: &MlvClip) -> Option<BadPixelFix> {
    if bad_pixel_mode() == BadPixelMode::Off {
        return None;
    }
    let (w, h) = (clip.width as usize, clip.height as usize);
    let n = clip.frames.len();
    if n == 0 || w < 16 || h < 16 {
        return None;
    }

    let want = n.min(6);
    let picks: Vec<usize> = if want == 1 {
        vec![0]
    } else {
        (0..want).map(|i| i * (n - 1) / (want - 1)).collect()
    };

    // Fast path: decode the two anchor frames first; if neither has any isolated defect candidate,
    // skip decoding the remaining frames completely.
    let dual_iso = clip.dual_iso.is_some();
    let ff = clip.focus_fix.as_deref();
    let mut decoded: Vec<Vec<u16>> = Vec::with_capacity(picks.len());
    for &idx in &picks {
        if let Ok(px) = decode_frame_u16(clip, idx) {
            decoded.push(px);
        }
    }
    if decoded.is_empty() {
        return None;
    }
    let slices: Vec<&[u16]> = decoded.iter().map(|v| v.as_slice()).collect();
    let fix = detect_from_frames(
        &slices,
        w,
        h,
        clip.levels.black,
        clip.levels.white,
        dual_iso,
        ff,
    );
    if let Some(f) = &fix {
        crate::log::write(
            "INFO",
            &format!(
                "clip '{}': bad pixels detected: {} hot, {} dead -> {} pixels corrected",
                clip.base_name,
                f.hot_count,
                f.dead_count,
                f.total_count()
            ),
        );
    }
    fix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_frame(w: usize, h: usize, base: u16) -> Vec<u16> {
        let mut px = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                px[y * w + x] = base + ((x * 12 + y * 8) as u16);
            }
        }
        px
    }

    #[test]
    fn detects_and_repairs_static_hot_and_dead_pixels() {
        let (w, h) = (96usize, 64usize);
        let (black, white) = (2048u32, 15000u32);
        let mut frames: Vec<Vec<u16>> = (0..6).map(|i| gradient_frame(w, h, 3500 + i * 200)).collect();
        // Inject 2 static hot pixels and 1 static dead pixel
        for f in &mut frames {
            f[20 * w + 30] = 14500;
            f[41 * w + 55] = 14900;
            f[32 * w + 44] = 2048; // dead (stuck at black)
        }
        // Inject a transient 1-frame specular glint (must NOT be flagged)
        frames[2][15 * w + 15] = 14800;

        let slices: Vec<&[u16]> = frames.iter().map(|v| v.as_slice()).collect();
        let fix = detect_from_frames(&slices, w, h, black, white, false, None)
            .expect("should detect static hot & dead pixels");
        assert_eq!(fix.hot_count, 2);
        assert_eq!(fix.dead_count, 1);
        assert!(fix.is_defect(30, 20));
        assert!(fix.is_defect(55, 41));
        assert!(fix.is_defect(44, 32));
        assert!(!fix.is_defect(15, 15));

        let mut test_frame = frames[0].clone();
        let expected = gradient_frame(w, h, 3500);
        fix.apply_u16_ex(&mut test_frame, w, h, false);
        for &(x, y) in &[(30usize, 20usize), (55, 41), (44, 32)] {
            let got = test_frame[y * w + x] as i32;
            let want = expected[y * w + x] as i32;
            assert!((got - want).abs() <= 2, "pixel ({x},{y}): got={got} want={want}");
        }
    }

    #[test]
    fn clean_frames_return_none() {
        let (w, h) = (96usize, 64usize);
        let frames: Vec<Vec<u16>> = (0..4).map(|i| gradient_frame(w, h, 3500 + i * 100)).collect();
        let slices: Vec<&[u16]> = frames.iter().map(|v| v.as_slice()).collect();
        assert!(detect_from_frames(&slices, w, h, 2048, 15000, false, None).is_none());
    }
}
