//! Black / white level sanity checks for `.MLV` clips.
//!
//! Metadata only: never touches pixels, so LJ92 passthrough is preserved.
//!
//! Note: measuring the real black level from optical-black pixels was tried and abandoned. MLV
//! stores only the cropped image area; `RAWI.active_area` is in *sensor* coordinates (it can exceed
//! the recorded frame size), so the recorded frames contain no optical-black band to measure.

use crate::mlv::MlvClip;

#[derive(Clone, Debug)]
pub struct ResolvedLevels {
    pub black: u32,
    pub white: u32,
    /// "file" | "fallback" (file values were invalid and had to be corrected)
    pub source: &'static str,
    pub note: String,
}

impl ResolvedLevels {
    pub fn from_file(black: i32, white: i32) -> Self {
        Self {
            black: black.max(0) as u32,
            white: white.max(1) as u32,
            source: "file",
            note: String::new(),
        }
    }
}

/// Resolves the levels for `clip`: the file values, corrected only if they are impossible.
pub fn resolve(clip: &MlvClip) -> ResolvedLevels {
    let bpp = clip.bits_per_pixel.clamp(8, 16);
    let max_val = ((1u64 << bpp) - 1) as i64;
    let mut lv = ResolvedLevels::from_file(clip.black_level, clip.white_level);

    if lv.white as i64 > max_val {
        lv.note = format!("white {} exceeds {}-bit range ({}); DNG writer clamps it", lv.white, bpp, max_val);
    }
    if (lv.white as i64).min(max_val) <= lv.black as i64 {
        let old = (lv.black, lv.white);
        lv.white = max_val as u32;
        if lv.black as i64 >= max_val {
            lv.black = 0;
        }
        lv.source = "fallback";
        lv.note = format!("invalid levels in file (black {} / white {}), using black {} white {}", old.0, old.1, lv.black, lv.white);
    }

    crate::log::write(
        "INFO",
        &format!(
            "clip '{}': levels black={} white={} source={}{}",
            clip.base_name,
            lv.black,
            lv.white,
            lv.source,
            if lv.note.is_empty() { String::new() } else { format!(" [{}]", lv.note) }
        ),
    );
    lv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_file_keeps_values() {
        let l = ResolvedLevels::from_file(2048, 15000);
        assert_eq!((l.black, l.white, l.source), (2048, 15000, "file"));
    }

    #[test]
    fn negative_black_is_clamped() {
        assert_eq!(ResolvedLevels::from_file(-5, 100).black, 0);
    }
}
