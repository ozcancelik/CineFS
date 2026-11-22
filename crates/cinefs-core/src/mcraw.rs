//! MotionCam Pro Raw (`.mcraw`) container parser, `O(1)` indexer, and block-level bitplane decoder.
//!
//! Supports MotionCam container v3 (`MOTION \x03`) with:
//! - Both `compressionType = 7` (64-pixel 2x2 Bayer quad block decoder) and
//!   `compressionType = 6` (legacy 32-pixel 1x2 pair block decoder), plus uncompressed `raw16`.
//! - Zero-dependency JSON metadata extraction for camera profile, dual-illuminant `ColorMatrix1/2`,
//!   `ForwardMatrix1/2`, CFA arrangement, dynamic levels, exposure, WB (`AsShotNeutral`), and lens.
//! - Sub-millisecond index parsing from the trailer (`BUFFER_INDEX` + `AUDIO_INDEX`).

use crate::color_matrices::{
    is_valid_color_matrix, lookup_camera_profile, ILLUMINANT_D65, ILLUMINANT_STD_A,
};
use crate::mlv::{
    CfaPattern, FrameDynamicMeta, MlvAudioEntry, MlvAudioInfo, MlvClip, MlvFrameEntry, RtciTime,
};
use crate::{read_exact_at, Error, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

const MCRAW_MAGIC: &[u8; 7] = b"MOTION ";
const MCRAW_VERSION: u8 = 3;
const MCRAW_INDEX_MAGIC: i32 = 0x8A905612u32 as i32;

const ITEM_BUFFER_INDEX: u32 = 0;
const ITEM_BUFFER_INDEX_DATA: u32 = 1;
const ITEM_BUFFER: u32 = 2;
const ITEM_METADATA: u32 = 3;
const ITEM_AUDIO_INDEX: u32 = 4;
const ITEM_AUDIO_DATA: u32 = 5;
const ITEM_AUDIO_DATA_METADATA: u32 = 6;

pub const MCRAW_COMP_LEGACY: u16 = 6;
pub const MCRAW_COMP_DEFAULT: u16 = 7;

/// Opens a MotionCam `.mcraw` file and indexes its video frames, audio packets, and metadata into an `MlvClip`.
pub fn open_mcraw_clip(primary_path: impl AsRef<Path>) -> Result<MlvClip> {
    let primary_path = primary_path.as_ref().to_path_buf();
    let base_name = primary_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("MCRAW")
        .to_string();

    let file = File::open(&primary_path)?;
    let file_len = file.metadata()?.len();
    if file_len < 32 {
        return Err(Error::InvalidFormat(
            "File is too small to be a valid .mcraw container".into(),
        ));
    }

    // 1. Verify 8-byte header ("MOTION \x03")
    let mut hdr = [0u8; 8];
    read_exact_at(&file, &mut hdr, 0)?;
    if &hdr[0..7] != MCRAW_MAGIC {
        return Err(Error::InvalidFormat(format!(
            "File {:?} does not start with MOTION magic",
            primary_path
        )));
    }
    if hdr[7] != MCRAW_VERSION {
        return Err(Error::InvalidFormat(format!(
            "Unsupported .mcraw container version: {}",
            hdr[7]
        )));
    }

    // 2. Read camera metadata item right at offset 8
    let mut meta_item = [0u8; 8];
    read_exact_at(&file, &mut meta_item, 8)?;
    let meta_type = u32::from_le_bytes(meta_item[0..4].try_into().unwrap());
    let meta_size = u32::from_le_bytes(meta_item[4..8].try_into().unwrap()) as usize;
    if meta_type != ITEM_METADATA || (16 + meta_size as u64) > file_len {
        return Err(Error::InvalidFormat(
            "Invalid camera metadata block in .mcraw".into(),
        ));
    }
    let mut cam_json_bytes = vec![0u8; meta_size];
    read_exact_at(&file, &mut cam_json_bytes, 16)?;
    let cam_json = String::from_utf8_lossy(&cam_json_bytes);

    // 3. Read trailer BUFFER_INDEX at (file_len - 24)
    let mut trailer = [0u8; 24];
    read_exact_at(&file, &mut trailer, file_len - 24)?;
    let idx_item_type = u32::from_le_bytes(trailer[0..4].try_into().unwrap());
    let magic = i32::from_le_bytes(trailer[8..12].try_into().unwrap());
    let num_offsets = i32::from_le_bytes(trailer[12..16].try_into().unwrap());
    let index_data_offset = i64::from_le_bytes(trailer[16..24].try_into().unwrap());

    if idx_item_type != ITEM_BUFFER_INDEX || magic != MCRAW_INDEX_MAGIC || num_offsets <= 0 {
        return Err(Error::CorruptData(
            "Corrupt or missing .mcraw trailer BUFFER_INDEX".into(),
        ));
    }

    let num_frames = num_offsets as usize;
    if index_data_offset < 16
        || (index_data_offset as u64) + (num_frames as u64) * 16 > file_len
    {
        return Err(Error::CorruptData(
            "Invalid indexDataOffset in .mcraw trailer".into(),
        ));
    }

    // 4. Read video frame offset table (16 bytes per entry: i64 offset, i64 timestamp_ns)
    let mut raw_index_bytes = vec![0u8; num_frames * 16];
    read_exact_at(&file, &mut raw_index_bytes, index_data_offset as u64)?;

    let mut raw_offsets: Vec<(u64, i64)> = Vec::with_capacity(num_frames);
    let mut max_video_offset = 0u64;
    for chunk in raw_index_bytes.chunks_exact(16) {
        let off = i64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let ts = i64::from_le_bytes(chunk[8..16].try_into().unwrap());
        if off >= 16 {
            let off_u64 = off as u64;
            if off_u64 > max_video_offset {
                max_video_offset = off_u64;
            }
            raw_offsets.push((off_u64, ts));
        }
    }
    if raw_offsets.is_empty() {
        return Err(Error::InvalidFormat(
            ".mcraw file contains no video frames".into(),
        ));
    }
    raw_offsets.sort_by_key(|&(_, ts)| ts);

    // 5. Parse camera metadata JSON
    let mut black_level = json_extract_f64(&cam_json, "blackLevel").unwrap_or(64.0) as i32;
    let mut white_level = json_extract_f64(&cam_json, "whiteLevel").unwrap_or(1023.0) as i32;
    let aperture = json_extract_f64(&cam_json, "apertures").unwrap_or(0.0);
    let focal_length = json_extract_f64(&cam_json, "focalLengths").unwrap_or(0.0);
    let audio_sample_rate =
        json_extract_f64(&cam_json, "audioSampleRate").unwrap_or(48000.0) as u32;
    let audio_channels = json_extract_f64(&cam_json, "audioChannels").unwrap_or(2.0) as u16;

    let brand = json_extract_str(&cam_json, "build.manufacturer")
        .or_else(|| json_extract_str(&cam_json, "build.brand"))
        .unwrap_or_default();
    let model = json_extract_str(&cam_json, "build.model")
        .or_else(|| json_extract_str(&cam_json, "deviceModel"))
        .unwrap_or_else(|| String::from("MotionCam"));
    let camera_name = if !brand.is_empty()
        && !model.to_ascii_lowercase().starts_with(&brand.to_ascii_lowercase())
    {
        format!("{} {}", brand.trim(), model.trim())
    } else {
        model.trim().to_string()
    };

    let sensor_arr = json_extract_str(&cam_json, "sensorArrangement")
        .or_else(|| json_extract_str(&cam_json, "sensorArrangment"))
        .unwrap_or_else(|| String::from("rggb"));
    let cfa_pattern = match sensor_arr.to_ascii_lowercase().as_str() {
        "gbrg" => CfaPattern::Gbrg,
        "grbg" => CfaPattern::Grbg,
        "bggr" => CfaPattern::Bggr,
        _ => CfaPattern::Rggb,
    };

    let illum1_str = json_extract_str(&cam_json, "colorIlluminant1").unwrap_or_default();
    let illum2_str = json_extract_str(&cam_json, "colorIlluminant2").unwrap_or_default();
    let mut calibration_illuminant1 = parse_illuminant(&illum1_str, ILLUMINANT_D65);
    let mut calibration_illuminant2 = if !illum2_str.is_empty() {
        Some(parse_illuminant(&illum2_str, ILLUMINANT_STD_A))
    } else {
        None
    };

    let mut color_matrix1 = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let mut color_matrix1_source = "default-identity";
    if let Some(m1) = json_extract_matrix9(&cam_json, "colorMatrix1") {
        if is_valid_color_matrix(&m1) {
            color_matrix1 = m1;
            color_matrix1_source = "MCRAW colorMatrix1";
        }
    }
    let mut color_matrix2 = json_extract_matrix9(&cam_json, "colorMatrix2")
        .filter(|m| is_valid_color_matrix(m));

    if color_matrix1_source == "default-identity" {
        if let Some(m2) = color_matrix2 {
            color_matrix1 = m2;
            color_matrix1_source = "MCRAW colorMatrix2";
        } else if let Some(profile) = lookup_camera_profile(&camera_name) {
            calibration_illuminant1 = profile.illuminant1;
            color_matrix1 = profile.color_matrix1;
            color_matrix1_source = "fallback-table";
            calibration_illuminant2 = profile.illuminant2;
            color_matrix2 = profile.color_matrix2;
        }
    }

    let forward_matrix1 = json_extract_matrix9(&cam_json, "forwardMatrix1")
        .filter(|m| is_valid_color_matrix(m));
    let forward_matrix2 = json_extract_matrix9(&cam_json, "forwardMatrix2")
        .filter(|m| is_valid_color_matrix(m));

    // 6. Read each video frame's 8-byte BUFFER item header (and first frame's per-frame METADATA)
    let mut frames = Vec::with_capacity(raw_offsets.len());
    let mut width = 0u32;
    let mut height = 0u32;
    let mut compression_type = MCRAW_COMP_DEFAULT;
    let mut stored_pixel_format = 16u32;
    let mut rotation_degrees = 0i32;
    let mut rtci = RtciTime::default();
    let mut meta0 = FrameDynamicMeta {
        focal_length_mm: focal_length.round() as u16,
        aperture_x100: (aperture * 100.0).round() as u16,
        lens_name: if focal_length > 0.0 {
            format!("{:.1}mm f/{:.2}", focal_length, aperture)
        } else {
            String::from("MotionCam Lens")
        },
        ..FrameDynamicMeta::default()
    };

    for (frame_idx, &(block_off, ts_ns)) in raw_offsets.iter().enumerate() {
        let mut item_hdr = [0u8; 8];
        read_exact_at(&file, &mut item_hdr, block_off)?;
        let item_type = u32::from_le_bytes(item_hdr[0..4].try_into().unwrap());
        let item_size = u32::from_le_bytes(item_hdr[4..8].try_into().unwrap());
        if item_type != ITEM_BUFFER {
            return Err(Error::CorruptData(format!(
                "Expected BUFFER item at offset {block_off}, found type {item_type}"
            )));
        }

        let payload_offset = block_off + 8;
        let timestamp_us = if ts_ns > 0 { (ts_ns as u64) / 1000 } else { 0 };

        // Inspect first frame's companion METADATA block immediately following the BUFFER payload
        if frame_idx == 0 {
            let fmeta_off = payload_offset + (item_size as u64);
            let mut fmeta_hdr = [0u8; 8];
            if read_exact_at(&file, &mut fmeta_hdr, fmeta_off).is_ok() {
                let fm_type = u32::from_le_bytes(fmeta_hdr[0..4].try_into().unwrap());
                let fm_size = u32::from_le_bytes(fmeta_hdr[4..8].try_into().unwrap()) as usize;
                if fm_type == ITEM_METADATA && fm_size > 0 && fm_size < 512 * 1024 {
                    // Only the first ~1 KB is needed for scalar fields (before lensShadingMap)
                    let read_len = fm_size.min(2048);
                    let mut fm_bytes = vec![0u8; read_len];
                    if read_exact_at(&file, &mut fm_bytes, fmeta_off + 8).is_ok() {
                        let fm_json = String::from_utf8_lossy(&fm_bytes);
                        width = json_extract_f64(&fm_json, "width").unwrap_or(0.0) as u32;
                        height = json_extract_f64(&fm_json, "height").unwrap_or(0.0) as u32;
                        if let Some(dyn_wl) = json_extract_f64(&fm_json, "dynamicWhiteLevel") {
                            if dyn_wl > 0.0 {
                                white_level = dyn_wl as i32;
                            }
                        }
                        if let Some(dyn_bl) = json_extract_f64(&fm_json, "dynamicBlackLevel") {
                            if dyn_bl >= 0.0 {
                                black_level = dyn_bl as i32;
                            }
                        }
                        if let Some(iso) = json_extract_f64(&fm_json, "iso") {
                            meta0.iso_value = iso.max(1.0) as u32;
                        }
                        if let Some(exp_ns) = json_extract_f64(&fm_json, "exposureTime") {
                            meta0.shutter_us = ((exp_ns / 1000.0).round() as u64).max(1);
                        }
                        if let Some(comp) = json_extract_f64(&fm_json, "compressionType") {
                            compression_type = comp as u16;
                        }
                        if let Some(is_comp) = json_extract_bool(&fm_json, "isCompressed") {
                            if !is_comp {
                                compression_type = 0;
                            }
                        }
                        if let Some(pf) = json_extract_str(&fm_json, "pixelFormat") {
                            if pf.starts_with("raw10") {
                                stored_pixel_format = 10;
                            } else if pf.starts_with("raw12") {
                                stored_pixel_format = 12;
                            } else {
                                stored_pixel_format = 16;
                            }
                        }
                        if let Some(neutral) = json_extract_array3(&fm_json, "asShotNeutral") {
                            let n0 = neutral[0].max(1e-4) as f32;
                            let n1 = neutral[1].max(1e-4) as f32;
                            let n2 = neutral[2].max(1e-4) as f32;
                            meta0.wb_gain_r = 1.0 / n0;
                            meta0.wb_gain_g = 1.0 / n1;
                            meta0.wb_gain_b = 1.0 / n2;
                        }
                        if let Some(recvd_ms) = json_extract_f64(&fm_json, "recvdTimestampMs") {
                            if recvd_ms > 1_000_000_000_000.0 {
                                rtci = RtciTime::from_unix_secs((recvd_ms / 1000.0) as u64);
                            }
                        }
                        let _ = json_extract_f64(&fm_json, "orientation").map(|o| {
                            // MotionCam orientation is kept upright in DNG (matching libmcraw / mlvcore)
                            let _ = o;
                            rotation_degrees = 0;
                        });
                    }
                }
            }
        }

        frames.push(MlvFrameEntry {
            chunk_idx: 0,
            frame_number: frame_idx as u32,
            timestamp_us,
            payload_offset,
            payload_size: item_size,
            meta_idx: 0,
        });
    }

    if width == 0 || height == 0 {
        return Err(Error::InvalidFormat(
            "Could not determine frame dimensions from .mcraw metadata".into(),
        ));
    }

    let bits_per_pixel = if white_level <= 0x3FF {
        10
    } else if white_level <= 0xFFF {
        12
    } else if white_level <= 0x3FFF {
        14
    } else {
        16
    };

    // 7. Compute frame rate from first and last frame timestamps
    let (fps_nom, fps_denom) = if frames.len() > 1 {
        let total_us = frames
            .last()
            .unwrap()
            .timestamp_us
            .saturating_sub(frames[0].timestamp_us);
        if total_us > 0 {
            let avg_fps = ((frames.len() - 1) as f64) * 1_000_000.0 / (total_us as f64);
            let nom = (avg_fps * 1000.0).round() as u32;
            (nom.max(1000), 1000u32)
        } else {
            (24000, 1000)
        }
    } else {
        (24000, 1000)
    };

    // 8. Scan from max_video_offset to index_data_offset for AUDIO_INDEX (type = 4)
    let mut audio_entries = Vec::new();
    let mut audio_info = None;
    let mut scan_pos = max_video_offset;
    let index_u64 = index_data_offset as u64;
    while scan_pos + 8 <= index_u64 {
        let mut hdr8 = [0u8; 8];
        if read_exact_at(&file, &mut hdr8, scan_pos).is_err() {
            break;
        }
        let itype = u32::from_le_bytes(hdr8[0..4].try_into().unwrap());
        let isize = u32::from_le_bytes(hdr8[4..8].try_into().unwrap()) as u64;

        if itype == ITEM_BUFFER
            || itype == ITEM_METADATA
            || itype == ITEM_AUDIO_DATA
            || itype == ITEM_AUDIO_DATA_METADATA
        {
            scan_pos += 8 + isize;
        } else if itype == ITEM_AUDIO_INDEX && isize >= 16 {
            let mut aidx_hdr = [0u8; 16];
            if read_exact_at(&file, &mut aidx_hdr, scan_pos + 8).is_ok() {
                let num_aud = i64::from_le_bytes(aidx_hdr[0..8].try_into().unwrap());
                if num_aud > 0 && num_aud < 1_000_000 {
                    let count = num_aud as usize;
                    let table_bytes = count * 16;
                    if scan_pos + 24 + (table_bytes as u64) <= file_len {
                        let mut aud_table = vec![0u8; table_bytes];
                        if read_exact_at(&file, &mut aud_table, scan_pos + 24).is_ok() {
                            let mut cum_pcm = 0u64;
                            for (a_idx, a_chunk) in aud_table.chunks_exact(16).enumerate() {
                                let a_off =
                                    i64::from_le_bytes(a_chunk[0..8].try_into().unwrap());
                                let a_ts =
                                    i64::from_le_bytes(a_chunk[8..16].try_into().unwrap());
                                if a_off >= 16 && (a_off as u64) + 8 <= file_len {
                                    let mut a_item = [0u8; 8];
                                    if read_exact_at(&file, &mut a_item, a_off as u64).is_ok() {
                                        let atype =
                                            u32::from_le_bytes(a_item[0..4].try_into().unwrap());
                                        let asize =
                                            u32::from_le_bytes(a_item[4..8].try_into().unwrap());
                                        if atype == ITEM_AUDIO_DATA && asize > 0 {
                                            audio_entries.push(MlvAudioEntry {
                                                chunk_idx: 0,
                                                frame_number: a_idx as u32,
                                                timestamp_us: if a_ts > 0 {
                                                    (a_ts as u64) / 1000
                                                } else {
                                                    0
                                                },
                                                payload_offset: (a_off as u64) + 8,
                                                payload_size: asize,
                                                cum_pcm_offset: cum_pcm,
                                            });
                                            cum_pcm += asize as u64;
                                        }
                                    }
                                }
                            }
                            if cum_pcm > 0 && audio_sample_rate > 0 && audio_channels > 0 {
                                audio_info = Some(MlvAudioInfo {
                                    sample_rate: audio_sample_rate,
                                    channels: audio_channels,
                                    bits_per_sample: 16,
                                    total_pcm_bytes: cum_pcm,
                                });
                            }
                        }
                    }
                }
            }
            break;
        } else {
            break;
        }
    }

    let mut block_counts = BTreeMap::new();
    block_counts.insert(String::from("MCRAW_VID"), frames.len());
    if !audio_entries.is_empty() {
        block_counts.insert(String::from("MCRAW_AUD"), audio_entries.len());
    }
    let _ = ITEM_BUFFER_INDEX_DATA;

    let mut clip = MlvClip {
        base_name,
        chunk_paths: vec![primary_path],
        chunks: vec![file],
        version_string: String::from("MCRAW v3"),
        video_guid: 0,
        video_class: compression_type,
        audio_class: if audio_info.is_some() { 1 } else { 0 },
        fps_nom,
        fps_denom,
        width,
        height,
        raw_buffer_width: 0,
        raw_buffer_height: 0,
        pan_x: 0,
        pan_y: 0,
        bits_per_pixel,
        black_level,
        white_level,
        levels: crate::levels::ResolvedLevels::from_file(black_level, white_level),
        focus_fix: None,
        diso_mode: 0,
        diso_iso_value: 0,
        dual_iso: None,
        bad_pixels: None,
        active_area: [0, 0, height as i32, width as i32],
        cfa_pattern,
        camera_name,
        camera_model_id: 0,
        rotation_degrees,
        calibration_illuminant1,
        color_matrix1,
        color_matrix1_source,
        calibration_illuminant2,
        color_matrix2,
        forward_matrix1,
        forward_matrix2,
        rtci,
        is_lj92: false,
        is_mcraw: true,
        is_braw: false,
        mcraw_compression_type: compression_type,
        mcraw_stored_format: stored_pixel_format,
        lj92_info: None,
        dynamic_metas: vec![meta0],
        rawc_sampling: None,
        stretch_y: 1.0,
        stretch_source: "none",
        frames,
        audio_info,
        audio_entries,
        block_counts,
    };
    clip.bad_pixels = crate::bad_pixels::detect_clip(&clip).map(std::sync::Arc::new);
    Ok(clip)
}

// =============================================================================
// Lightweight zero-dependency JSON field extractors for MotionCam metadata
// =============================================================================

fn find_json_key_value<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\"");
    let pos = json.find(&pattern)?;
    let after_key = &json[pos + pattern.len()..];
    let colon = after_key.find(':')?;
    Some(after_key[colon + 1..].trim_start())
}

