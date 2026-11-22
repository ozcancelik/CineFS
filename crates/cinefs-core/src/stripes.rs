//! Vertical stripe (8-column Canon ADC readout banding) detection and correction.
//!
//! Canon sensors read each row through 8 parallel ADC channels (`x % 8`). Slight per-channel
//! gain and black-offset mismatches appear as repeating vertical stripes — especially when
//! lifting shadows, shooting high ISO, or using `crop_rec` modes.
//!
//! Unlike single-frame multiplicative-only methods, [`calibrate_clip`] models all 16 Bayer
//! phases `((y & 1) << 3) | (x & 7)` across 4 independent colour groups (`R`, `G1`, `G2`, `B`),
//! measuring both **multiplicative gain** (mid/bright pixels) and **additive offset** (dark
//! pixels) from low-gradient horizontal neighbours across multiple sampled frames, with
//! zero-sum loop closure per colour group.
//!
//! In [`StripeMode::Auto`] (default), clean clips whose measured striping is below the visible
//! threshold return `None`, preserving zero-decode LJ92 passthrough. [`StripeMode::Force`]
//! applies the calibrated correction regardless of threshold.

use crate::focus_probe::decode_frame_u16;
use crate::mlv::MlvClip;
use std::sync::atomic::{AtomicU8, Ordering};

/// Vertical stripe correction mode for clips opened afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StripeMode {
    /// Disabled.
    Off = 0,
    /// Automatically detect and correct visible 8-column vertical stripes (default).
    Auto = 1,
    /// Always calibrate and apply 8-column vertical stripe correction on MLV clips.
    Force = 2,
}

impl StripeMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            2 => Self::Force,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Auto => "Auto",
            Self::Force => "Force",
        }
    }
}

static STRIPE_MODE: AtomicU8 = AtomicU8::new(StripeMode::Auto as u8);

pub fn set_stripe_mode(mode: StripeMode) {
    STRIPE_MODE.store(mode as u8, Ordering::Relaxed);
}

pub fn stripe_mode() -> StripeMode {
    StripeMode::from_u8(STRIPE_MODE.load(Ordering::Relaxed))
}

