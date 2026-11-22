//! Magic Lantern Video (`.MLV` + `.M00`..`.M99`) parser and `O(1)` in-memory indexer.
//!
//! All file reads use positioned I/O (`pread` on Unix, `seek_read` on Windows)
//! against file descriptors opened once when the clip is loaded.

use crate::color_matrices::{is_valid_color_matrix, lookup_camera_profile, ILLUMINANT_D65};
use crate::lj92::{inspect_lj92, Lj92HeaderInfo};
use crate::{read_exact_at, Error, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// MLV `videoClass` compression flags.
pub const MLV_VIDEO_CLASS_FLAG_LJ92: u16 = 0x0020;

/// CFA Bayer pattern phases in DNG `CFAPattern` 4-byte order (`0=R, 1=G, 2=B`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfaPattern {
    Rggb,
    Gbrg,
    Grbg,
    Bggr,
}

impl CfaPattern {
    pub fn from_mlv_u32(v: u32) -> Self {
        match v {
            0x01000201 => CfaPattern::Gbrg,
            0x01020001 => CfaPattern::Grbg,
            0x00010102 => CfaPattern::Bggr,
            0x02010100 | _ => CfaPattern::Rggb,
        }
    }

    pub fn dng_bytes(self) -> [u8; 4] {
        match self {
            CfaPattern::Rggb => [0, 1, 1, 2],
            CfaPattern::Gbrg => [1, 2, 0, 1],
            CfaPattern::Grbg => [1, 0, 2, 1],
            CfaPattern::Bggr => [2, 1, 1, 0],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CfaPattern::Rggb => "RGGB",
            CfaPattern::Gbrg => "GBRG",
            CfaPattern::Grbg => "GRBG",
            CfaPattern::Bggr => "BGGR",
        }
    }
}

/// Real-time clock timestamp from an `RTCI` block.
#[derive(Debug, Clone, Copy)]
pub struct RtciTime {
    pub year: u16,
    pub month: u8, // 1..=12
    pub day: u8,   // 1..=31
    pub hour: u8,  // 0..=23
    pub minute: u8,
    pub second: u8,
}

impl Default for RtciTime {
    fn default() -> Self {
        Self {
            year: 2026,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }
}

impl RtciTime {
    pub fn seconds_since_midnight(&self) -> u32 {
        (self.hour as u32) * 3600 + (self.minute as u32) * 60 + (self.second as u32)
    }