fn json_extract_f64(json: &str, key: &str) -> Option<f64> {
    let mut val = find_json_key_value(json, key)?;
    if val.starts_with('[') {
        val = val[1..].trim_start();
    }
    if val.starts_with('"') {
        val = &val[1..];
        let end = val.find('"')?;
        return val[..end].trim().parse::<f64>().ok();
    }
    let end = val
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E')
        .unwrap_or(val.len());
    val[..end].parse::<f64>().ok()
}

fn json_extract_bool(json: &str, key: &str) -> Option<bool> {
    let val = find_json_key_value(json, key)?;
    if val.starts_with("true") {
        Some(true)
    } else if val.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_extract_str(json: &str, key: &str) -> Option<String> {
    let mut val = find_json_key_value(json, key)?;
    if val.starts_with('[') {
        val = val[1..].trim_start();
    }
    if !val.starts_with('"') {
        return None;
    }
    val = &val[1..];
    let end = val.find('"')?;
    Some(val[..end].to_string())
}

fn json_extract_n_floats(json: &str, key: &str, count: usize) -> Option<Vec<f64>> {
    let val = find_json_key_value(json, key)?;
    if !val.starts_with('[') {
        return None;
    }
    let close = val.find(']')?;
    let inner = &val[1..close];
    let mut out = Vec::with_capacity(count);
    for part in inner.split(',') {
        let trimmed = part.trim().trim_matches('"');
        if let Ok(v) = trimmed.parse::<f64>() {
            out.push(v);
            if out.len() == count {
                break;
            }
        }
    }
    if out.len() == count {
        Some(out)
    } else {
        None
    }
}

fn json_extract_matrix9(json: &str, key: &str) -> Option<[f64; 9]> {
    let v = json_extract_n_floats(json, key, 9)?;
    Some([v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8]])
}