/// Calibrated 16-phase (2 rows × 8 columns) vertical stripe correction parameters.
#[derive(Clone, Debug)]
pub struct StripeFix {
    /// Multiplicative gain correction per phase `((y & 1) << 3) | (x & 7)`.
    pub gain: [f32; 16],
    /// Additive offset correction (in raw units) per phase `((y & 1) << 3) | (x & 7)`.
    pub offset: [f32; 16],
    /// Black level used as the linear pivot (`black + (v - black) * gain + offset`).
    pub black: f32,
    /// White level ceiling (clipped pixels at or above effective white are left untouched).
    pub white: f32,
    /// Maximum gain deviation from 1.0, in percent (e.g. `1.2` = 1.2%).
    pub max_gain_pct: f32,
    /// Maximum absolute offset correction, in raw counts.
    pub max_offset: f32,
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

/// Calibrates 16-phase vertical stripe correction from decoded raw frames (`w * h` u16 samples).
/// When `force` is `false`, returns `None` if no visible striping is present.
pub fn calibrate_from_frames(
    frames: &[&[u16]],
    w: usize,
    h: usize,
    black: f32,
    white: f32,
    force: bool,
) -> Option<StripeFix> {
    if frames.is_empty() || w < 24 || h < 8 || white <= black + 64.0 {
        return None;
    }
    let range = white - black;
    let hi_safe = white - (range * 0.08).clamp(32.0, 400.0);
    let mid_lo = (range * 0.04).clamp(24.0, 200.0);
    let dark_cap = (range * 0.10).clamp(64.0, 800.0);

    // Subsample vertically for speed (~1-2 ms total across 4 frames).
    let step_y = ((h / 160).max(1)) * 2; // even step preserves y & 1
    let step_x = ((w / 320).max(1)) * 8; // multiple of 8 preserves x & 7

    // For each of 4 Bayer colour groups g = (y&1)*2 + (x&1), and each adjacent pair c -> (c+1)%4:
    let mut log_ratios: [[Vec<f32>; 4]; 4] = Default::default();
    let mut dark_diffs: [[Vec<f32>; 4]; 4] = Default::default();

    for &px in frames {
        if px.len() < w * h {
            continue;
        }
        let mut y = 2usize;
        while y + 1 < h - 2 {
            for dy in 0..2usize {
                let yy = y + dy;
                let row = yy * w;
                let mut x_blk = 8usize;
                while x_blk + 10 < w {
                    for c in 0..4usize {
                        for dx in 0..2usize {
                            let x0 = x_blk + c * 2 + dx;
                            let x1 = x0 + 2;
                            let x_prev = x0 - 2;
                            let x_next = x1 + 2;
                            let v0 = px[row + x0] as f32;
                            let v1 = px[row + x1] as f32;
                            if v0 >= hi_safe || v1 >= hi_safe {
                                continue;
                            }
                            let vp = px[row + x_prev] as f32;
                            let vn = px[row + x_next] as f32;
                            // Smoothness gate: reject strong horizontal edges/texture so scene
                            // gradients do not bias the column-to-column ratio.
                            let local_grad = (vp - vn).abs();
                            let d0 = v0 - black;
                            let d1 = v1 - black;
                            let g_idx = (dy << 1) | dx;

                            if d0 > mid_lo && d1 > mid_lo {
                                let avg = 0.5 * (d0 + d1);
                                if local_grad <= avg * 0.12 + 32.0 {
                                    let r = d1 / d0;
                                    if r > 0.85 && r < 1.18 {
                                        log_ratios[g_idx][c].push(r.ln());
                                    }
                                }
                            } else if d0 >= -16.0 && d0 <= dark_cap && d1 >= -16.0 && d1 <= dark_cap {
                                if local_grad <= 64.0 {
                                    let diff = d1 - d0;
                                    if diff.abs() <= 64.0 {
                                        dark_diffs[g_idx][c].push(diff);
                                    }
                                }
                            }
                        }
                    }
                    x_blk += step_x;
                }
            }
            y += step_y;
        }
    }

    let mut gain = [1.0f32; 16];
    let mut offset = [0.0f32; 16];
    let mut any_valid = false;

    for g_idx in 0..4usize {
        let dy = g_idx >> 1;
        let dx = g_idx & 1;

        // Solve multiplicative gains with zero-sum loop closure
        let mut l = [0.0f32; 4];
        let mut gain_ok = true;
        for c in 0..4 {
            if log_ratios[g_idx][c].len() < 24 {
                gain_ok = false;
                break;
            }
            l[c] = median_f32(&mut log_ratios[g_idx][c]).unwrap_or(0.0);
        }
        if gain_ok {
            any_valid = true;
            let mean_l = 0.25 * (l[0] + l[1] + l[2] + l[3]);
            for v in &mut l {
                *v -= mean_l;
            }
            let s = [0.0, l[0], l[0] + l[1], l[0] + l[1] + l[2]];
            let mean_s = 0.25 * (s[0] + s[1] + s[2] + s[3]);
            for c in 0..4 {
                let phase = (dy << 3) | (c * 2 + dx);
                gain[phase] = (-(s[c] - mean_s)).exp().clamp(0.94, 1.06);
            }
        }

        // Solve additive offsets with zero-sum loop closure
        let mut d = [0.0f32; 4];
        let mut off_ok = true;
        for c in 0..4 {
            if dark_diffs[g_idx][c].len() < 24 {
                off_ok = false;
                break;
            }
            d[c] = median_f32(&mut dark_diffs[g_idx][c]).unwrap_or(0.0);
        }
        if off_ok {
            any_valid = true;
            let mean_d = 0.25 * (d[0] + d[1] + d[2] + d[3]);
            for v in &mut d {
                *v -= mean_d;
            }
            let o = [0.0, d[0], d[0] + d[1], d[0] + d[1] + d[2]];
            let mean_o = 0.25 * (o[0] + o[1] + o[2] + o[3]);
            let max_off = (range * 0.004).clamp(4.0, 24.0);
            for c in 0..4 {
                let phase = (dy << 3) | (c * 2 + dx);
                offset[phase] = (-(o[c] - mean_o)).clamp(-max_off, max_off);
            }
        }
    }

    if !any_valid {
        return None;
    }

    let max_gain_pct = gain
        .iter()
        .map(|&g| (g - 1.0).abs() * 100.0)
        .fold(0.0f32, f32::max);
    let max_offset = offset
        .iter()
        .map(|&o| o.abs())
        .fold(0.0f32, f32::max);

    // Auto gate: skip clean clips with negligible striping so LJ92 passthrough is preserved.
    let min_offset = (range * 0.0006).clamp(0.8, 4.0);
    if !force && max_gain_pct < 0.30 && max_offset < min_offset {
        return None;
    }

    Some(StripeFix {
        gain,
        offset,
        black,
        white,
        max_gain_pct,
        max_offset,
    })
}

/// Calibrates vertical stripe correction for `clip` according to [`stripe_mode()`].
///
/// Runs after `dual_iso` calibration so if Dual ISO is active, stripes are calibrated in the
/// reconstructed 16-bit linear space (`black = 0`, `white = 65535`).
pub fn calibrate_clip(clip: &MlvClip) -> Option<StripeFix> {
    calibrate_clip_with_mode(
        clip,
        stripe_mode(),
        clip.focus_fix.as_deref(),
        clip.bad_pixels.as_deref(),
        clip.dual_iso.as_deref(),
    )
}

/// Same as [`calibrate_clip`] with explicit mode and upstream fixes (used for live per-clip overrides).
pub fn calibrate_clip_with_mode(
    clip: &MlvClip,
    mode: StripeMode,
    focus_fix: Option<&crate::focus_fix::FocusFix>,
    bad_pixels: Option<&crate::bad_pixels::BadPixelFix>,
    dual_iso: Option<&crate::dual_iso::DualIsoCalibration>,
) -> Option<StripeFix> {
    if mode == StripeMode::Off || clip.is_codec_decoded() {
        return None;
    }
    let (w, h) = (clip.width as usize, clip.height as usize);
    let n = clip.frames.len();
    if n == 0 || w < 24 || h < 8 {
        return None;
    }

    let want = n.min(4);
    let picks: Vec<usize> = if want == 1 {
        vec![0]
    } else {
        (0..want).map(|i| i * (n - 1) / (want - 1)).collect()
    };

    let mut decoded: Vec<Vec<u16>> = Vec::with_capacity(picks.len());
    for idx in picks {
        if let Ok(mut px) = decode_frame_u16(clip, idx) {
            if let Some(ff) = focus_fix {
                ff.apply_u16_ex(&mut px, w, h, dual_iso.is_some());
            }
            if let Some(bp) = bad_pixels {
                bp.apply_u16_ex(&mut px, w, h, dual_iso.is_some());
            }
            if let Some(diso) = dual_iso {
                diso.apply_u16(&mut px, w, h);
            }
            decoded.push(px);
        }
    }
    if decoded.is_empty() {
        return None;
    }

    let (black, white) = if dual_iso.is_some() {
        (0.0f32, 65535.0f32)
    } else {
        (clip.levels.black as f32, clip.levels.white as f32)
    };
    let slices: Vec<&[u16]> = decoded.iter().map(|v| v.as_slice()).collect();
    let fix = calibrate_from_frames(&slices, w, h, black, white, mode == StripeMode::Force);
    if let Some(f) = &fix {
        crate::log::write(
            "INFO",
            &format!(
                "clip '{}': vertical stripes calibrated (mode={}): max_gain={:.2}%, max_offset={:.2}",
                clip.base_name,
                mode.label(),
                f.max_gain_pct,
                f.max_offset,
            ),
        );
    }
    fix
}

impl StripeFix {
    /// Applies 16-phase vertical stripe correction in place on host-order `u16` samples.
    pub fn apply_u16(&self, buf: &mut [u16], w: usize, h: usize) {
        if buf.len() < w * h {
            return;
        }
        // Estimate effective clipping white so saturated highlights are never scaled into bands.
        let floor_white = (self.white * (2.0 / 3.0)) as u16;
        let max_px = buf[..w * h].iter().copied().max().unwrap_or(0);
        let eff_white = max_px.max(floor_white).min(self.white as u16);
        let eff_white_f = eff_white as f32;
        let black = self.black;
        for y in 0..h {
            let row = &mut buf[y * w..(y + 1) * w];
            let y_off = (y & 1) << 3;
            for (x, px) in row.iter_mut().enumerate() {
                if *px >= eff_white {
                    continue;
                }
                let p = y_off | (x & 7);
                let v = black + (*px as f32 - black) * self.gain[p] + self.offset[p];
                *px = (v + 0.5).clamp(0.0, eff_white_f) as u16;
            }
        }
    }

