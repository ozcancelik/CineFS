//! UI-independent description of a mounted volume (`MountSummary`) — the single source of truth
//! that the macOS app (via JSON over the C ABI), the Windows app and the CLI all render.

use crate::vfs::CineFs;

/// JSON string escaping (quotes, backslashes, control characters).
pub fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Short human label for a payload mode description.
pub fn payload_badge(description: &str) -> &'static str {
    if description.contains("Passthrough") {
        "LJ92 Passthrough"
    } else if description.contains("Native") {
        "Native Packed"
    } else {
        "16-bit Unpacked"
    }
}

#[derive(Clone, Debug)]
pub struct ClipSummary {
    pub folder_name: String,
    pub camera_model: String,
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u32,
    pub fps: f64,
    pub frame_count: usize,
    pub has_audio: bool,
    pub payload_mode: String,
    pub payload_badge: &'static str,
    pub is_passthrough: bool,
    /// Vertical pixel stretch that restores square pixels (1.0 = none).
    pub stretch: f64,
    /// Axis the stretch applies to: "none" | "horizontal" | "vertical".
    pub stretch_axis: &'static str,
    /// `true` when the stretch is baked into the DNG pixels (square pixels, any application).
    pub stretch_baked: bool,
    /// "none" | "RAWC" | "manual"
    pub stretch_source: &'static str,
    /// Black level written to the DNG (mean over channels) and white level.
    pub black_level: u32,
    pub white_level: u32,
    /// "file" | "ob" | "fallback"
    pub level_source: &'static str,
    /// Number of focus pixels corrected per frame (0 = none).
    pub focus_pixels: usize,
}

#[derive(Clone, Debug)]
pub struct MountSummary {
    pub source_paths: Vec<String>,
    pub mount_point: String,
    pub nfs_port: u16,
    pub total_bytes: u64,
    pub total_files: u64,
    pub index_time_ms: f64,
    pub clips: Vec<ClipSummary>,
}

impl ClipSummary {
    pub fn from_mounted(c: &crate::vfs::MountedClip) -> Self {
        let desc = c.dng_template.mode.description();
        // clip.stretch_y is the y/x sampling ratio: >1 stretch vertically, <1 stretch horizontally.
        let r = c.clip.stretch_y;
        let (factor, axis) = if !r.is_finite() || r <= 0.0 || (r - 1.0).abs() < 0.01 {
            (1.0, "none")
        } else {
            let f = if r < 1.0 { 1.0 / r } else { r };
            let f = if (f - f.round()).abs() < 0.01 { f.round() } else { f };
            (f, if r < 1.0 { "horizontal" } else { "vertical" })
        };
        Self {
            folder_name: c.folder_name.clone(),
            camera_model: c.clip.camera_name.clone(),
            width: c.dng_template.out_width,
            height: c.dng_template.out_height,
            bits_per_pixel: c.clip.bits_per_pixel,
            fps: c.clip.fps_f64(),
            frame_count: c.clip.frames.len(),
            has_audio: c.wav.is_some(),
            payload_mode: desc.to_string(),
            payload_badge: payload_badge(desc),
            is_passthrough: desc.contains("Passthrough"),
            stretch: factor,
            stretch_axis: axis,
            stretch_baked: c.dng_template.bake.is_some(),
            stretch_source: c.clip.stretch_source,
            black_level: c.clip.levels.black,
            white_level: c.clip.levels.white.min(
                if c.dng_template.dng_bits_per_sample < 16 {
                    (1u32 << c.dng_template.dng_bits_per_sample) - 1
                } else {
                    u32::MAX
                },
            ),
            level_source: c.clip.levels.source,
            focus_pixels: c.clip.focus_fix.as_ref().map_or(0, |f| f.pixel_count()),
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"folderName\":\"{}\",\"cameraModel\":\"{}\",\"width\":{},\"height\":{},\
             \"bitsPerPixel\":{},\"fps\":{:.3},\"frameCount\":{},\"hasAudio\":{},\"payloadMode\":\"{}\",\
             \"stretch\":{:.4},\"stretchAxis\":\"{}\",\"stretchBaked\":{},\"stretchSource\":\"{}\",\
             \"blackLevel\":{},\"whiteLevel\":{},\"levelSource\":\"{}\",\"focusPixels\":{}}}",
            escape_json_str(&self.folder_name),
            escape_json_str(&self.camera_model),
            self.width,
            self.height,
            self.bits_per_pixel,
            self.fps,
            self.frame_count,
            self.has_audio,
            escape_json_str(&self.payload_mode),
            self.stretch,
            self.stretch_axis,
            self.stretch_baked,
            self.stretch_source,
            self.black_level,
            self.white_level,
            self.level_source,
            self.focus_pixels,
        )
    }
}