    pub fn tiff_datetime_string(&self) -> String {
        format!(
            "{:04}:{:02}:{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    pub fn date_ymd_string(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub fn time_hms_string(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// Converts the `RTCI` calendar date/time to Unix epoch seconds (UTC).
    pub fn unix_epoch_secs(&self) -> u64 {
        let y = (self.year as i64) - if self.month <= 2 { 1 } else { 0 };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64;
        let m = self.month as u64;
        let d = self.day.max(1) as u64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + (doe as i64) - 719468;
        if days <= 0 {
            1_700_000_000
        } else {
            (days as u64) * 86400 + (self.seconds_since_midnight() as u64)
        }
    }

    /// Constructs an `RtciTime` from Unix epoch seconds (UTC).
    pub fn from_unix_secs(secs: u64) -> Self {
        let day_secs = (secs % 86400) as u32;
        let hour = (day_secs / 3600) as u8;
        let minute = ((day_secs % 3600) / 60) as u8;
        let second = (day_secs % 60) as u8;

        let z = (secs / 86400) as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = (z - era * 146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
        let year = (y + if month <= 2 { 1 } else { 0 }).clamp(1970, 9999) as u16;

        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }
}

/// Dynamic metadata state (`EXPO`, `WBAL`, `LENS`) active for a given frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameDynamicMeta {
    pub iso_value: u32,
    pub shutter_us: u64,
    pub wb_kelvin: u32,
    pub wb_gain_r: f32,
    pub wb_gain_g: f32,
    pub wb_gain_b: f32,
    pub focal_length_mm: u16,
    pub aperture_x100: u16,
    pub lens_name: String,
}

impl Default for FrameDynamicMeta {
    fn default() -> Self {
        Self {
            iso_value: 100,
            shutter_us: 16_667,
            wb_kelvin: 5600,
            wb_gain_r: 1.0,
            wb_gain_g: 1.0,
            wb_gain_b: 1.0,
            focal_length_mm: 0,
            aperture_x100: 0,
            lens_name: String::new(),
        }
    }
}

/// Indexed video frame entry (`VIDF`).
#[derive(Debug, Clone, Copy)]
pub struct MlvFrameEntry {
    pub chunk_idx: u16,
    pub frame_number: u32,
    pub timestamp_us: u64,
    pub payload_offset: u64,
    pub payload_size: u32,
    pub meta_idx: u16,
}

/// Indexed audio chunk entry (`AUDF`).
#[derive(Debug, Clone, Copy)]
pub struct MlvAudioEntry {
    pub chunk_idx: u16,
    pub frame_number: u32,
    pub timestamp_us: u64,
    pub payload_offset: u64,
    pub payload_size: u32,
    /// Cumulative byte offset of this chunk's PCM payload in the concatenated audio stream.
    pub cum_pcm_offset: u64,
}

/// Audio format info from `WAVI`.
#[derive(Debug, Clone, Copy)]
pub struct MlvAudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub total_pcm_bytes: u64,
}

/// Clip-level metadata and index for an `.MLV` or `.mcraw` recording.
pub struct MlvClip {
    pub base_name: String,
    pub chunk_paths: Vec<PathBuf>,
    pub(crate) chunks: Vec<File>,
    pub version_string: String,
    pub video_guid: u64,
    pub video_class: u16,
    pub audio_class: u16,
    pub fps_nom: u32,
    pub fps_denom: u32,
    pub width: u32,
    pub height: u32,
    /// Sensor readout buffer dimensions (`raw_info.width/height` from `RAWI`), used for matching
    /// Magic Lantern focus-pixel maps.
    pub raw_buffer_width: u32,
    pub raw_buffer_height: u32,
    /// Recorded window origin inside the sensor buffer (`panPosX/Y` from the first `VIDF`,
    /// aligned to 8px horizontally and 2px vertically for packed raw / Bayer boundaries).
    pub pan_x: u32,
    pub pan_y: u32,
    pub bits_per_pixel: u32,
    pub black_level: i32,
    pub white_level: i32,
    /// Resolved (sanity-checked / OB-refined) levels actually written to the DNG.
    pub levels: crate::levels::ResolvedLevels,
    /// Focus-pixel (PDAF) correction (bundled map or clip-learned lattice), if active.
    pub focus_fix: Option<std::sync::Arc<crate::focus_fix::FocusFix>>,
    /// `DISO` block metadata (`dualMode`, `isoValue`), if present in the MLV container.
    pub diso_mode: u32,
    pub diso_iso_value: u32,
    /// Dual ISO calibration (alternating 2-row gain recovery), if detected and enabled.
    pub dual_iso: Option<std::sync::Arc<crate::dual_iso::DualIsoCalibration>>,
    /// Static hot/dead pixel repair map, if any defects were detected and enabled.
    pub bad_pixels: Option<std::sync::Arc<crate::bad_pixels::BadPixelFix>>,
    /// 8-column Canon ADC vertical stripe correction, if detected (or forced).
    pub stripes: Option<std::sync::Arc<crate::stripes::StripeFix>>,
    pub active_area: [i32; 4],
    pub cfa_pattern: CfaPattern,
    pub camera_name: String,
    pub camera_model_id: u32,
    pub rotation_degrees: i32,
    pub calibration_illuminant1: u16,
    pub color_matrix1: [f64; 9],
    pub color_matrix1_source: &'static str,
    pub calibration_illuminant2: Option<u16>,
    pub color_matrix2: Option<[f64; 9]>,
    pub forward_matrix1: Option<[f64; 9]>,
    pub forward_matrix2: Option<[f64; 9]>,
    pub rtci: RtciTime,
    pub is_lj92: bool,
    pub is_mcraw: bool,
    /// Blackmagic RAW (experimental decoder, always fully decoded to 12-bit Bayer).
    pub is_braw: bool,
    pub mcraw_compression_type: u16,
    pub mcraw_stored_format: u32,
    pub lj92_info: Option<Lj92HeaderInfo>,
    pub dynamic_metas: Vec<FrameDynamicMeta>,
    pub frames: Vec<MlvFrameEntry>,
    pub audio_info: Option<MlvAudioInfo>,
    pub audio_entries: Vec<MlvAudioEntry>,
    pub block_counts: BTreeMap<String, usize>,
    /// `RAWC` sampling `[binning_x, skipping_x, binning_y, skipping_y]` if the file has a RAWC block.
    pub rawc_sampling: Option<[u8; 4]>,
    /// Vertical pixel stretch needed to restore square pixels (1.0 = none, 3.0 = crop_rec 3x1,
    /// 1.6667 = 5D3 720p style 1:1.67). Written to the DNG `DefaultScale` tag.
    pub stretch_y: f64,
    /// Where `stretch_y` came from: "none", "RAWC", "manual".
    pub stretch_source: &'static str,
}

static STRETCH_OVERRIDE_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Forces the vertical stretch factor for every clip opened afterwards (`None` = automatic from `RAWC`).
pub fn set_stretch_override(factor: Option<f32>) {
    STRETCH_OVERRIDE_BITS.store(
        factor.filter(|f| *f > 0.0).map(f32::to_bits).unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn stretch_override() -> Option<f64> {
    match STRETCH_OVERRIDE_BITS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        b => Some(f32::from_bits(b) as f64),
    }
}

/// Maps UI stretch override index (`0=Auto, 1=Off, 2=H×3, 3=V×1.67, 4=V×3`) to a `stretch_y` factor.
pub fn stretch_override_factor(mode: u8) -> Option<f64> {
    match mode {
        1 => Some(1.0),
        2 => Some(1.0 / 3.0),
        3 => Some(5.0 / 3.0),
        4 => Some(3.0),
        _ => None,
    }
}

/// Returns the current global stretch override as a UI mode index (`0..=4`).
pub fn stretch_override_mode() -> u8 {
    let Some(f) = stretch_override() else { return 0 };
    if (f - 1.0).abs() < 0.02 {
        1
    } else if (f - 1.0 / 3.0).abs() < 0.02 {
        2
    } else if (f - 5.0 / 3.0).abs() < 0.05 {
        3
    } else if (f - 3.0).abs() < 0.05 {
        4
    } else {
        0
    }
}

/// Same rule as MLV App's `getMlvAspectRatio`: `(binning_y+skipping_y) / (binning_x+skipping_x)`,
/// snapped to the standard 1.0 / 1.67 / 3.0 factors. Returns 1.0 when unknown.
pub fn stretch_from_sampling(s: [u8; 4]) -> f64 {
    let sx = s[0] as f64 + s[1] as f64;
    let sy = s[2] as f64 + s[3] as f64;
    if sx == 0.0 || sy == 0.0 {
        return 1.0;
    }
    let r = sy / sx;
    if r > 0.9 && r < 1.1 {
        1.0
    } else if r > 1.6 && r < 1.7 {
        5.0 / 3.0
    } else if r > 2.9 && r < 3.1 {
        3.0
    } else {
        r
    }
}

impl MlvClip {
    /// Opens an `.MLV` (plus `.M00`..`.M99` companion chunks) or `.mcraw` file
    /// and indexes all frames, audio packets, and metadata in a single pass.
    pub fn open(primary_path: impl AsRef<Path>) -> Result<Self> {
        let primary_path = primary_path.as_ref().to_path_buf();
        if let Some(ext) = primary_path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("mcraw") {
                return crate::mcraw::open_mcraw_clip(&primary_path);
            }
            if ext.eq_ignore_ascii_case("braw") {
                return crate::braw::open_braw_clip(&primary_path);
            }
        }
        let base_name = primary_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("CLIP")
            .to_string();

        let chunk_paths = discover_chunks(&primary_path);
        let mut chunks = Vec::with_capacity(chunk_paths.len());
        for p in &chunk_paths {
            chunks.push(File::open(p)?);
        }

        // Verify primary MLVI header (52 bytes) or MOTION header
        let mut mlvi = [0u8; 52];
        read_exact_at(&chunks[0], &mut mlvi, 0)?;
        if &mlvi[0..7] == b"MOTION " {
            drop(chunks);
            return crate::mcraw::open_mcraw_clip(&primary_path);
        }
        if &mlvi[0..4] != b"MLVI" {
            return Err(Error::InvalidFormat(format!(
                "File {:?} does not start with MLVI or MOTION magic",
                primary_path
            )));
        }

        let version_string = trim_ascii(&mlvi[8..16]);
        let video_guid = u64::from_le_bytes(mlvi[16..24].try_into().unwrap());
        let video_class = u16::from_le_bytes(mlvi[32..34].try_into().unwrap());
        let audio_class = u16::from_le_bytes(mlvi[34..36].try_into().unwrap());
        let fps_nom = u32::from_le_bytes(mlvi[44..48].try_into().unwrap());
        let fps_denom = u32::from_le_bytes(mlvi[48..52].try_into().unwrap());
        let is_lj92 = (video_class & MLV_VIDEO_CLASS_FLAG_LJ92) != 0;

        let mut clip = Self {
            base_name,
            chunk_paths,
            chunks,
            version_string,
            video_guid,
            video_class,
            audio_class,
            fps_nom,
            fps_denom,
            width: 0,
            height: 0,
            raw_buffer_width: 0,
            raw_buffer_height: 0,
            pan_x: 0,
            pan_y: 0,
            bits_per_pixel: 16,
            black_level: 0,
            white_level: 16383,
            levels: crate::levels::ResolvedLevels::from_file(0, 16383),
            focus_fix: None,
            diso_mode: 0,
            diso_iso_value: 0,
            dual_iso: None,
            bad_pixels: None,
            stripes: None,
            active_area: [0; 4],
            cfa_pattern: CfaPattern::Rggb,
            camera_name: String::from("CineFS"),
            camera_model_id: 0,
            rotation_degrees: 0,
            calibration_illuminant1: ILLUMINANT_D65,
            color_matrix1: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            color_matrix1_source: "default-identity",
            calibration_illuminant2: None,
            color_matrix2: None,
            forward_matrix1: None,
            forward_matrix2: None,
            rtci: RtciTime::default(),
            is_lj92,
            is_mcraw: false,
            is_braw: false,
            mcraw_compression_type: 0,
            mcraw_stored_format: 0,
            lj92_info: None,
            dynamic_metas: vec![FrameDynamicMeta::default()],
            frames: Vec::new(),
            audio_info: None,
            audio_entries: Vec::new(),
            block_counts: BTreeMap::new(),
            rawc_sampling: None,
            stretch_y: 1.0,
            stretch_source: "none",
        };

        clip.index_all_chunks()?;
        Ok(clip)
    }

    /// Nominal frame rate in frames per second.
    pub fn fps_f64(&self) -> f64 {
        if self.fps_denom > 0 && self.fps_nom > 0 {
            (self.fps_nom as f64) / (self.fps_denom as f64)
        } else {
            24.0
        }
    }

    /// Rounded integer frame rate for SMPTE timecode calculation.
    pub fn fps_rounded(&self) -> u32 {
        (self.fps_f64().round() as u32).max(1)
    }

    /// Returns `true` if this clip's LJ92 bitstream can be passed straight through into a DNG
    /// `Compression = 7` strip without pixel decoding.
    /// `true` for sources whose frames must always be fully decoded by a CineFS codec
    /// (MCRAW, BRAW) rather than repacked from MLV bit-packing or LJ92.
    pub fn is_codec_decoded(&self) -> bool {
        self.is_mcraw || self.is_braw
    }

    /// Decodes a codec-compressed frame payload (MCRAW / BRAW) to little-endian `u16` samples.
    pub fn decode_codec_frame_to_u16_le(&self, raw: &[u8], dst: &mut [u8]) -> Result<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.is_braw {
            crate::braw::decode_braw_frame_to_u16_le(raw, w, h, dst)
        } else {
            crate::mcraw::decode_mcraw_frame_to_u16_le(raw, w, h, self.mcraw_compression_type, dst)
        }
    }

