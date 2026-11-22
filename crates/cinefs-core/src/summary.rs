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
    /// Where the focus-pixel fix came from: `"none"` | `"map"` | `"detected"`.
    pub focus_source: &'static str,
    /// Recovered Dual ISO EV spread (0.0 = not Dual ISO).
    pub dual_iso_ev: f32,
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
            black_level: if c.clip.dual_iso.is_some() { 0 } else { c.clip.levels.black },
            white_level: if c.clip.dual_iso.is_some() {
                65535
            } else {
                c.clip.levels.white.min(
                    if c.dng_template.dng_bits_per_sample < 16 {
                        (1u32 << c.dng_template.dng_bits_per_sample) - 1
                    } else {
                        u32::MAX
                    },
                )
            },
            level_source: c.clip.levels.source,
            focus_pixels: c.clip.focus_fix.as_ref().map_or(0, |f| f.pixel_count()),
            focus_source: c.clip.focus_fix.as_ref().map_or("none", |f| f.source()),
            dual_iso_ev: c.clip.dual_iso.as_ref().map_or(0.0, |d| d.ev_stops),
        }
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"folderName\":\"{}\",\"cameraModel\":\"{}\",\"width\":{},\"height\":{},\
             \"bitsPerPixel\":{},\"fps\":{:.3},\"frameCount\":{},\"hasAudio\":{},\"payloadMode\":\"{}\",\
             \"stretch\":{:.4},\"stretchAxis\":\"{}\",\"stretchBaked\":{},\"stretchSource\":\"{}\",\
             \"blackLevel\":{},\"whiteLevel\":{},\"levelSource\":\"{}\",\"focusPixels\":{},\"focusSource\":\"{}\",\"dualIsoEv\":{:.2}}}",
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
            self.focus_source,
            self.dual_iso_ev,
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
    let files = expand_sources(sources);
    mount_files_with_progress(vfs, &files, mode, |_, _| {})
}

/// Progress of one clip file while a selection is being mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipLoad {
    /// Being opened and indexed right now.
    Loading,
    /// Indexed; it will be part of the volume.
    Ready,
    /// Could not be opened (the message says why); the other clips are still mounted.
    Failed(String),
}

/// Turns what the user picked into the clip files it contains, in the order they are mounted:
/// a file stays as it is, a folder becomes its `.MLV` / `.mcraw` / `.braw` files (searched
/// recursively, sorted by path). Used to list every clip before mounting starts, so the apps
/// can show the whole selection with a per-clip status while it is indexed.
pub fn expand_sources(sources: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for s in sources {
        if s.is_dir() {
            match crate::vfs::collect_mlv_files_in_dir(s) {
                Ok(mut found) => {
                    found.sort();
                    if found.is_empty() {
                        crate::log::write("WARN", &format!("no clips found in folder '{}'", s.display()));
                    }
                    files.extend(found);
                }
                Err(e) => crate::log::write("WARN", &format!("cannot read folder '{}': {e}", s.display())),
            }
        } else {
            files.push(s.clone());
        }
    }
    files
}

/// Mounts clip `files` (see [`expand_sources`]) into `vfs` one after another, calling
/// `on_progress(index, state)` as each file starts and finishes, so the apps can show live
/// per-clip progress. Files that fail are skipped (and logged); errors only if none could be opened.
pub fn mount_files_with_progress(
    vfs: &mut CineFs,
    files: &[std::path::PathBuf],
    mode: crate::DngPayloadMode,
    mut on_progress: impl FnMut(usize, ClipLoad),
) -> crate::Result<()> {
    let mut first_err = None;
    for (i, f) in files.iter().enumerate() {
        on_progress(i, ClipLoad::Loading);
        match vfs.mount_mlv(f, mode) {
            Ok(_) => on_progress(i, ClipLoad::Ready),
            Err(e) => {
                crate::log::write("WARN", &format!("skipping '{}': {e}", f.display()));
                on_progress(i, ClipLoad::Failed(e.to_string()));
                first_err.get_or_insert(e);
            }
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

    #[test]
    fn expand_sources_lists_folder_clips_in_order() {
        let dir = std::env::temp_dir().join(format!("cinefs_expand_{}", std::process::id()));
        let sub = dir.join("day2");
        std::fs::create_dir_all(&sub).unwrap();
        for p in [dir.join("B.MLV"), dir.join("a.mcraw"), dir.join("notes.txt"), sub.join("C.braw")] {
            std::fs::write(&p, b"x").unwrap();
        }
        let loose = std::path::PathBuf::from("/somewhere/loose.MLV");
        let files = expand_sources(&[loose.clone(), dir.clone()]);
        let mut expected = vec![dir.join("B.MLV"), dir.join("a.mcraw"), sub.join("C.braw")];
        expected.sort();
        expected.insert(0, loose);
        assert_eq!(files, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_reports_every_file_and_failures() {
        let mut vfs = CineFs::new(64 * 1024 * 1024);
        let files = vec![std::path::PathBuf::from("/does/not/exist.MLV")];
        let mut events = Vec::new();
        let res = mount_files_with_progress(&mut vfs, &files, crate::DngPayloadMode::Auto, |i, s| events.push((i, s)));
        assert!(res.is_err());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (0, ClipLoad::Loading));
        assert!(matches!(events[1], (0, ClipLoad::Failed(_))));
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