fn json_extract_array3(json: &str, key: &str) -> Option<[f64; 3]> {
    let v = json_extract_n_floats(json, key, 3)?;
    Some([v[0], v[1], v[2]])
}

fn parse_illuminant(s: &str, default_val: u16) -> u16 {
    match s.trim().to_ascii_lowercase().as_str() {
        "standarda" | "std_a" | "a" => ILLUMINANT_STD_A,
        "d65" => ILLUMINANT_D65,
        "d50" => 23,
        "d55" => 20,
        "d75" => 22,
        _ => default_val,
    }
}

// =============================================================================
// MotionCam Raw Frame Decoder (Type 7 Default & Type 6 Legacy)
// =============================================================================

const ENCODING_BLOCK: usize = 64;
const HEADER_LENGTH: usize = 2;
const METADATA_OFFSET: usize = 16;

const ENCODING_BLOCK_LENGTH_V7: [usize; 17] = [
    0, 8, 16, 24, 32, 40, 48, 64, 64, 80, 80, 128, 128, 128, 128, 128, 128,
];

const LEGACY_BLOCK_SIZE: usize = 16;
const LEGACY_ENCODING_BLOCK: usize = LEGACY_BLOCK_SIZE * 2;
const ENCODING_BLOCK_LENGTH_V6: [usize; 17] = [
    0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 32, 32, 32, 32, 32, 32,
];

