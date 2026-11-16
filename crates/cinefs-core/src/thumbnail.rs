//! Small preview images for the clip list.
//!
//! One frame (the middle one) is decoded, every 2x2 Bayer quad is collapsed into one RGB sample
//! (no real demosaic needed at thumbnail size), levels and as-shot white balance are applied, the
//! anamorphic stretch is reflected in the output aspect ratio, and a simple auto-exposure + sRGB
//! curve makes it viewable. Colour: camera RGB -> XYZ (DNG forward matrix, else inverse of the
//! colour matrix) -> Bradford to D65 -> linear Rec.709 primaries, encoded with the Rec.709 OETF.
//! Still an identification aid, not a graded render.

use crate::Result;
use crate::focus_probe::decode_frame_u16;
use crate::mlv::{CfaPattern, MlvClip};

#[derive(Clone, Debug)]
pub struct Thumbnail {
    pub width: usize,
    pub height: usize,
    /// `width * height * 4` bytes, straight (non-premultiplied) RGBA, alpha always 255.
    pub rgba: Vec<u8>,
}

/// Rec.709 OETF (BT.709 transfer characteristic).
fn rec709_encode(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v < 0.018 { 4.5 * v } else { 1.099 * v.powf(0.45) - 0.099 };
    (s * 255.0 + 0.5) as u8
}

type M3 = [f64; 9];

fn mul(a: &M3, b: &M3) -> M3 {
    let mut o = [0.0; 9];
    for r in 0..3 {
        for c in 0..3 {
            o[r * 3 + c] = (0..3).map(|k| a[r * 3 + k] * b[k * 3 + c]).sum();
        }
    }
    o
}

fn mul_vec(a: &M3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|r| (0..3).map(|k| a[r * 3 + k] * v[k]).sum())
}

fn invert(m: &M3) -> Option<M3> {
    let [a, b, c, d, e, f, g, h, i] = *m;
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if det.abs() < 1e-9 {
        return None;
    }
    let s = 1.0 / det;
    Some([
        (e * i - f * h) * s,
        (c * h - b * i) * s,
        (b * f - c * e) * s,
        (f * g - d * i) * s,
        (a * i - c * g) * s,
        (c * d - a * f) * s,
        (d * h - e * g) * s,
        (b * g - a * h) * s,
        (a * e - b * d) * s,
    ])
}

const BRADFORD: M3 = [0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296];
const D50: [f64; 3] = [0.9642, 1.0, 0.8249];
const D65: [f64; 3] = [0.9505, 1.0, 1.0890];
const XYZ_TO_709: M3 = [3.2406, -1.5372, -0.4986, -0.9689, 1.8758, 0.0415, 0.0557, -0.2040, 1.0570];

fn bradford(src: [f64; 3], dst: [f64; 3]) -> M3 {
    let inv = invert(&BRADFORD).unwrap();
    let s = mul_vec(&BRADFORD, src);
    let d = mul_vec(&BRADFORD, dst);
    let scale = [d[0] / s[0], 0.0, 0.0, 0.0, d[1] / s[1], 0.0, 0.0, 0.0, d[2] / s[2]];
    mul(&inv, &mul(&scale, &BRADFORD))
}

/// Matrix taking white-balanced camera RGB (neutral = 1,1,1) to linear Rec.709.
/// `cm` = DNG ColorMatrix (XYZ -> camera), `fm` = DNG ForwardMatrix (WB camera -> XYZ D50), `gains`
/// = WB gains (R,G,B). `None` when no usable matrix exists (the caller then skips colour mapping).
pub fn camera_to_rec709(cm: &[f64; 9], fm: Option<&[f64; 9]>, gains: [f32; 3]) -> Option<[f32; 9]> {
    let finite = |m: &M3| m.iter().all(|v| v.is_finite());
    let to_f32 = |m: M3| m.map(|v| v as f32);
    if let Some(fm) = fm.filter(|m| finite(m) && m.iter().any(|v| *v != 0.0)) {
        return Some(to_f32(mul(&XYZ_TO_709, &mul(&bradford(D50, D65), fm))));
    }
    let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    if !finite(cm) || cm.iter().all(|v| *v == 0.0) || *cm == identity {
        return None;
    }
    let inv = invert(cm)?;
    let n = [gains[0], gains[1], gains[2]].map(|g| 1.0 / (g.max(0.001) / gains[1].max(0.001)) as f64);
    let diag = [n[0], 0.0, 0.0, 0.0, n[1], 0.0, 0.0, 0.0, n[2]];
    let cam_to_xyz = mul(&inv, &diag);
    let white = mul_vec(&cam_to_xyz, [1.0, 1.0, 1.0]);
    if white.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let w = [white[0] / white[1], 1.0, white[2] / white[1]];
    Some(to_f32(mul(&XYZ_TO_709, &mul(&bradford(w, D65), &cam_to_xyz))))
}

