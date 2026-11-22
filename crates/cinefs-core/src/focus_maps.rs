//! Bundled Magic Lantern focus-pixel (PDAF) maps for Canon EOS M, EOS M2, 100D, 650D, and 700D.
//!
//! All 273 community-measured readout maps are stored losslessly in `../data/focus_maps.bin`
//! (format `CFPM` v1: deduplicated row arithmetic runs + sorted map index) so `cinefs-core`
//! needs no external decompression dependency and lookups run in microseconds.

use crate::mlv::MlvClip;

static MAPS_BIN: &[u8] = include_bytes!("../data/focus_maps.bin");

const HEADER_LEN: usize = 16;
const INDEX_ENTRY_LEN: usize = 16;

/// Concrete set of focus-pixel coordinates for one clip, already shifted into the clip's
/// recorded frame space (`(0..width, 0..height)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PixelMapFix {
    pub width: usize,
    pub height: usize,
    /// Defect coordinates `(x, y)` in frame space, sorted in row-major order.
    pub coords: Vec<(u32, u32)>,
    /// 1-bit-per-pixel lookup table (`width * height` bits) for O(1) `is_defect`.
    mask: Vec<u8>,
}

impl PixelMapFix {
    pub fn from_coords(width: usize, height: usize, coords: Vec<(u32, u32)>) -> Self {
        let mut mask = vec![0u8; (width * height + 7) / 8];
        for &(x, y) in &coords {
            let idx = y as usize * width + x as usize;
            mask[idx >> 3] |= 1 << (idx & 7);
        }
        Self { width, height, coords, mask }
    }

    #[inline]
    pub fn is_defect(&self, x: u32, y: u32) -> bool {
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height {
            return false;
        }
        let idx = y * self.width + x;
        (self.mask[idx >> 3] >> (idx & 7)) & 1 != 0
    }

    #[inline]
    pub fn pixel_count(&self) -> usize {
        self.coords.len()
    }

    #[inline]
    pub fn for_each_defect(&self, mut f: impl FnMut(u32, u32)) {
        for &(x, y) in &self.coords {
            f(x, y);
        }
    }
}

#[inline]
fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct Archive<'a> {
    data: &'a [u8],
    pattern_offsets: Vec<usize>,
    index_offset: usize,
    num_maps: usize,
    data_offset: usize,
}

