use cinefs_core::nfs::MountedVolume;
use cinefs_core::vfs::CineFs;
use cinefs_core::DngPayloadMode;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Picks the highest available Windows drive letter from `Z:` down to `M:` given a 26-bit `GetLogicalDrives()` bitmask.
pub fn pick_free_drive_letter(logical_drives_mask: u32, already_used: &[String]) -> Option<String> {
    for letter in (b'M'..=b'Z').rev() {
        let bit = 1u32 << (letter - b'A');
        if (logical_drives_mask & bit) == 0 {
            let candidate = format!("{}:", letter as char);
            let candidate_slash = format!("{}:\\", letter as char);
            if !already_used.iter().any(|u| {
                u.eq_ignore_ascii_case(&candidate) || u.eq_ignore_ascii_case(&candidate_slash)
            }) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Formats a byte count into a human-readable string (`"4.28 GB"`, `"812.4 MB"`).
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Per-clip metadata summary for UI rendering (shared with macOS: `cinefs_core::summary`).
pub use cinefs_core::summary::ClipSummary as WinClipSummary;

static NEXT_MOUNT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Active mounted item managed by the Windows app.
pub struct WinMountEntry {
    /// Unique for the lifetime of the process (never reused), e.g. to key per-clip UI caches.
    pub id: u64,
    pub source_path: PathBuf,
    pub source_paths: Vec<PathBuf>,
    pub source_name: String,
    pub mount_point: PathBuf,
    pub drive_badge: String,
    pub vfs: Arc<CineFs>,
    pub volume: MountedVolume,
    pub clips: Vec<WinClipSummary>,
    pub clip_folders: Vec<String>,
    pub total_bytes: u64,
    pub index_time_ms: f64,
    pub display_label: String,
}

impl WinMountEntry {
    /// Indexes the clip `files` (the user's selection `source_paths`, expanded with
    /// `cinefs_core::summary::expand_sources`) into ONE virtual volume and mounts it at
    /// `mount_point`. `on_progress(file index, state)` is called as each clip is indexed.
    pub fn mount(
        source_paths: Vec<PathBuf>,
        files: &[PathBuf],
        mount_point: PathBuf,
        mode: DngPayloadMode,
        on_progress: impl FnMut(usize, cinefs_core::summary::ClipLoad),
    ) -> cinefs_core::Result<Self> {
        let t0 = Instant::now();
        let mut vfs = CineFs::new(512 * 1024 * 1024);
        // All sources share ONE virtual volume (one drive letter / one folder).
        cinefs_core::summary::mount_files_with_progress(&mut vfs, files, mode, on_progress)?;
        let source_path = source_paths[0].clone();
        let index_time_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let (total_bytes, _) = vfs.total_volume_stats();
        let clip_folders: Vec<String> = vfs.clips.iter().map(|c| c.folder_name.clone()).collect();
        let clips: Vec<WinClipSummary> = vfs
            .clips
            .iter()
            .map(|c| {
                let mut s = WinClipSummary::from_mounted(c);
                // The Windows UI shows the DNG bit depth that is actually written.
                s.bits_per_pixel = c.runtime().dng_template.dng_bits_per_sample as u32;
                s
            })
            .collect();

        let first = &vfs.clips[0];
        let first_width = first.clip.width;
        let first_height = first.clip.height;
        let first_bpp = first.clip.bits_per_pixel;
        let first_rt = first.runtime();
        let first_mode_desc = first_rt.dng_template.mode.description();
        let total_frames: usize = vfs.clips.iter().map(|c| c.clip.frames.len()).sum();
        let source_name = if source_paths.len() > 1 {
            format!("{} files", source_paths.len())
        } else {
            source_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| source_path.display().to_string())
        };

        let vfs_arc = Arc::new(vfs);
        let volume = MountedVolume::mount(Arc::clone(&vfs_arc), &mount_point)?;
        let actual_mp = volume.mount_point.clone();
        let mp_str = actual_mp.to_string_lossy();
        let drive_badge = if mp_str.len() <= 3 && mp_str.as_bytes().get(1) == Some(&b':') {
            format!("{}:\\", mp_str.chars().next().unwrap().to_ascii_uppercase())
        } else {
            String::from("DIR")
        };

        let display_label = format!(
            "[{}] {}  ({} clip{}, {}x{} {}-bit, {} frames, {})",
            actual_mp.display(),
            source_name,
            clips.len(),
            if clips.len() == 1 { "" } else { "s" },
            first_width,
            first_height,
            first_bpp,
            total_frames,
            first_mode_desc
        );

        Ok(Self {
            id: NEXT_MOUNT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            source_path,
            source_paths,
            source_name,
            mount_point: actual_mp,
            drive_badge,
            vfs: vfs_arc,
            volume,
            clips,
            clip_folders,
            total_bytes,
            index_time_ms,
            display_label,
        })
    }

    /// Returns the clean Windows root path suitable for `ShellExecuteW` / `explorer.exe` (always with trailing `\` for drive letters).
    pub fn explorer_root_path(&self) -> PathBuf {
        let s = self.mount_point.to_string_lossy();
        if s.len() == 2 && s.as_bytes()[1] == b':' {
            PathBuf::from(format!("{s}\\"))
        } else {
            self.mount_point.clone()
        }
    }

    pub fn full_clip_paths(&self) -> Vec<PathBuf> {
        let root = self.explorer_root_path();
        self.clip_folders.iter().map(|f| root.join(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_free_drive_letter_prefers_z_downward() {
        // Mask with C: (bit 2) and Z: (bit 25) occupied
        let mask = (1u32 << 2) | (1u32 << 25);
        let picked = pick_free_drive_letter(mask, &[]).unwrap();
        assert_eq!(picked, "Y:");

        let picked2 = pick_free_drive_letter(mask, &[String::from("Y:\\")]).unwrap();
        assert_eq!(picked2, "X:");
    }
}