    pub fn is_lj92_passthrough_compatible(&self) -> bool {
        match self.lj92_info {
            Some(info) => info.is_dng_cfa_passthrough_compatible(self.width, self.height),
            None => false,
        }
    }

    /// Reads the raw on-disk payload of `frame_index` into `dst`.
    pub fn read_raw_frame_payload(&self, frame_index: usize, dst: &mut [u8]) -> Result<()> {
        let entry = self
            .frames
            .get(frame_index)
            .ok_or_else(|| Error::NotFound(format!("Frame {frame_index} out of range")))?;
        let len = entry.payload_size as usize;
        if dst.len() < len {
            return Err(Error::InvalidFormat("Destination buffer too small".into()));
        }
        let file = &self.chunks[entry.chunk_idx as usize];
        read_exact_at(file, &mut dst[..len], entry.payload_offset)?;
        Ok(())
    }

    /// Reads a slice `[offset_in_payload .. offset_in_payload + dst.len()]` directly from
    /// the on-disk payload of `frame_index` via `pread` without reading the entire frame.
    pub fn read_raw_frame_slice(
        &self,
        frame_index: usize,
        offset_in_payload: u64,
        dst: &mut [u8],
    ) -> Result<usize> {
        let entry = self
            .frames
            .get(frame_index)
            .ok_or_else(|| Error::NotFound(format!("Frame {frame_index} out of range")))?;
        let payload_len = entry.payload_size as u64;
        if offset_in_payload >= payload_len || dst.is_empty() {
            return Ok(0);
        }
        let to_read = (payload_len - offset_in_payload).min(dst.len() as u64) as usize;
        let file = &self.chunks[entry.chunk_idx as usize];
        read_exact_at(
            file,
            &mut dst[..to_read],
            entry.payload_offset + offset_in_payload,
        )?;
        Ok(to_read)
    }