impl<'a> Archive<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() < HEADER_LEN || &data[0..4] != b"CFPM" {
            return None;
        }
        let version = read_u32(data, 4);
        if version != 1 {
            return None;
        }
        let num_patterns = read_u32(data, 8) as usize;
        let num_maps = read_u32(data, 12) as usize;

        let mut pattern_offsets = Vec::with_capacity(num_patterns);
        let mut pos = HEADER_LEN;
        for _ in 0..num_patterns {
            if pos + 2 > data.len() {
                return None;
            }
            pattern_offsets.push(pos);
            let num_runs = read_u16(data, pos) as usize;
            pos += 2 + num_runs * 6;
            if pos > data.len() {
                return None;
            }
        }
        let index_offset = pos;
        let data_offset = index_offset + num_maps * INDEX_ENTRY_LEN;
        if data_offset > data.len() {
            return None;
        }
        Some(Self { data, pattern_offsets, index_offset, num_maps, data_offset })
    }

    fn find_map(&self, camera_model_id: u32, raw_w: u16, raw_h: u16) -> Option<(usize, usize)> {
        let key = (camera_model_id, raw_w, raw_h);
        let mut lo = 0usize;
        let mut hi = self.num_maps;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = self.index_offset + mid * INDEX_ENTRY_LEN;
            let k = (read_u32(self.data, e), read_u16(self.data, e + 4), read_u16(self.data, e + 6));
            match k.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let off = read_u32(self.data, e + 8) as usize;
                    let num_rows = read_u32(self.data, e + 12) as usize;
                    return Some((self.data_offset + off, num_rows));
                }
            }
        }
        None
    }

    fn build_fix(
        &self,
        camera_model_id: u32,
        raw_w: u32,
        raw_h: u32,
        frame_w: usize,
        frame_h: usize,
        pan_x: u32,
        pan_y: u32,
    ) -> Option<PixelMapFix> {
        let rw = u16::try_from(raw_w).ok()?;
        let rh = u16::try_from(raw_h).ok()?;
        let (rows_off, num_rows) = self.find_map(camera_model_id, rw, rh)?;
        if rows_off + num_rows * 4 > self.data.len() || frame_w == 0 || frame_h == 0 {
            return None;
        }

        let x_end = pan_x.saturating_add(frame_w as u32);
        let y_end = pan_y.saturating_add(frame_h as u32);
        let mut coords = Vec::new();

        for r in 0..num_rows {
            let ro = rows_off + r * 4;
            let sy = read_u16(self.data, ro) as u32;
            if sy < pan_y || sy >= y_end {
                continue;
            }
            let pid = read_u16(self.data, ro + 2) as usize;
            let po = *self.pattern_offsets.get(pid)?;
            let num_runs = read_u16(self.data, po) as usize;
            let fy = sy - pan_y;
            for i in 0..num_runs {
                let run_off = po + 2 + i * 6;
                let x0 = read_u16(self.data, run_off) as u32;
                let step = read_u16(self.data, run_off + 2) as u32;
                let cnt = read_u16(self.data, run_off + 4) as u32;
                let mut sx = x0;
                for _ in 0..cnt {
                    if sx >= pan_x && sx < x_end {
                        coords.push((sx - pan_x, fy));
                    }
                    sx = sx.wrapping_add(step);
                }
            }
        }

        if coords.is_empty() {
            return None;
        }
        Some(PixelMapFix::from_coords(frame_w, frame_h, coords))
    }
}

/// Looks up a bundled Magic Lantern focus-pixel map matching `(camera_model_id, raw_w, raw_h)`
/// and crops/shifts it into the recorded frame window `(frame_w, frame_h)` at `(pan_x, pan_y)`.
pub fn lookup(
    camera_model_id: u32,
    raw_w: u32,
    raw_h: u32,
    frame_w: usize,
    frame_h: usize,
    pan_x: u32,
    pan_y: u32,
) -> Option<PixelMapFix> {
    if camera_model_id == 0 || raw_w == 0 || raw_h == 0 {
        return None;
    }
    Archive::parse(MAPS_BIN)?.build_fix(camera_model_id, raw_w, raw_h, frame_w, frame_h, pan_x, pan_y)
}