/// Decodes a MotionCam `.mcraw` video frame payload into little-endian `u16` pixels (`width * height * 2` bytes).
pub fn decode_mcraw_frame_to_u16_le(
    src: &[u8],
    width: usize,
    height: usize,
    compression_type: u16,
    dst_u16_le: &mut [u8],
) -> Result<()> {
    let expected_bytes = width * height * 2;
    if dst_u16_le.len() < expected_bytes {
        return Err(Error::InvalidFormat(
            "Destination buffer too small for decoded MCRAW frame".into(),
        ));
    }

    if compression_type == 0 {
        let copy_len = src.len().min(expected_bytes);
        dst_u16_le[..copy_len].copy_from_slice(&src[..copy_len]);
        if copy_len < expected_bytes {
            dst_u16_le[copy_len..expected_bytes].fill(0);
        }
        return Ok(());
    }

    if compression_type == MCRAW_COMP_DEFAULT {
        decode_v7(src, width, height, &mut dst_u16_le[..expected_bytes])
    } else if compression_type == MCRAW_COMP_LEGACY {
        decode_v6_legacy(src, width, height, &mut dst_u16_le[..expected_bytes])
    } else {
        Err(Error::InvalidFormat(format!(
            "Unsupported MCRAW compressionType: {compression_type}"
        )))
    }
}

