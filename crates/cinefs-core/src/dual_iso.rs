//! Magic Lantern Dual ISO detection, calibration, and real-time Bayer-domain recovery.
//!
//! Magic Lantern's Dual ISO module alternates two analog ISO gains on pairs of sensor rows
//! (`y % 4` in `{0, 1}` vs `{2, 3}` so each pair contains a full 2×2 Bayer quad).
//!
//! At clip open time, [`calibrate_clip`] checks the optional `DISO` MLV block and verifies
//! the alternating-row gain ratio directly on sampled frames (so normal single-ISO clips are
//! never touched, while Dual ISO clips without a `DISO` block are still detected). It measures:
//! - which row-pair carries the high ISO (`bright_pair` = 0 or 2),
//! - per-CFA-phase gain ratios `[f32; 4]` (eliminating slight R/G/B gain mismatch between ISOs),
//! - separate low-ISO and high-ISO black levels (`black_lo`, `black_hi`).
//!
//! Per frame, [`DualIsoCalibration::apply_u16`] / [`DualIsoCalibration::apply_le`] reconstructs
//! a clean 16-bit linear Bayer frame (`0..65535`, low-ISO exposure scale, `BlackLevel = 0`,
//! `WhiteLevel = 65535`) using edge-directed same-colour interpolation and smooth shadow/midtone/
//! highlight blending — preserving full vertical resolution in midtones while running in a few
//! milliseconds per frame for real-time VFS and GPU playback.

use crate::focus_probe::decode_frame_u16;
use crate::mlv::MlvClip;
use std::sync::atomic::{AtomicU8, Ordering};

/// Dual ISO processing mode for clips opened afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DualIsoMode {
    /// Disabled: leave raw pixels untouched.
    Off = 0,
    /// Automatically detect and recover Dual ISO clips per clip (default).
    Auto = 1,
}