/// Looks up a bundled focus-pixel map for `clip`.
///
/// The map key uses the sensor readout buffer size (`raw_buffer_width x raw_buffer_height` from
/// `RAWI`), falling back to the recorded frame size if the buffer size was absent.
pub fn lookup_for_clip(clip: &MlvClip) -> Option<PixelMapFix> {
    if clip.is_codec_decoded() || clip.camera_model_id == 0 {
        return None;
    }
    let raw_w = if clip.raw_buffer_width > 0 { clip.raw_buffer_width } else { clip.width };
    let raw_h = if clip.raw_buffer_height > 0 { clip.raw_buffer_height } else { clip.height };
    lookup(
        clip.camera_model_id,
        raw_w,
        raw_h,
        clip.width as usize,
        clip.height as usize,
        clip.pan_x,
        clip.pan_y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_archive_parses_all_273_maps() {
        let arch = Archive::parse(MAPS_BIN).expect("valid CFPM archive");
        assert_eq!(arch.num_maps, 273);
        assert!(arch.pattern_offsets.len() > 100);
    }

    #[test]
    fn lookups_cover_all_five_canon_bodies() {
        // 650D, 700D, EOS M, 100D, EOS M2
        let cases = [
            (0x8000_0301, 1808, 726),
            (0x8000_0326, 1808, 726),
            (0x8000_0331, 1808, 726),
            (0x8000_0346, 1808, 726),
            (0x8000_0355, 1808, 726),
        ];
        for (cam, w, h) in cases {
            let fix = lookup(cam, w, h, w as usize, h as usize, 0, 0)
                .unwrap_or_else(|| panic!("expected bundled map for {cam:x}_{w}x{h}"));
            assert!(fix.pixel_count() > 1000, "{cam:x}_{w}x{h} had too few pixels");
            let &(fx, fy) = fix.coords.first().unwrap();
            assert!(fix.is_defect(fx, fy));
        }
    }

    #[test]
    fn pan_offset_shifts_and_clips_coordinates() {
        let full = lookup(0x8000_0331, 1808, 726, 1808, 726, 0, 0).expect("full map");
        let (px, py) = (200u32, 100u32);
        let (fw, fh) = (1000usize, 400usize);
        let cropped = lookup(0x8000_0331, 1808, 726, fw, fh, px, py).expect("cropped map");
        assert!(cropped.pixel_count() > 0 && cropped.pixel_count() < full.pixel_count());
        for &(x, y) in &cropped.coords {
            assert!((x as usize) < fw && (y as usize) < fh);
            assert!(full.is_defect(x + px, y + py));
        }
    }

    #[test]
    fn unknown_camera_returns_none() {
        assert!(lookup(0x8000_0285, 1920, 1080, 1920, 1080, 0, 0).is_none());
    }

    #[test]
    fn inspect_real_focus_clip_when_set() {
        let Ok(path) = std::env::var("CINEFS_TEST_FOCUS_MLV") else { return };
        let clip = MlvClip::open(std::path::Path::new(&path)).expect("open clip");
        let fix = lookup_for_clip(&clip).expect("bundled map");
        let (w, h) = (clip.width as usize, clip.height as usize);
        let cfg = crate::focus_probe::ProbeConfig::for_bits(clip.bits_per_pixel, clip.levels.black as f32);
        let mut acc = crate::focus_probe::ProbeAccum::new(w, h);
        let n = clip.frames.len();
        for i in 0..n.min(8) {
            let idx = if n <= 1 { 0 } else { i * (n - 1) / (n.min(8) - 1) };
            if let Ok(px) = crate::focus_probe::decode_frame_u16(&clip, idx) {
                acc.add_frame(&px, cfg);
            }
        }
        let probed = acc.candidates(0.7);
        let mut hits = 0usize;
        for c in &probed {
            if fix.is_defect(c.x, c.y) { hits += 1; }
        }
        eprintln!(
            "clip={} raw={}x{} frame={}x{} origin=({},{}) map_px={} probed={} hits={} ({:.1}%)",
            clip.base_name, clip.raw_buffer_width, clip.raw_buffer_height,
            clip.width, clip.height, clip.pan_x, clip.pan_y,
            fix.pixel_count(), probed.len(), hits,
            if probed.is_empty() { 0.0 } else { hits as f64 * 100.0 / probed.len() as f64 }
        );
        // Test candidate shifts (dx, dy) around (0, 0) AND wider Y shifts
        let mut best: Vec<((i32, i32), usize)> = Vec::new();
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let h = probed.iter().filter(|c| {
                    let (nx, ny) = (c.x as i32 - dx, c.y as i32 - dy);
                    nx >= 0 && ny >= 0 && fix.is_defect(nx as u32, ny as u32)
                }).count();
                if h > 0 { best.push(((dx, dy), h)); }
            }
        }
        best.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("  top local (probed - map) shifts: {:?}", &best[..best.len().min(6)]);
        if hits * 2 < probed.len() {
            // Check larger shifts too
            let mut votes = std::collections::HashMap::<(i32, i32), usize>::new();
            for c in probed.iter().take(300) {
                for &(mx, my) in &fix.coords {
                    let dx = c.x as i32 - mx as i32;
                    let dy = c.y as i32 - my as i32;
                    if dx.abs() <= 160 && dy.abs() <= 300 {
                        *votes.entry((dx, dy)).or_insert(0) += 1;
                    }
                }
            }
            let mut top: Vec<_> = votes.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            eprintln!("  top wide (probed - map) shifts: {:?}", &top[..top.len().min(6)]);
        }
    }
}