#[inline]
fn decode_header_pair(input: &[u8]) -> (u8, u16) {
    let b0 = input[0];
    let b1 = input[1];
    let bits = (b0 >> 4) & 0x0F;
    let reference = (((b0 & 0x0F) as u16) << 8) | (b1 as u16);
    (bits, reference)
}

#[inline]
fn decode_v7_1(out: &mut [u16; 64], inp: &[u8]) {
    for lane in 0..8 {
        let p = inp[lane] as u16;
        out[lane] = p & 1;
        out[8 + lane] = (p >> 1) & 1;
        out[16 + lane] = (p >> 2) & 1;
        out[24 + lane] = (p >> 3) & 1;
        out[32 + lane] = (p >> 4) & 1;
        out[40 + lane] = (p >> 5) & 1;
        out[48 + lane] = (p >> 6) & 1;
        out[56 + lane] = (p >> 7) & 1;
    }
}

#[inline]
fn decode_v7_2_half(out: &mut [u16], inp: &[u8]) {
    for lane in 0..8 {
        let p = inp[lane] as u16;
        out[lane] = p & 0x03;
        out[8 + lane] = (p >> 2) & 0x03;
        out[16 + lane] = (p >> 4) & 0x03;
        out[24 + lane] = (p >> 6) & 0x03;
    }
}

#[inline]
fn decode_v7_2(out: &mut [u16; 64], inp: &[u8]) {
    decode_v7_2_half(&mut out[0..32], &inp[0..8]);
    decode_v7_2_half(&mut out[32..64], &inp[8..16]);
}