impl DualIsoMode {
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

static DUAL_ISO_MODE: AtomicU8 = AtomicU8::new(DualIsoMode::Auto as u8);

pub fn set_dual_iso_mode(mode: DualIsoMode) {
    DUAL_ISO_MODE.store(mode as u8, Ordering::Relaxed);
}

pub fn dual_iso_mode() -> DualIsoMode {
    DualIsoMode::from_u8(DUAL_ISO_MODE.load(Ordering::Relaxed))
}

/// Per-clip Dual ISO calibration parameters measured in raw sensor space.
#[derive(Clone, Debug)]
pub struct DualIsoCalibration {
    /// For each `y % 4` in `0..4`, `true` if that row carries the high-ISO (bright) exposure.
    /// Always has two adjacent rows (modulo 4) set to `true` (`{0,1}`, `{1,2}`, `{2,3}`, or `{3,0}`),
    /// so `y ± 2` is always the opposite ISO of the same Bayer colour.
    pub is_hi_row: [bool; 4],
    /// Measured `high_iso / low_iso` linear gain for each Bayer phase `(y & 1) * 2 + (x & 1)`.
    pub gain: [f32; 4],
    /// Global median gain ratio (> 1.0).
    pub global_gain: f32,
    /// Measured EV separation (`log2(global_gain)`).
    pub ev_stops: f32,
    /// Black level for low-ISO rows.
    pub black_lo: f32,
    /// Black level for high-ISO rows.
    pub black_hi: f32,
    /// White level in raw sensor units (low-ISO ceiling).
    pub white: f32,
    /// Detected high-ISO clipping level (may be below `white` to prevent pink highlights).
    pub white_hi: f32,
    /// Scale factor mapping low-ISO linear `(0 .. white - black_lo)` into `0 .. 65535`.
    pub scale_to_u16: f32,
}

#[inline]
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn median_f32(vals: &mut [f32]) -> Option<f32> {
    if vals.is_empty() {
        return None;
    }
    let mid = vals.len() / 2;
    vals.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    Some(vals[mid])
}

/// Calibrates Dual ISO parameters from a slice of decoded raw frames (`w * h` u16 samples each).
/// Returns `None` if the frames do not exhibit a consistent Dual ISO alternating-row gain ratio.
pub fn calibrate_from_frames(
    frames: &[&[u16]],
    w: usize,
    h: usize,
    black: f32,
    white: f32,
    has_diso_block: bool,
) -> Option<DualIsoCalibration> {
    if frames.is_empty() || w < 16 || h < 16 || white <= black + 64.0 {
        return None;
    }

    let range = white - black;
    let hi_safe = white - (range * 0.08).clamp(32.0, 256.0);
    let lo_min = black + (range * 0.004).clamp(4.0, 20.0);

    // Subsample evenly across the frame so calibration is fast (~1 ms) even on 5K frames.
    let step_x = ((w / 160).max(1)) * 2; // even step preserves CFA x-parity
    let step_y = ((h / 96).max(1)) * 4;  // multiple of 4 preserves row-pair parity

    // Vote independently on even rows (r=0 vs r=2) and odd rows (r=1 vs r=3).
    // This handles all four possible 2-row alignments: {0,1}, {1,2}, {2,3}, and {3,0}.
    let mut votes_hi = [0usize; 2]; // [0]: count(r=0 > r=2), [1]: count(r=1 > r=3)
    let mut votes_lo = [0usize; 2]; // [0]: count(r=2 > r=0), [1]: count(r=3 > r=1)
    let mut pairs_per_phase: [Vec<(f32, f32)>; 4] = Default::default();

    for &px in frames {
        if px.len() < w * h {
            continue;
        }
        let mut y = 4usize;
        while y + 3 < h - 4 {
            for dy in 0..2usize {
                let ya = y + dy;     // y % 4 == dy (0 or 1)
                let yb = y + dy + 2; // y % 4 == dy + 2 (2 or 3)
                let mut x = 4usize;
                while x + 1 < w - 4 {
                    for dx in 0..2usize {
                        let xx = x + dx;
                        let va = px[ya * w + xx] as f32;
                        let vb = px[yb * w + xx] as f32;
                        if va >= hi_safe || vb >= hi_safe {
                            continue;
                        }
                        if va <= lo_min && vb <= lo_min {
                            continue;
                        }
                        let da = (va - black).max(1.0);
                        let db = (vb - black).max(1.0);
                        if da > db * 1.15 {
                            votes_hi[dy] += 1;
                        } else if db > da * 1.15 {
                            votes_lo[dy] += 1;
                        }
                        let phase = (dy << 1) | dx;
                        pairs_per_phase[phase].push((va, vb));
                    }
                    x += step_x;
                }
            }
            y += step_y;
        }
    }

    let tot0 = votes_hi[0] + votes_lo[0];
    let tot1 = votes_hi[1] + votes_lo[1];
    if tot0 < 40 || tot1 < 40 {
        return None;
    }
    let r0_is_hi = votes_hi[0] >= votes_lo[0];
    let r1_is_hi = votes_hi[1] >= votes_lo[1];
    let cons0 = votes_hi[0].max(votes_lo[0]) as f32 / tot0 as f32;
    let cons1 = votes_hi[1].max(votes_lo[1]) as f32 / tot1 as f32;
    let consistency = cons0.min(cons1);
    let min_consistency = if has_diso_block { 0.72 } else { 0.85 };
    if consistency < min_consistency {
        return None;
    }
    let is_hi_row = [r0_is_hi, r1_is_hi, !r0_is_hi, !r1_is_hi];

    let mut phase_gains = [0.0f32; 4];
    let mut all_ratios = Vec::new();
    let min_d_lo = (range * 0.004).clamp(4.0, 20.0);
    for phase in 0..4 {
        let dy = phase >> 1;
        let a_is_hi = is_hi_row[dy];
        let mut ratios = Vec::new();
        for &(va, vb) in &pairs_per_phase[phase] {
            let (v_hi, v_lo) = if a_is_hi { (va, vb) } else { (vb, va) };
            let d_lo = v_lo - black;
            let d_hi = v_hi - black;
            if d_lo > min_d_lo && d_hi > d_lo && v_hi < hi_safe {
                let r = d_hi / d_lo;
                if r.is_finite() && r > 1.0 && r < 64.0 {
                    ratios.push(r);
                    all_ratios.push(r);
                }
            }
        }
        phase_gains[phase] = median_f32(&mut ratios).unwrap_or(0.0);
    }

    let global_gain = median_f32(&mut all_ratios)?;
    let ev_stops = global_gain.log2();
    let min_ev = if has_diso_block { 0.35 } else { 0.55 };
    if !ev_stops.is_finite() || ev_stops < min_ev {
        return None;
    }

    // Keep per-phase gains tightly bounded around the global gain so a single noisy channel
    // cannot introduce a colour cast, while still cancelling real 1–3% Canon ISO WB shifts.
    for g in &mut phase_gains {
        if *g <= 0.0 {
            *g = global_gain;
        } else {
            *g = g.clamp(global_gain * 0.92, global_gain * 1.08);
        }
    }

    // Estimate high-ISO black offset relative to low-ISO black using dark-mid pixels:
    //   (v_hi - black_hi) ≈ gain * (v_lo - black_lo)
    //   => black_hi - black_lo ≈ median(v_hi - black_lo - gain * (v_lo - black_lo))
    let mut black_diffs = Vec::new();
    let dark_cap = black + range * 0.08;
    for phase in 0..4 {
        let dy = phase >> 1;
        let a_is_hi = is_hi_row[dy];
        let g = phase_gains[phase];
        for &(va, vb) in &pairs_per_phase[phase] {
            let (v_hi, v_lo) = if a_is_hi { (va, vb) } else { (vb, va) };
            if v_lo >= black && v_lo <= dark_cap && v_hi < hi_safe {
                let diff = (v_hi - black) - g * (v_lo - black);
                if diff.is_finite() && diff.abs() < 256.0 {
                    black_diffs.push(diff);
                }
            }
        }
    }
    let delta_black = median_f32(&mut black_diffs).unwrap_or(0.0).clamp(-64.0, 64.0);
    let black_lo = black;
    let black_hi = (black + delta_black).max(0.0);
    let white_range = (white - black_lo).max(1.0);

    // Detect the true high-ISO clipping point (some Canon bodies clip high-ISO rows below
    // header `white_level`, which would otherwise cause pink highlights). Discard the brightest
    // outlier pixels and apply a safety margin only when the frame actually approaches clipping.
    let mut hi_samples: Vec<u16> = Vec::new();
    for &px in frames {
        if px.len() < w * h {
            continue;
        }
        let mut y = 0usize;
        while y < h {
            if is_hi_row[y & 3] {
                let row = y * w;
                let mut x = 0usize;
                while x < w {
                    hi_samples.push(px[row + x]);
                    x += 3;
                }
            }
            y += 3;
        }
    }
    let white_hi = if hi_samples.len() > 64 {
        let discard = (50 * frames.len()).min(hi_samples.len() / 20).max(10);
        let idx = hi_samples.len().saturating_sub(1 + discard);
        hi_samples.select_nth_unstable(idx);
        let p_hi_max = hi_samples[idx] as f32;
        if p_hi_max >= black_hi + range * 0.70 {
            let margin = (range * 0.08).clamp(32.0, 1200.0);
            (p_hi_max - margin).clamp(black_hi + range * 0.50, white)
        } else {
            white
        }
    } else {
        white
    };

    Some(DualIsoCalibration {
        is_hi_row,
        gain: phase_gains,
        global_gain,
        ev_stops,
        black_lo,
        black_hi,
        white,
        white_hi,
        scale_to_u16: 65535.0 / white_range,
    })
}

/// Detects and calibrates Dual ISO for `clip` if Dual ISO mode is enabled.
/// Returns `None` for normal single-ISO clips, codec-decoded clips, or when Dual ISO is off.
pub fn calibrate_clip(clip: &MlvClip) -> Option<DualIsoCalibration> {
    calibrate_clip_with_mode(clip, dual_iso_mode())
}

/// Same as [`calibrate_clip`] with an explicit [`DualIsoMode`] (used for live per-clip overrides).
pub fn calibrate_clip_with_mode(clip: &MlvClip, mode: DualIsoMode) -> Option<DualIsoCalibration> {
    if mode == DualIsoMode::Off || clip.is_codec_decoded() {
        return None;
    }
    let (w, h) = (clip.width as usize, clip.height as usize);
    let n = clip.frames.len();
    if n == 0 || w < 16 || h < 16 {
        return None;
    }

    let picks: Vec<usize> = match n {
        1 => vec![0],
        2 => vec![0, 1],
        _ => vec![0, n / 2, n - 1],
    };
    let mut decoded: Vec<Vec<u16>> = Vec::with_capacity(picks.len());
    for idx in picks {
        if let Ok(px) = decode_frame_u16(clip, idx) {
            decoded.push(px);
        }
    }
    if decoded.is_empty() {
        return None;
    }
    let slices: Vec<&[u16]> = decoded.iter().map(|v| v.as_slice()).collect();
    let has_diso = clip.diso_mode > 0;
    let cal = calibrate_from_frames(
        &slices,
        w,
        h,
        clip.levels.black as f32,
        clip.levels.white as f32,
        has_diso,
    );
    if let Some(c) = &cal {
        crate::log::write(
            "INFO",
            &format!(
                "clip '{}': Dual ISO detected (DISO={}/{}): +{:.2} EV (gain {:.2}x [{:.2},{:.2},{:.2},{:.2}], hi_rows={:?}, black lo={:.1} hi={:.1})",
                clip.base_name,
                clip.diso_mode,
                clip.diso_iso_value,
                c.ev_stops,
                c.global_gain,
                c.gain[0],
                c.gain[1],
                c.gain[2],
                c.gain[3],
                c.is_hi_row,
                c.black_lo,
                c.black_hi,
            ),
        );
    }
    cal
}

impl DualIsoCalibration {
    /// Edge-directed vertical/diagonal interpolation of a same-colour pixel from the adjacent
    /// opposite-ISO rows `y - 2` and `y + 2`.
    #[inline]
    fn interp_other_row(src: &[u16], w: usize, h: usize, x: usize, y: usize) -> f32 {
        let has_up = y >= 2;
        let has_dn = y + 2 < h;
        match (has_up, has_dn) {
            (true, true) => {
                let up = (y - 2) * w;
                let dn = (y + 2) * w;
                let vu = src[up + x] as f32;
                let vd = src[dn + x] as f32;
                if x >= 2 && x + 2 < w {
                    let gv = (vu - vd).abs();
                    let ul = src[up + x - 2] as f32;
                    let dr = src[dn + x + 2] as f32;
                    let g1 = (ul - dr).abs();
                    let ur = src[up + x + 2] as f32;
                    let dl = src[dn + x - 2] as f32;
                    let g2 = (ur - dl).abs();
                    // Pick the direction with the smallest gradient (strongly suppresses zippering
                    // along diagonal edges while keeping clean vertical averages on flat areas).
                    if gv <= g1 && gv <= g2 {
                        0.5 * (vu + vd)
                    } else if g1 <= g2 {
                        0.5 * (ul + dr)
                    } else {
                        0.5 * (ur + dl)
                    }
                } else {
                    0.5 * (vu + vd)
                }
            }
            (true, false) => src[(y - 2) * w + x] as f32,
            (false, true) => src[(y + 2) * w + x] as f32,
            (false, false) => src[y * w + x] as f32,
        }
    }