/// Renders a thumbnail at most `max_width` pixels wide from an already decoded Bayer frame.
#[allow(clippy::too_many_arguments)]
pub fn render_from_bayer(
    src: &[u16],
    w: usize,
    h: usize,
    cfa: CfaPattern,
    black: f32,
    white: f32,
    wb_gains: [f32; 3],
    stretch_y: f64,
    to_rec709: Option<[f32; 9]>,
    max_width: usize,
) -> Thumbnail {
    let hw = (w / 2).max(1);
    let hh = (h / 2).max(1);
    let layout = cfa.dng_bytes(); // colour index of (0,0), (1,0), (0,1), (1,1)
    let range = (white - black).max(1.0);

    // Half-resolution linear RGB, white-balanced (green = 1).
    let g_ref = wb_gains[1].max(0.001);
    let gains = [wb_gains[0] / g_ref, 1.0, wb_gains[2] / g_ref];
    let mut half = vec![0f32; hw * hh * 3];
    for by in 0..hh {
        for bx in 0..hw {
            let mut sum = [0f32; 3];
            let mut cnt = [0f32; 3];
            for (i, (dx, dy)) in [(0usize, 0usize), (1, 0), (0, 1), (1, 1)].into_iter().enumerate() {
                let x = bx * 2 + dx;
                let y = by * 2 + dy;
                if x >= w || y >= h {
                    continue;
                }
                let c = layout[i] as usize;
                sum[c] += ((src[y * w + x] as f32 - black) / range).max(0.0);
                cnt[c] += 1.0;
            }
            let o = (by * hw + bx) * 3;
            for c in 0..3 {
                half[o + c] = if cnt[c] > 0.0 { sum[c] / cnt[c] * gains[c] } else { 0.0 };
            }
        }
    }

    // Output size: width limited, height follows the (stretched) aspect ratio.
    let out_w = max_width.clamp(8, 1024).min(hw.max(8));
    let aspect = (h as f64 * if stretch_y.is_finite() && stretch_y > 0.0 { stretch_y } else { 1.0 }) / w as f64;
    let out_h = ((out_w as f64 * aspect).round() as usize).clamp(1, 1024);

    // Area average from the half-res image.
    let mut lin = vec![0f32; out_w * out_h * 3];
    for oy in 0..out_h {
        let y0 = oy * hh / out_h;
        let y1 = (((oy + 1) * hh).div_ceil(out_h)).clamp(y0 + 1, hh);
        for ox in 0..out_w {
            let x0 = ox * hw / out_w;
            let x1 = (((ox + 1) * hw).div_ceil(out_w)).clamp(x0 + 1, hw);
            let mut s = [0f32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * hw + x) * 3;
                    s[0] += half[i];
                    s[1] += half[i + 1];
                    s[2] += half[i + 2];
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            let o = (oy * out_w + ox) * 3;
            lin[o] = s[0] / n;
            lin[o + 1] = s[1] / n;
            lin[o + 2] = s[2] / n;
        }
    }

    if let Some(m) = to_rec709 {
        for p in lin.chunks_exact_mut(3) {
            let (r, g, b) = (p[0], p[1], p[2]);
            p[0] = (m[0] * r + m[1] * g + m[2] * b).max(0.0);
            p[1] = (m[3] * r + m[4] * g + m[5] * b).max(0.0);
            p[2] = (m[6] * r + m[7] * g + m[8] * b).max(0.0);
        }
    }

    // Auto exposure: bring the 97th-percentile luma to ~0.9 (gain limited to avoid blowing up noise).
    let mut lumas: Vec<f32> = lin.chunks_exact(3).map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).collect();
    lumas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p97 = lumas[((lumas.len() - 1) as f32 * 0.97) as usize].max(1e-4);
    let gain = (0.9 / p97).clamp(0.5, 24.0);

    let mut rgba = vec![255u8; out_w * out_h * 4];
    for (i, p) in lin.chunks_exact(3).enumerate() {
        rgba[i * 4] = rec709_encode(p[0] * gain);
        rgba[i * 4 + 1] = rec709_encode(p[1] * gain);
        rgba[i * 4 + 2] = rec709_encode(p[2] * gain);
    }
    Thumbnail { width: out_w, height: out_h, rgba }
}