    /// Applies 16-phase vertical stripe correction in place on little-endian `u16` bytes (DNG path).
    pub fn apply_le(&self, buf: &mut [u8], w: usize, h: usize) {
        if buf.len() < w * h * 2 {
            return;
        }
        let floor_white = (self.white * (2.0 / 3.0)) as u16;
        let mut max_px = 0u16;
        for chunk in buf[..w * h * 2].chunks_exact(2) {
            let v = u16::from_le_bytes([chunk[0], chunk[1]]);
            if v > max_px {
                max_px = v;
            }
        }
        let eff_white = max_px.max(floor_white).min(self.white as u16);
        let eff_white_f = eff_white as f32;
        let black = self.black;
        for y in 0..h {
            let row = &mut buf[y * w * 2..(y + 1) * w * 2];
            let y_off = (y & 1) << 3;
            for (x, chunk) in row.chunks_exact_mut(2).enumerate() {
                let raw_u16 = u16::from_le_bytes([chunk[0], chunk[1]]);
                if raw_u16 >= eff_white {
                    continue;
                }
                let p = y_off | (x & 7);
                let v = black + (raw_u16 as f32 - black) * self.gain[p] + self.offset[p];
                chunk.copy_from_slice(&((v + 0.5).clamp(0.0, eff_white_f) as u16).to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_striped_frame(w: usize, h: usize, black: f32, col_gain: &[f32; 8], col_off: &[f32; 8]) -> Vec<u16> {
        let mut px = vec![0u16; w * h];
        for y in 0..h {
            // Vary luminance vertically from shadows to midtones
            let base = 40.0 + (y as f32 / h as f32) * 5000.0;
            for x in 0..w {
                let c = x & 7;
                let v = black + base * col_gain[c] + col_off[c];
                px[y * w + x] = v.round().clamp(0.0, 65535.0) as u16;
            }
        }
        px
    }

    #[test]
    fn detects_and_removes_8_column_stripes() {
        let (w, h) = (128usize, 96usize);
        let (black, white) = (2048.0f32, 15000.0f32);
        let col_gain = [1.0, 1.0, 1.018, 0.985, 0.990, 1.012, 1.005, 0.992];
        let col_off = [0.0, 0.0, 4.0, -3.0, -2.0, 3.0, 1.0, -1.0];
        let mut frame = synth_striped_frame(w, h, black, &col_gain, &col_off);
        let fix = calibrate_from_frames(&[&frame], w, h, black, white, false)
            .expect("should detect 1.8% vertical striping");
        assert!(fix.max_gain_pct > 1.0);

        fix.apply_u16(&mut frame, w, h);
        // Columns in the same colour group should now agree closely at midtone row y = 60
        let y = 60usize;
        let r0 = frame[y * w + 16] as i32;
        for c in 1..4usize {
            let rc = frame[y * w + 16 + c * 2] as i32;
            assert!((r0 - rc).abs() <= 4, "residual stripe at col {}: {} vs {}", c * 2, r0, rc);
        }
    }

    #[test]
    fn clean_frame_skipped_in_auto_but_runs_in_force() {
        let (w, h) = (128usize, 96usize);
        let frame = synth_striped_frame(w, h, 2048.0, &[1.0; 8], &[0.0; 8]);
        assert!(calibrate_from_frames(&[&frame], w, h, 2048.0, 15000.0, false).is_none());
        assert!(calibrate_from_frames(&[&frame], w, h, 2048.0, 15000.0, true).is_some());
    }
}