    /// Reads `[pcm_offset .. pcm_offset + dst.len()]` from the virtual concatenated PCM audio stream
    /// using binary search (`O(log N)`) over `AUDF` entries.
    pub fn read_audio_pcm_slice(&self, mut pcm_offset: u64, mut dst: &mut [u8]) -> Result<usize> {
        let total_pcm = match self.audio_info {
            Some(info) => info.total_pcm_bytes,
            None => return Ok(0),
        };
        if pcm_offset >= total_pcm || dst.is_empty() {
            return Ok(0);
        }

        // Binary search for the first AUDF block whose end (`cum_pcm_offset + payload_size`) > pcm_offset
        let mut idx = self
            .audio_entries
            .partition_point(|e| e.cum_pcm_offset + (e.payload_size as u64) <= pcm_offset);

        let mut total_read = 0usize;
        while !dst.is_empty() && idx < self.audio_entries.len() {
            let entry = &self.audio_entries[idx];
            let in_block_off = pcm_offset.saturating_sub(entry.cum_pcm_offset);
            let avail = (entry.payload_size as u64).saturating_sub(in_block_off) as usize;
            let take = avail.min(dst.len());
            if take == 0 {
                break;
            }
            let file = &self.chunks[entry.chunk_idx as usize];
            read_exact_at(file, &mut dst[..take], entry.payload_offset + in_block_off)?;
            dst = &mut dst[take..];
            pcm_offset += take as u64;
            total_read += take;
            idx += 1;
        }
        Ok(total_read)
    }