impl MountSummary {
    pub fn build(
        vfs: &CineFs,
        source_paths: Vec<String>,
        mount_point: String,
        nfs_port: u16,
        index_time_ms: f64,
    ) -> Self {
        let (total_bytes, total_files) = vfs.total_volume_stats();
        Self {
            source_paths,
            mount_point,
            nfs_port,
            total_bytes,
            total_files,
            index_time_ms,
            clips: vfs.clips.iter().map(|c| ClipSummary::from_mounted(c)).collect(),
        }
    }

    pub fn to_json(&self) -> String {
        let clips: Vec<String> = self.clips.iter().map(|c| c.to_json()).collect();
        let sources: Vec<String> = self
            .source_paths
            .iter()
            .map(|p| format!("\"{}\"", escape_json_str(p)))
            .collect();
        format!(
            "{{\"sourcePath\":\"{}\",\"sourcePaths\":[{}],\"mountPoint\":\"{}\",\"nfsPort\":{},\
             \"totalBytes\":{},\"totalFiles\":{},\"indexTimeMs\":{:.2},\"clips\":[{}]}}",
            escape_json_str(self.source_paths.first().map(String::as_str).unwrap_or("")),
            sources.join(","),
            escape_json_str(&self.mount_point),
            self.nfs_port,
            self.total_bytes,
            self.total_files,
            self.index_time_ms,
            clips.join(",")
        )
    }
}

/// Mounts several sources into ONE virtual filesystem (one volume for a multi-selection).
/// Sources that fail to open are skipped (and logged); errors only if nothing could be opened.
pub fn mount_many(
    vfs: &mut CineFs,
    sources: &[std::path::PathBuf],
    mode: crate::DngPayloadMode,
) -> crate::Result<()> {
    let mut first_err = None;
    for s in sources {
        if let Err(e) = vfs.mount_path(s, mode) {
            crate::log::write("WARN", &format!("skipping '{}': {e}", s.display()));
            first_err.get_or_insert(e);
        }
    }
    if vfs.clips.is_empty() {
        return Err(first_err.unwrap_or_else(|| {
            crate::Error::NotFound(String::from("no clips found in the selection"))
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_and_badge() {
        assert_eq!(escape_json_str("a\"b\\c\n\u{1}"), "a\\\"b\\\\c\\n\\u0001");
        assert_eq!(payload_badge("LJ92 Passthrough (zero decode)"), "LJ92 Passthrough");
        assert_eq!(payload_badge("Native 12-bit Packed"), "Native Packed");
        assert_eq!(payload_badge("Unpacked"), "16-bit Unpacked");
    }

    #[test]
    fn mount_many_rejects_empty_selection() {
        let mut vfs = CineFs::new(64 * 1024 * 1024);
        assert!(mount_many(&mut vfs, &[], crate::DngPayloadMode::Auto).is_err());
    }

    /// Opt-in end-to-end check on a real file: `CINEFS_TEST_MLV=/path/clip.mlv cargo test -p cinefs-core baked_dng_is_consistent -- --nocapture`.
    /// Verifies that for every frame the generated DNG header + payload size match, for both
    /// stretch methods.
    #[test]
    fn baked_dng_is_consistent() {
        let Ok(path) = std::env::var("CINEFS_TEST_MLV") else {
            return;
        };
        for bake in [true, false] {
            crate::dng::set_stretch_bake(bake);
            let mut vfs = CineFs::new(256 * 1024 * 1024);
            vfs.mount_path(&path, crate::DngPayloadMode::Auto).unwrap();
            let c = &vfs.clips[0];
            let frames = c.clip.frames.len().min(3);
            for f in 0..frames {
                let hdr = c.dng_template.build_header(&c.clip, f);
                let payload = c.dng_template.build_payload(&c.clip, f).unwrap();
                assert_eq!(
                    hdr.len() + payload.len(),
                    c.dng_template.file_size(&c.clip, f) as usize,
                    "size mismatch (bake={bake}, frame {f})"
                );
            }
            eprintln!(
                "bake={bake}: {}x{} -> {}x{}, mode={}",
                c.clip.width,
                c.clip.height,
                c.dng_template.out_width,
                c.dng_template.out_height,
                c.dng_template.mode.description()
            );
        }
        crate::dng::set_stretch_bake(true);
    }
}