/// Decodes the middle frame of `clip` and renders a thumbnail of at most `max_width` pixels.
pub fn render(clip: &MlvClip, max_width: usize) -> Result<Thumbnail> {
    render_frame(clip, clip.frames.len() / 2, max_width)
}

/// Same as [`render`] for an arbitrary frame (clamped to the clip's range); used for hover scrubbing.
pub fn render_frame(clip: &MlvClip, frame_index: usize, max_width: usize) -> Result<Thumbnail> {
    let idx = frame_index.min(clip.frames.len().saturating_sub(1));
    let src = decode_frame_u16(clip, idx)?;
    let meta = clip
        .frames
        .get(idx)
        .and_then(|f| clip.dynamic_metas.get(f.meta_idx as usize))
        .or_else(|| clip.dynamic_metas.first());
    let gains = meta.map(|m| [m.wb_gain_r, m.wb_gain_g, m.wb_gain_b]).unwrap_or([1.0; 3]);
    Ok(render_from_bayer(
        &src,
        clip.width as usize,
        clip.height as usize,
        clip.cfa_pattern,
        clip.levels.black as f32,
        clip.levels.white as f32,
        gains,
        clip.stretch_y,
        camera_to_rec709(&clip.color_matrix1, clip.forward_matrix1.as_ref(), gains),
        max_width,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_scene_comes_out_neutral_with_stretched_aspect() {
        let (w, h) = (64usize, 36usize);
        let src = vec![2000u16; w * h];
        let t = render_from_bayer(&src, w, h, CfaPattern::Rggb, 0.0, 4000.0, [1.0, 1.0, 1.0], 3.0, None, 32);
        assert_eq!(t.width, 32);
        assert_eq!(t.height, (32.0f64 * 36.0 * 3.0 / 64.0).round() as usize);
        assert_eq!(t.rgba.len(), t.width * t.height * 4);
        let p = &t.rgba[0..4];
        assert!((p[0] as i32 - p[1] as i32).abs() <= 1 && (p[2] as i32 - p[1] as i32).abs() <= 1);
        assert_eq!(p[3], 255);
    }

    #[test]
    fn neutral_stays_neutral_through_colour_matrix() {
        // A typical Canon-like XYZ->camera matrix; camera neutral (0.5, 1, 0.6) must map to grey.
        let cm = [0.6722, -0.0635, -0.0963, -0.4287, 1.2460, 0.2028, -0.0908, 0.2162, 0.5668];
        let gains = [2.0f32, 1.0, 1.0 / 0.6];
        let m = camera_to_rec709(&cm, None, gains).unwrap();
        let o = [m[0] + m[1] + m[2], m[3] + m[4] + m[5], m[6] + m[7] + m[8]];
        assert!((o[0] - o[1]).abs() < 0.02 && (o[2] - o[1]).abs() < 0.02, "{o:?}");
    }

    #[test]
    fn identity_matrix_means_no_colour_mapping() {
        assert!(camera_to_rec709(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], None, [1.0; 3]).is_none());
    }

    #[test]
    fn white_balance_gain_shifts_colour() {
        let (w, h) = (16usize, 16usize);
        let src = vec![1000u16; w * h];
        let t = render_from_bayer(&src, w, h, CfaPattern::Rggb, 0.0, 4000.0, [2.0, 1.0, 1.0], 1.0, None, 8);
        assert!(t.rgba[0] > t.rgba[1]);
    }
}