    fn index_all_chunks(&mut self) -> Result<()> {
        let mut current_meta = FrameDynamicMeta::default();
        let mut saw_any_frame_before_meta = false;
        let mut wavi_rate = 0u32;
        let mut wavi_channels = 0u16;
        let mut wavi_bits = 16u16;
        let mut has_rawi_matrix = false;

        for chunk_idx in 0..self.chunks.len() {
            let file_len = self.chunks[chunk_idx].metadata()?.len();
            let mut offset = 0u64;

            while offset + 16 <= file_len {
                let mut hdr = [0u8; 16];
                if read_exact_at(&self.chunks[chunk_idx], &mut hdr, offset).is_err() {
                    break;
                }
                let btype = String::from_utf8_lossy(&hdr[0..4]).to_string();
                let bsize = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as u64;
                let timestamp_us = u64::from_le_bytes(hdr[8..16].try_into().unwrap());

                if bsize < 16 || offset + bsize > file_len {
                    break;
                }

                *self.block_counts.entry(btype.clone()).or_insert(0) += 1;

                match &hdr[0..4] {
                    b"MLVI" => {
                        // Already parsed from chunk 0
                    }
                    b"VIDF" => {
                        if bsize >= 32 {
                            let mut vhdr = [0u8; 16];
                            read_exact_at(&self.chunks[chunk_idx], &mut vhdr, offset + 16)?;
                            let frame_number = u32::from_le_bytes(vhdr[0..4].try_into().unwrap());
                            let frame_space = u32::from_le_bytes(vhdr[12..16].try_into().unwrap()) as u64;
                            let header_and_space = 32 + frame_space;
                            if bsize > header_and_space {
                                if self.frames.is_empty() {
                                    // Align `panPosX` to 8px (14-bit pack boundary) and `panPosY` to 2px (Bayer).
                                    let pan_x = u16::from_le_bytes([vhdr[8], vhdr[9]]) as u32;
                                    let pan_y = u16::from_le_bytes([vhdr[10], vhdr[11]]) as u32;
                                    self.pan_x = (pan_x + 7) & !7;
                                    self.pan_y = pan_y & !1;
                                }
                                let payload_offset = offset + header_and_space;
                                let payload_size = (bsize - header_and_space) as u32;
                                let meta_idx = (self.dynamic_metas.len() - 1) as u16;
                                if meta_idx == 0 {
                                    saw_any_frame_before_meta = true;
                                }
                                self.frames.push(MlvFrameEntry {
                                    chunk_idx: chunk_idx as u16,
                                    frame_number,
                                    timestamp_us,
                                    payload_offset,
                                    payload_size,
                                    meta_idx,
                                });
                            }
                        }
                    }
                    b"AUDF" => {
                        if bsize >= 24 {
                            let mut ahdr = [0u8; 8];
                            read_exact_at(&self.chunks[chunk_idx], &mut ahdr, offset + 16)?;
                            let frame_number = u32::from_le_bytes(ahdr[0..4].try_into().unwrap());
                            let frame_space = u32::from_le_bytes(ahdr[4..8].try_into().unwrap()) as u64;
                            let header_and_space = 24 + frame_space;
                            if bsize > header_and_space {
                                self.audio_entries.push(MlvAudioEntry {
                                    chunk_idx: chunk_idx as u16,
                                    frame_number,
                                    timestamp_us,
                                    payload_offset: offset + header_and_space,
                                    payload_size: (bsize - header_and_space) as u32,
                                    cum_pcm_offset: 0,
                                });
                            }
                        }
                    }
                    b"RAWI" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 100 {
                            self.width = u16::from_le_bytes([buf[16], buf[17]]) as u32;
                            self.height = u16::from_le_bytes([buf[18], buf[19]]) as u32;
                            let raw_h = i32::from_le_bytes(buf[28..32].try_into().unwrap());
                            let raw_w = i32::from_le_bytes(buf[32..36].try_into().unwrap());
                            if raw_w > 0 && raw_h > 0 {
                                self.raw_buffer_width = raw_w as u32;
                                self.raw_buffer_height = raw_h as u32;
                            }
                            self.bits_per_pixel =
                                i32::from_le_bytes(buf[44..48].try_into().unwrap()).max(8) as u32;
                            self.black_level = i32::from_le_bytes(buf[48..52].try_into().unwrap());
                            self.white_level = i32::from_le_bytes(buf[52..56].try_into().unwrap());
                            for i in 0..4 {
                                let base = 72 + i * 4;
                                self.active_area[i] =
                                    i32::from_le_bytes(buf[base..base + 4].try_into().unwrap());
                            }
                            let cfa_u32 = u32::from_le_bytes(buf[96..100].try_into().unwrap());
                            self.cfa_pattern = CfaPattern::from_mlv_u32(cfa_u32);
                        }
                        if buf.len() >= 176 {
                            let illum = i32::from_le_bytes(buf[100..104].try_into().unwrap());
                            let mut m = [0.0f64; 9];
                            for i in 0..9 {
                                let base = 104 + i * 8;
                                let num = i32::from_le_bytes(buf[base..base + 4].try_into().unwrap());
                                let den =
                                    i32::from_le_bytes(buf[base + 4..base + 8].try_into().unwrap());
                                m[i] = if den != 0 {
                                    (num as f64) / (den as f64)
                                } else {
                                    0.0
                                };
                            }
                            if is_valid_color_matrix(&m) {
                                self.color_matrix1 = m;
                                self.calibration_illuminant1 =
                                    if illum > 0 { illum as u16 } else { ILLUMINANT_D65 };
                                self.color_matrix1_source = "MLV RAWI";
                                has_rawi_matrix = true;
                            }
                        }
                    }
                    b"RAWC" => {
                        // mlv_rawc_hdr_t: sensor_res_x/y @16/18, sensor_crop @20, binning_x @24,
                        // skipping_x @25, binning_y @26, skipping_y @27
                        let buf = self.read_block_vec(chunk_idx, offset, (bsize as usize).min(64))?;
                        if buf.len() >= 28 {
                            self.rawc_sampling = Some([buf[24], buf[25], buf[26], buf[27]]);
                        }
                    }
                    b"IDNT" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 52 {
                            let name = trim_ascii(&buf[16..48]);
                            if !name.is_empty() {
                                self.camera_name = name;
                            }
                            self.camera_model_id =
                                u32::from_le_bytes(buf[48..52].try_into().unwrap());
                        }
                    }
                    b"EXPO" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 40 {
                            current_meta.iso_value =
                                u32::from_le_bytes(buf[20..24].try_into().unwrap());
                            current_meta.shutter_us =
                                u64::from_le_bytes(buf[32..40].try_into().unwrap());
                            self.push_or_update_meta(&current_meta, saw_any_frame_before_meta);
                        }
                    }
                    b"DISO" => {
                        // mlv_diso_hdr_t: dualMode @16, isoValue @20
                        let buf = self.read_block_vec(chunk_idx, offset, (bsize as usize).min(32))?;
                        if buf.len() >= 24 {
                            self.diso_mode = u32::from_le_bytes(buf[16..20].try_into().unwrap());
                            self.diso_iso_value = u32::from_le_bytes(buf[20..24].try_into().unwrap());
                        }
                    }
                    b"WBAL" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 36 {
                            current_meta.wb_kelvin =
                                u32::from_le_bytes(buf[20..24].try_into().unwrap());
                            let r_recip = i32::from_le_bytes(buf[24..28].try_into().unwrap());
                            let g_recip = i32::from_le_bytes(buf[28..32].try_into().unwrap());
                            let b_recip = i32::from_le_bytes(buf[32..36].try_into().unwrap());
                            if g_recip > 0 {
                                current_meta.wb_gain_r = 1024.0 / (r_recip.max(1) as f32);
                                current_meta.wb_gain_g = 1024.0 / (g_recip.max(1) as f32);
                                current_meta.wb_gain_b = 1024.0 / (b_recip.max(1) as f32);
                            }
                            self.push_or_update_meta(&current_meta, saw_any_frame_before_meta);
                        }
                    }
                    b"LENS" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 64 {
                            current_meta.focal_length_mm =
                                u16::from_le_bytes([buf[16], buf[17]]);
                            current_meta.aperture_x100 =
                                u16::from_le_bytes([buf[20], buf[21]]);
                            current_meta.lens_name = trim_ascii(&buf[32..64]);
                            self.push_or_update_meta(&current_meta, saw_any_frame_before_meta);
                        }
                    }
                    b"RTCI" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 36 {
                            let sec = u16::from_le_bytes([buf[16], buf[17]]);
                            let min = u16::from_le_bytes([buf[18], buf[19]]);
                            let hour = u16::from_le_bytes([buf[20], buf[21]]);
                            let mday = u16::from_le_bytes([buf[22], buf[23]]);
                            let mon0 = u16::from_le_bytes([buf[24], buf[25]]);
                            let year_since_1900 = u16::from_le_bytes([buf[26], buf[27]]);
                            if year_since_1900 > 70 {
                                self.rtci = RtciTime {
                                    year: 1900 + year_since_1900,
                                    month: ((mon0 + 1).clamp(1, 12)) as u8,
                                    day: (mday.clamp(1, 31)) as u8,
                                    hour: (hour.min(23)) as u8,
                                    minute: (min.min(59)) as u8,
                                    second: (sec.min(59)) as u8,
                                };
                            }
                        }
                    }
                    b"WAVI" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 28 {
                            wavi_channels = u16::from_le_bytes([buf[18], buf[19]]);
                            wavi_rate = u32::from_le_bytes(buf[20..24].try_into().unwrap());
                            if buf.len() >= 32 {
                                let bps = u16::from_le_bytes([buf[30], buf[31]]);
                                if bps > 0 {
                                    wavi_bits = bps;
                                }
                            }
                        }
                    }
                    b"RKOR" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 20 {
                            let rot = i32::from_le_bytes(buf[16..20].try_into().unwrap());
                            self.rotation_degrees = rot.rem_euclid(360);
                        }
                    }
                    b"CKCM" => {
                        let buf = self.read_block_vec(chunk_idx, offset, bsize as usize)?;
                        if buf.len() >= 92 {
                            let illum2 = i32::from_le_bytes(buf[16..20].try_into().unwrap());
                            let mut m2 = [0.0f64; 9];
                            for i in 0..9 {
                                let base = 20 + i * 8;
                                let num = i32::from_le_bytes(buf[base..base + 4].try_into().unwrap());
                                let den =
                                    i32::from_le_bytes(buf[base + 4..base + 8].try_into().unwrap());
                                m2[i] = if den != 0 {
                                    (num as f64) / (den as f64)
                                } else {
                                    0.0
                                };
                            }
                            if illum2 > 0 && is_valid_color_matrix(&m2) {
                                self.calibration_illuminant2 = Some(illum2 as u16);
                                self.color_matrix2 = Some(m2);
                            }
                        }
                    }
                    _ => {}
                }

                offset += bsize;
            }
        }

        if self.frames.is_empty() {
            return Err(Error::InvalidFormat("MLV file contains no VIDF frames".into()));
        }

        // Sort frames and audio entries by frame_number (and timestamp as tie-breaker)
        self.frames
            .sort_by_key(|f| (f.frame_number, f.timestamp_us));
        self.audio_entries
            .sort_by_key(|a| (a.frame_number, a.timestamp_us));

        // Compute cumulative audio offsets
        let mut cum_pcm = 0u64;
        for entry in &mut self.audio_entries {
            entry.cum_pcm_offset = cum_pcm;
            cum_pcm += entry.payload_size as u64;
        }
        if wavi_rate > 0 && wavi_channels > 0 && cum_pcm > 0 {
            self.audio_info = Some(MlvAudioInfo {
                sample_rate: wavi_rate,
                channels: wavi_channels,
                bits_per_sample: wavi_bits,
                total_pcm_bytes: cum_pcm,
            });
        }

        // If RAWI had no valid color matrix, look up fallback by camera name
        if !has_rawi_matrix {
            if let Some(profile) = lookup_camera_profile(&self.camera_name) {
                self.calibration_illuminant1 = profile.illuminant1;
                self.color_matrix1 = profile.color_matrix1;
                self.color_matrix1_source = "fallback-table";
                if self.color_matrix2.is_none() {
                    self.calibration_illuminant2 = profile.illuminant2;
                    self.color_matrix2 = profile.color_matrix2;
                }
            }
        }

        // If LJ92 flag is set, inspect the first frame's LJ92 header (only ~256 bytes)
        if self.is_lj92 {
            let first = &self.frames[0];
            let inspect_len = (first.payload_size as usize).min(512);
            let mut probe = vec![0u8; inspect_len];
            let file = &self.chunks[first.chunk_idx as usize];
            if read_exact_at(file, &mut probe, first.payload_offset).is_ok() {
                if let Ok(info) = inspect_lj92(&probe) {
                    self.lj92_info = Some(info);
                }
            }
        }

        // Pixel stretch (crop_rec anamorphic modes: 1x3 binning etc.)
        if let Some(f) = stretch_override() {
            self.stretch_y = f;
            self.stretch_source = "manual";
        } else if let Some(sampling) = self.rawc_sampling {
            self.stretch_y = stretch_from_sampling(sampling);
            self.stretch_source = "RAWC";
        }
        self.levels = crate::levels::resolve(self);
        self.focus_fix = crate::focus_fix::resolve_clip(self).map(std::sync::Arc::new);
        self.dual_iso = crate::dual_iso::calibrate_clip(self).map(std::sync::Arc::new);
        self.bad_pixels = crate::bad_pixels::detect_clip(self).map(std::sync::Arc::new);
        self.stripes = crate::stripes::calibrate_clip(self).map(std::sync::Arc::new);
        crate::log::write(
            "INFO",
            &format!(
                "clip '{}': {}x{} RAWC sampling(bx,sx,by,sy)={:?} -> stretch_y={:.4} ({}), blocks={:?}",
                self.base_name,
                self.width,
                self.height,
                self.rawc_sampling,
                self.stretch_y,
                self.stretch_source,
                self.block_counts
            ),
        );

        Ok(())
    }

    fn read_block_vec(&self, chunk_idx: usize, offset: u64, size: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; size];
        read_exact_at(&self.chunks[chunk_idx], &mut buf, offset)?;
        Ok(buf)
    }

    fn push_or_update_meta(&mut self, current: &FrameDynamicMeta, saw_any_frame_before_meta: bool) {
        if !saw_any_frame_before_meta && self.dynamic_metas.len() == 1 {
            self.dynamic_metas[0] = current.clone();
        } else if self.dynamic_metas.last() != Some(current) {
            self.dynamic_metas[0] = current.clone(); // also update initial fallback
            self.dynamic_metas.push(current.clone());
        }
    }
}