#[inline]
fn decode_v7_3(out: &mut [u16; 64], inp: &[u8]) {
    for lane in 0..8 {
        let p0 = inp[lane] as u16;
        let p1 = inp[8 + lane] as u16;
        let p2 = inp[16 + lane] as u16;

        out[lane] = p0 & 0x07;
        out[8 + lane] = (p0 >> 3) & 0x07;
        out[16 + lane] = ((p0 >> 6) & 0x03) | (((p2 >> 6) & 0x01) << 2);

        out[24 + lane] = p1 & 0x07;
        out[32 + lane] = (p1 >> 3) & 0x07;
        out[40 + lane] = ((p1 >> 6) & 0x03) | (((p2 >> 7) & 0x01) << 2);

        out[48 + lane] = p2 & 0x07;
        out[56 + lane] = (p2 >> 3) & 0x07;
    }
}

#[inline]
fn decode_v7_4_quarter(out: &mut [u16], inp: &[u8]) {
    for lane in 0..8 {
        let p = inp[lane] as u16;
        out[lane] = p & 0x0F;
        out[8 + lane] = (p >> 4) & 0x0F;
    }
}

#[inline]
fn decode_v7_4(out: &mut [u16; 64], inp: &[u8]) {
    decode_v7_4_quarter(&mut out[0..16], &inp[0..8]);
    decode_v7_4_quarter(&mut out[16..32], &inp[8..16]);
    decode_v7_4_quarter(&mut out[32..48], &inp[16..24]);
    decode_v7_4_quarter(&mut out[48..64], &inp[24..32]);
}

#[inline]
fn decode_v7_5(out: &mut [u16; 64], inp: &[u8]) {
    for lane in 0..8 {
        let p0 = inp[lane] as u16;
        let p1 = inp[8 + lane] as u16;
        let p2 = inp[16 + lane] as u16;
        let p3 = inp[24 + lane] as u16;
        let p4 = inp[32 + lane] as u16;

        out[lane] = p0 & 0x1F;
        out[8 + lane] = p1 & 0x1F;
        out[16 + lane] = p2 & 0x1F;
        out[24 + lane] = p3 & 0x1F;
        out[32 + lane] = p4 & 0x1F;

        out[40 + lane] = ((p0 >> 5) & 0x07) | (((p3 >> 5) & 0x03) << 3);
        out[48 + lane] = ((p1 >> 5) & 0x07) | (((p4 >> 5) & 0x03) << 3);
        out[56 + lane] =
            ((p2 >> 5) & 0x07) | (((p3 >> 7) & 0x01) << 3) | (((p4 >> 7) & 0x01) << 4);
    }
}

#[inline]
fn decode_v7_6(out: &mut [u16; 64], inp: &[u8]) {
    for lane in 0..8 {
        let p0 = inp[lane] as u16;
        let p1 = inp[8 + lane] as u16;
        let p2 = inp[16 + lane] as u16;
        let p3 = inp[24 + lane] as u16;
        let p4 = inp[32 + lane] as u16;
        let p5 = inp[40 + lane] as u16;

        out[lane] = p0 & 0x3F;
        out[8 + lane] = p1 & 0x3F;
        out[16 + lane] = p2 & 0x3F;
        out[24 + lane] = p3 & 0x3F;
        out[32 + lane] = p4 & 0x3F;
        out[40 + lane] = p5 & 0x3F;

        out[48 + lane] =
            ((p0 >> 6) & 0x03) | (((p1 >> 6) & 0x03) << 2) | (((p2 >> 6) & 0x03) << 4);
        out[56 + lane] =
            ((p3 >> 6) & 0x03) | (((p4 >> 6) & 0x03) << 2) | (((p5 >> 6) & 0x03) << 4);
    }
}

#[inline]
fn decode_v7_8(out: &mut [u16; 64], inp: &[u8]) {
    for i in 0..64 {
        out[i] = inp[i] as u16;
    }
}

#[inline]
fn decode_v7_10(out: &mut [u16; 64], inp: &[u8]) {
    for lane in 0..8 {
        let p0 = inp[lane] as u16;
        let p1 = inp[8 + lane] as u16;
        let p2 = inp[16 + lane] as u16;
        let p3 = inp[24 + lane] as u16;
        let p4 = inp[32 + lane] as u16;
        let p5 = inp[40 + lane] as u16;
        let p6 = inp[48 + lane] as u16;
        let p7 = inp[56 + lane] as u16;
        let p8 = inp[64 + lane] as u16;
        let p9 = inp[72 + lane] as u16;

        out[lane] = p0 | ((p4 & 0x03) << 8);
        out[8 + lane] = p1 | ((p4 & 0x0C) << 6);
        out[16 + lane] = p2 | ((p4 & 0x30) << 4);
        out[24 + lane] = p3 | ((p4 & 0xC0) << 2);

        out[32 + lane] = p5 | ((p9 & 0x03) << 8);
        out[40 + lane] = p6 | ((p9 & 0x0C) << 6);
        out[48 + lane] = p7 | ((p9 & 0x30) << 4);
        out[56 + lane] = p8 | ((p9 & 0xC0) << 2);
    }
}

#[inline]
fn decode_v7_16(out: &mut [u16; 64], inp: &[u8]) {
    for i in 0..64 {
        out[i] = u16::from_le_bytes([inp[i * 2], inp[i * 2 + 1]]);
    }
}