    /// Reconstructs a Dual ISO Bayer frame in-place into 16-bit linear (`0..65535`, `BlackLevel = 0`).
    pub fn apply_u16(&self, buf: &mut [u16], w: usize, h: usize) {
        if buf.len() < w * h || w < 8 || h < 8 {
            return;
        }
        let src = buf[..w * h].to_vec();
        let hi_clip = (self.white_hi - self.black_hi).max(1.0);
        // High-ISO row transition: own high-ISO in shadows/midtones, interpolated low-ISO in highlights.
        let hi_t0 = 0.50 * hi_clip;
        let hi_t1 = 0.88 * hi_clip;
        // Low-ISO row transition: interpolated high-ISO in deep shadows, own low-ISO in midtones/highlights.
        let lo_t0 = 0.22 * hi_clip;
        let lo_t1 = 0.52 * hi_clip;

        for y in 0..h {
            let is_hi_row = self.is_hi_row[y & 3];
            let row_off = y * w;
            let y_phase = (y & 1) << 1;

            if is_hi_row {
                for x in 0..w {
                    let g = self.gain[y_phase | (x & 1)];
                    let raw_hi = (src[row_off + x] as f32 - self.black_hi).max(0.0);
                    let v_hi = raw_hi / g;
                    let lin = if raw_hi <= hi_t0 {
                        v_hi
                    } else {
                        let lo_raw = (Self::interp_other_row(&src, w, h, x, y) - self.black_lo).max(0.0);
                        if raw_hi >= hi_t1 {
                            lo_raw
                        } else {
                            let w_lo = smoothstep(hi_t0, hi_t1, raw_hi);
                            v_hi * (1.0 - w_lo) + lo_raw * w_lo
                        }
                    };
                    buf[row_off + x] = (lin * self.scale_to_u16 + 0.5).clamp(0.0, 65535.0) as u16;
                }
            } else {
                for x in 0..w {
                    let g = self.gain[y_phase | (x & 1)];
                    let v_lo = (src[row_off + x] as f32 - self.black_lo).max(0.0);
                    let hi_raw_interp = (Self::interp_other_row(&src, w, h, x, y) - self.black_hi).max(0.0);
                    let est_hi = hi_raw_interp.max(v_lo * g);
                    let lin = if est_hi >= lo_t1 {
                        v_lo
                    } else {
                        let v_hi = hi_raw_interp / g;
                        if est_hi <= lo_t0 {
                            v_hi
                        } else {
                            let w_lo = smoothstep(lo_t0, lo_t1, est_hi);
                            v_hi * (1.0 - w_lo) + v_lo * w_lo
                        }
                    };
                    buf[row_off + x] = (lin * self.scale_to_u16 + 0.5).clamp(0.0, 65535.0) as u16;
                }
            }
        }
    }