fn trim_ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Discovers `.M00`..`.M99` (or `.m00`..`.m99`) chunk files next to `primary`.
fn discover_chunks(primary: &Path) -> Vec<PathBuf> {
    let mut result = vec![primary.to_path_buf()];
    let Some(parent) = primary.parent() else {
        return result;
    };
    let Some(stem) = primary.file_stem().and_then(|s| s.to_str()) else {
        return result;
    };
    let is_lower = primary
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.chars().all(|c| c.is_ascii_lowercase()))
        .unwrap_or(false);

    for i in 0..100 {
        let ext_upper = format!("M{i:02}");
        let ext_lower = format!("m{i:02}");
        let (first, second) = if is_lower {
            (ext_lower, ext_upper)
        } else {
            (ext_upper, ext_lower)
        };
        let p1 = parent.join(format!("{stem}.{first}"));
        if p1.is_file() {
            result.push(p1);
            continue;
        }
        let p2 = parent.join(format!("{stem}.{second}"));
        if p2.is_file() {
            result.push(p2);
            continue;
        }
        break;
    }
    result
}

#[cfg(test)]
mod stretch_tests {
    use super::*;

    #[test]
    fn stretch_snaps_like_mlv_app() {
        assert_eq!(stretch_from_sampling([1, 0, 3, 0]), 3.0); // crop_rec 3x1
        assert!((stretch_from_sampling([3, 0, 5, 0]) - 5.0 / 3.0).abs() < 1e-9); // 5D3 720p
        assert_eq!(stretch_from_sampling([3, 0, 1, 2]), 1.0); // 1080p
        assert_eq!(stretch_from_sampling([1, 0, 1, 0]), 1.0);
        assert_eq!(stretch_from_sampling([0, 0, 3, 0]), 1.0); // invalid
    }
}