#[inline]
fn decode_v7_block(
    out: &mut [u16; 64],
    bits: u16,
    input: &[u8],
    offset: usize,
) -> usize {
    let b = (bits as usize).min(16);
    let block_len = ENCODING_BLOCK_LENGTH_V7[b];
    if offset + block_len > input.len() {
        out.fill(0);
        return input.len().saturating_sub(offset);
    }
    let slice = &input[offset..offset + block_len];
    match b {
        0 => out.fill(0),
        1 => decode_v7_1(out, slice),
        2 => decode_v7_2(out, slice),
        3 => decode_v7_3(out, slice),
        4 => decode_v7_4(out, slice),
        5 => decode_v7_5(out, slice),
        6 => decode_v7_6(out, slice),
        7 | 8 => decode_v7_8(out, slice),
        9 | 10 => decode_v7_10(out, slice),
        _ => decode_v7_16(out, slice),
    }
    block_len
}

fn decode_v7_metadata_stream(input: &[u8], mut offset: usize) -> Result<Vec<u16>> {
    if offset + 4 > input.len() {
        return Err(Error::CorruptData("Truncated MCRAW v7 metadata header".into()));
    }
    let num_blocks = u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    let padded_len = ((num_blocks + ENCODING_BLOCK - 1) / ENCODING_BLOCK) * ENCODING_BLOCK;
    let mut out = vec![0u16; padded_len.max(ENCODING_BLOCK)];
    let mut blk = [0u16; ENCODING_BLOCK];

    let mut i = 0usize;
    while i < num_blocks {
        if offset + HEADER_LENGTH > input.len() {
            break;
        }
        let (bits, reference) = decode_header_pair(&input[offset..offset + 2]);
        offset += HEADER_LENGTH;
        offset += decode_v7_block(&mut blk, bits as u16, input, offset);
        for x in 0..ENCODING_BLOCK {
            out[i + x] = blk[x].wrapping_add(reference);
        }
        i += ENCODING_BLOCK;
    }
    out.truncate(num_blocks);
    Ok(out)
}

fn decode_v7(src: &[u8], width: usize, height: usize, dst_u16_le: &mut [u8]) -> Result<()> {
    if src.len() < METADATA_OFFSET {
        return Err(Error::CorruptData("MCRAW v7 frame buffer too short".into()));
    }
    let encoded_width = u32::from_le_bytes(src[0..4].try_into().unwrap()) as usize;
    let encoded_height = u32::from_le_bytes(src[4..8].try_into().unwrap()) as usize;
    let bits_offset = u32::from_le_bytes(src[8..12].try_into().unwrap()) as usize;
    let refs_offset = u32::from_le_bytes(src[12..16].try_into().unwrap()) as usize;

    if bits_offset > src.len()
        || refs_offset > src.len()
        || encoded_width == 0
        || encoded_width % ENCODING_BLOCK != 0
        || encoded_width < width
    {
        return Err(Error::CorruptData(format!(
            "Invalid MCRAW v7 frame header (encoded={}x{}, target={}x{})",
            encoded_width, encoded_height, width, height
        )));
    }

    let bits = decode_v7_metadata_stream(src, bits_offset)?;
    let refs = decode_v7_metadata_stream(src, refs_offset)?;

    let mut offset = METADATA_OFFSET;
    let mut p0 = [0u16; ENCODING_BLOCK];
    let mut p1 = [0u16; ENCODING_BLOCK];
    let mut p2 = [0u16; ENCODING_BLOCK];
    let mut p3 = [0u16; ENCODING_BLOCK];

    let mut row0 = vec![0u8; encoded_width * 2];
    let mut row1 = vec![0u8; encoded_width * 2];
    let mut row2 = vec![0u8; encoded_width * 2];
    let mut row3 = vec![0u8; encoded_width * 2];

    let row_copy_bytes = width * 2;
    let mut metadata_idx = 0usize;
    let mut y = 0usize;

    while y < encoded_height {
        let mut x = 0usize;
        while x < encoded_width {
            if metadata_idx + 4 > bits.len() || metadata_idx + 4 > refs.len() {
                break;
            }
            let b0 = bits[metadata_idx];
            let b1 = bits[metadata_idx + 1];
            let b2 = bits[metadata_idx + 2];
            let b3 = bits[metadata_idx + 3];

            let r0 = refs[metadata_idx];
            let r1 = refs[metadata_idx + 1];
            let r2 = refs[metadata_idx + 2];
            let r3 = refs[metadata_idx + 3];

            offset += decode_v7_block(&mut p0, b0, src, offset);
            offset += decode_v7_block(&mut p1, b1, src, offset);
            offset += decode_v7_block(&mut p2, b2, src, offset);
            offset += decode_v7_block(&mut p3, b3, src, offset);

            for half in 0..(ENCODING_BLOCK / 2) {
                let col_byte = (x + half * 2) * 2;
                let v00 = p0[half].wrapping_add(r0).to_le_bytes();
                let v01 = p1[half].wrapping_add(r1).to_le_bytes();
                row0[col_byte] = v00[0];
                row0[col_byte + 1] = v00[1];
                row0[col_byte + 2] = v01[0];
                row0[col_byte + 3] = v01[1];

                let v10 = p2[half].wrapping_add(r2).to_le_bytes();
                let v11 = p3[half].wrapping_add(r3).to_le_bytes();
                row1[col_byte] = v10[0];
                row1[col_byte + 1] = v10[1];
                row1[col_byte + 2] = v11[0];
                row1[col_byte + 3] = v11[1];

                let idx2 = (ENCODING_BLOCK / 2) + half;
                let v20 = p0[idx2].wrapping_add(r0).to_le_bytes();
                let v21 = p1[idx2].wrapping_add(r1).to_le_bytes();
                row2[col_byte] = v20[0];
                row2[col_byte + 1] = v20[1];
                row2[col_byte + 2] = v21[0];
                row2[col_byte + 3] = v21[1];

                let v30 = p2[idx2].wrapping_add(r2).to_le_bytes();
                let v31 = p3[idx2].wrapping_add(r3).to_le_bytes();
                row3[col_byte] = v30[0];
                row3[col_byte + 1] = v30[1];
                row3[col_byte + 2] = v31[0];
                row3[col_byte + 3] = v31[1];
            }

            metadata_idx += 4;
            x += ENCODING_BLOCK;
        }

        for (dy, rbuf) in [&row0, &row1, &row2, &row3].into_iter().enumerate() {
            let out_y = y + dy;
            if out_y < height {
                let dst_off = out_y * row_copy_bytes;
                dst_u16_le[dst_off..dst_off + row_copy_bytes]
                    .copy_from_slice(&rbuf[..row_copy_bytes]);
            }
        }
        y += 4;
    }

    Ok(())
}