    /// Reconstructs a Dual ISO Bayer frame in-place on little-endian `u16` bytes (DNG path).
    pub fn apply_le(&self, buf: &mut [u8], w: usize, h: usize) {
        let pixels = w * h;
        if buf.len() < pixels * 2 || w < 8 || h < 8 {
            return;
        }
        let mut tmp: Vec<u16> = buf[..pixels * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        self.apply_u16(&mut tmp, w, h);
        for (dst, &v) in buf[..pixels * 2].chunks_exact_mut(2).zip(tmp.iter()) {
            dst.copy_from_slice(&v.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_dual_iso_frame(w: usize, h: usize, gain: f32, hi_rows: [bool; 4], black_lo: f32, black_hi: f32, white: f32) -> Vec<u16> {
        let mut px = vec![0u16; w * h];
        for y in 0..h {
            let is_hi = hi_rows[y & 3];
            for x in 0..w {
                // Smooth horizontal ramp from deep shadows to bright highlights
                let scene = (x as f32 / (w - 1) as f32) * (white - black_lo);
                let val = if is_hi {
                    (black_hi + scene * gain).min(white)
                } else {
                    (black_lo + scene).min(white)
                };
                px[y * w + x] = val.round().clamp(0.0, 65535.0) as u16;
            }
        }
        px
    }

    #[test]
    fn detects_and_recovers_synthetic_dual_iso_all_four_phases() {
        let (w, h) = (256usize, 128usize);
        let (black_lo, black_hi, white) = (2048.0f32, 2052.0f32, 15000.0f32);
        let gain = 8.0f32; // +3 EV (e.g. ISO 100 / 800)
        for hi_rows in [
            [true, true, false, false],
            [false, true, true, false],
            [false, false, true, true],
            [true, false, false, true],
        ] {
            let mut frame = synth_dual_iso_frame(w, h, gain, hi_rows, black_lo, black_hi, white);
            let cal = calibrate_from_frames(&[&frame], w, h, black_lo, white, false)
                .unwrap_or_else(|| panic!("should detect +3 EV Dual ISO for {hi_rows:?}"));
            assert_eq!(cal.is_hi_row, hi_rows);
            assert!((cal.global_gain - gain).abs() < 0.25, "gain={}", cal.global_gain);
            assert!((cal.ev_stops - 3.0).abs() < 0.08, "ev={}", cal.ev_stops);
            assert!((cal.black_hi - black_hi).abs() < 4.0, "black_hi={}", cal.black_hi);

            cal.apply_u16(&mut frame, w, h);
            for x in (8..w - 8).step_by(8) {
                let v0 = frame[60 * w + x] as i32;
                let v2 = frame[62 * w + x] as i32;
                assert!((v0 - v2).abs() < 120, "banding at x={x}: row60={v0} vs row62={v2}");
            }
            assert!(frame[60 * w + (w - 6)] > 60000);
            assert!(frame[62 * w + (w - 6)] > 60000);
        }
    }

    #[test]
    fn normal_single_iso_frame_is_not_detected() {
        let (w, h) = (256usize, 128usize);
        let frame = synth_dual_iso_frame(w, h, 1.0, [true, true, false, false], 2048.0, 2048.0, 15000.0);
        assert!(calibrate_from_frames(&[&frame], w, h, 2048.0, 15000.0, false).is_none());
    }

    #[test]
    fn inspect_real_dual_iso_clip_when_set() {
        let Ok(path) = std::env::var("CINEFS_TEST_DISO") else { return };
        let clip = crate::mlv::MlvClip::open(std::path::Path::new(&path)).expect("open clip");
        let px = decode_frame_u16(&clip, 0).expect("decode frame 0");
        let (w, h) = (clip.width as usize, clip.height as usize);
        let mut sorted = px.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        println!(
            "clip={} {}x{} bpp={} black={} white={} diso={}/{}",
            clip.base_name, w, h, clip.bits_per_pixel, clip.levels.black, clip.levels.white, clip.diso_mode, clip.diso_iso_value
        );
        println!(
            "px percentiles: min={} p1={} p10={} p50={} p90={} p99={} max={}",
            sorted[0], sorted[n/100], sorted[n/10], sorted[n/2], sorted[n*9/10], sorted[n*99/100], sorted[n-1]
        );
        for rem in 0..4usize {
            let mut sum = 0u64;
            let mut cnt = 0u64;
            let mut mx = 0u16;
            for y in (rem..h).step_by(4) {
                for x in 0..w {
                    let v = px[y * w + x];
                    sum += v as u64;
                    cnt += 1;
                    if v > mx { mx = v; }
                }
            }
            println!("  y%4=={rem}: mean={:.1} max={mx}", sum as f64 / cnt.max(1) as f64);
        }
        println!("dual_iso calibrated: {:?}", clip.dual_iso);
    }
}