// =============================================================================
// Legacy Type 6 Decoder (`RawData_Legacy.cpp` port)
// =============================================================================

fn decode_v6_block(
    out: &mut [u16; LEGACY_BLOCK_SIZE],
    input: &[u8],
    offset: usize,
) -> (u16, usize) {
    if offset + HEADER_LENGTH >= input.len() {
        out.fill(0);
        return (0, input.len().saturating_sub(offset));
    }
    let (raw_bits, reference) = decode_header_pair(&input[offset..offset + 2]);
    let bits = (raw_bits as usize).min(16);
    let block_len = ENCODING_BLOCK_LENGTH_V6[bits];
    if offset + HEADER_LENGTH + block_len >= input.len() {
        out.fill(0);
        return (reference, input.len().saturating_sub(offset));
    }
    let inp = &input[offset + HEADER_LENGTH..offset + HEADER_LENGTH + block_len];
    match bits {
        0 => out.fill(0),
        1 => {
            for i in 0..2 {
                let b = inp[i] as u16;
                for bit in 0..8 {
                    out[i * 8 + bit] = (b >> (7 - bit)) & 1;
                }
            }
        }
        2 => {
            for i in 0..4 {
                let b = inp[i] as u16;
                out[i * 4] = (b >> 6) & 0x03;
                out[i * 4 + 1] = (b >> 4) & 0x03;
                out[i * 4 + 2] = (b >> 2) & 0x03;
                out[i * 4 + 3] = b & 0x03;
            }
        }
        4 => {
            for i in 0..8 {
                let b = inp[i] as u16;
                out[i * 2] = (b >> 4) & 0x0F;
                out[i * 2 + 1] = b & 0x0F;
            }
        }
        8 => {
            for i in 0..16 {
                out[i] = inp[i] as u16;
            }
        }
        16 => {
            for i in 0..16 {
                out[i] = ((inp[i * 2] as u16) << 8) | (inp[i * 2 + 1] as u16);
            }
        }
        b => {
            // Bitstream MSB-first unpack for 3, 5, 6, 7, 9, 10 bits
            let mask = (1u32 << b) - 1;
            let mut bit_pos = 0usize;
            for item in out.iter_mut() {
                let byte_idx = bit_pos / 8;
                let bit_off = bit_pos % 8;
                let b0 = inp.get(byte_idx).copied().unwrap_or(0) as u32;
                let b1 = inp.get(byte_idx + 1).copied().unwrap_or(0) as u32;
                let b2 = inp.get(byte_idx + 2).copied().unwrap_or(0) as u32;
                let combined = (b0 << 16) | (b1 << 8) | b2;
                let shift = 24 - b - bit_off;
                *item = ((combined >> shift) & mask) as u16;
                bit_pos += b;
            }
        }
    }
    (reference, HEADER_LENGTH + block_len)
}

fn decode_v6_legacy(src: &[u8], width: usize, height: usize, dst_u16_le: &mut [u8]) -> Result<()> {
    let padded_width =
        ((width + LEGACY_ENCODING_BLOCK - 1) / LEGACY_ENCODING_BLOCK) * LEGACY_ENCODING_BLOCK;
    let mut row = vec![0u8; padded_width * 2];
    let mut p0 = [0u16; LEGACY_BLOCK_SIZE];
    let mut p1 = [0u16; LEGACY_BLOCK_SIZE];
    let mut offset = 0usize;
    let row_copy_bytes = width * 2;

    for y in 0..height {
        let mut x = 0usize;
        while x < padded_width {
            let (ref0, adv0) = decode_v6_block(&mut p0, src, offset);
            offset += adv0;
            let (ref1, adv1) = decode_v6_block(&mut p1, src, offset);
            offset += adv1;
            for half in 0..LEGACY_BLOCK_SIZE {
                let col_byte = (x + half * 2) * 2;
                let v0 = p0[half].wrapping_add(ref0).to_le_bytes();
                let v1 = p1[half].wrapping_add(ref1).to_le_bytes();
                row[col_byte] = v0[0];
                row[col_byte + 1] = v0[1];
                row[col_byte + 2] = v1[0];
                row[col_byte + 3] = v1[1];
            }
            x += LEGACY_ENCODING_BLOCK;
        }
        let dst_off = y * row_copy_bytes;
        dst_u16_le[dst_off..dst_off + row_copy_bytes].copy_from_slice(&row[..row_copy_bytes]);
    }
    Ok(())
}
