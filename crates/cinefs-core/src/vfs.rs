//! Virtual filesystem (`VirtualFs`) representation for CineFS.
//!
//! Exposes each loaded `.MLV` clip as a virtual CinemaDNG folder using the
//! Blackmagic / DaVinci Resolve naming convention (`<base>_1_<yyyy-MM-dd>_0001_C0000/`)
//! containing:
//! - `<folder>_000000.dng` .. `<folder>_NNNNNN.dng`
//! - `<folder>.wav` (Broadcast WAV with matching `bext`/`iXML` timecode, if audio is present)
//! - `info.json` (clip metadata sidecar)
//!
//! Also provides `O(1)` 64-bit inode (`node_id`) addressing for zero-lookup NFSv3 / FSKit / WinFsp
//! file handles and `O(1)` indexed directory pagination.

use crate::cache::{CacheKey, PayloadCache};
use crate::dng::DngTemplate;
use crate::mlv::MlvClip;
use crate::wav::VirtualWav;
use crate::{DngPayloadMode, Error, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Root directory inode (`/`).
pub const NODE_ROOT: u64 = 1;
/// Special macOS Spotlight marker file (`/.metadata_never_index`) to prevent background indexing storms.
pub const NODE_METADATA_NEVER_INDEX: u64 = 2;
/// Bit-63 flag indicating a mutable overlay sidecar file (`node_id = NODE_OVERLAY_FLAG | id`).
pub const NODE_OVERLAY_FLAG: u64 = 0x8000_0000_0000_0000;

const SUB_DIR: u64 = 0;
const SUB_INFO_JSON: u64 = 1;
const SUB_WAV: u64 = 2;
const SUB_FRAME_BASE: u64 = 100;
const MAX_OVERLAY_FILE_SIZE: usize = 64 * 1024 * 1024; // 64 MB safety cap per overlay file

/// Virtual filesystem entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
}

/// File/directory attributes returned by `stat` (always O(1), zero pixel work).
#[derive(Debug, Clone)]
pub struct VfsAttr {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub timestamp_unix_secs: u64,
}

/// Compact inode attribute struct for `O(1)` handle-based filesystem protocols (NFSv3, FSKit, WinFsp).
#[derive(Debug, Clone, Copy)]
pub struct VfsNodeAttr {
    pub node_id: u64,
    pub kind: EntryKind,
    pub size: u64,
    pub nlink: u32,
    pub timestamp_unix_secs: u64,
}

/// Virtual clip mounted inside `CineFs`.
pub struct MountedClip {
    pub clip_id: usize,
    pub folder_name: String,
    pub clip: MlvClip,
    pub dng_template: DngTemplate,
    pub wav: Option<VirtualWav>,
    pub info_json: Vec<u8>,
    pub base_timestamp_secs: u64,
    thumbnail: std::sync::OnceLock<Option<Arc<crate::thumbnail::Thumbnail>>>,
}

impl MountedClip {
    /// Lazily rendered, cached preview image (`None` if the frame could not be decoded).
    pub fn thumbnail(&self, max_width: usize) -> Option<Arc<crate::thumbnail::Thumbnail>> {
        self.thumbnail
            .get_or_init(|| match crate::thumbnail::render(&self.clip, max_width) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    crate::log::write("WARN", &format!("thumbnail for '{}' failed: {e}", self.folder_name));
                    None
                }
            })
            .clone()
    }

    pub fn open(path: impl AsRef<Path>, clip_id: usize, mode: DngPayloadMode) -> Result<Self> {
        Self::open_with_clip_number(path, clip_id, 0, mode)
    }

    pub fn open_with_clip_number(
        path: impl AsRef<Path>,
        clip_id: usize,
        clip_number: usize,
        mode: DngPayloadMode,
    ) -> Result<Self> {
        let clip = MlvClip::open(path)?;
        let folder_name = format!(
            "{}_1_{}_0001_C{:04}",
            clip.base_name,
            clip.rtci.date_ymd_string(),
            clip_number
        );
        let dng_template = DngTemplate::new(&clip, mode, folder_name.clone());
        let wav = VirtualWav::new(&clip, &folder_name);
        let info_json = build_info_json(&clip, &dng_template);
        let base_timestamp_secs = clip.rtci.unix_epoch_secs();
        Ok(Self {
            clip_id,
            folder_name,
            clip,
            dng_template,
            wav,
            info_json,
            base_timestamp_secs,
            thumbnail: std::sync::OnceLock::new(),
        })
    }

    #[inline]
    pub fn frame_timestamp_secs(&self, frame_index: usize) -> u64 {
        let first_us = self.clip.frames.first().map(|f| f.timestamp_us).unwrap_or(0);
        let cur_us = self
            .clip
            .frames
            .get(frame_index)
            .map(|f| f.timestamp_us)
            .unwrap_or(first_us);
        self.base_timestamp_secs + (cur_us.saturating_sub(first_us) / 1_000_000)
    }

    #[inline]
    pub fn frame_file_name(&self, frame_index: usize) -> String {
        format!("{}_{:06}.dng", self.folder_name, frame_index)
    }

    #[inline]
    pub fn wav_file_name(&self) -> String {
        format!("{}.wav", self.folder_name)
    }

    /// Reads `[offset .. offset + dst.len()]` from virtual `.dng` frame `frame_index`.
    ///
    /// - If the read lies entirely within the DNG header (`offset + len <= header_len`),
    ///   only the ~792-byte header is synthesized; zero pixel bytes are read or decoded!
    /// - If the clip uses `Lj92Passthrough` or 16-bit uncompressed pixels, payload slices
    ///   are served directly from the `.MLV` file via `pread` without buffering.
    /// - Otherwise, the transformed payload is fetched through `cache` with automatic
    ///   sequential read-ahead prefetching.
    pub fn read_dng_at(
        self: &Arc<Self>,
        frame_index: usize,
        offset: u64,
        dst: &mut [u8],
        cache: &Arc<PayloadCache>,
    ) -> Result<usize> {
        if frame_index >= self.clip.frames.len() {
            return Err(Error::NotFound(format!("Frame {frame_index} not found")));
        }
        let total_size = self.dng_template.file_size(&self.clip, frame_index);
        if offset >= total_size || dst.is_empty() {
            return Ok(0);
        }
        let max_len = (total_size - offset).min(dst.len() as u64) as usize;
        let dst = &mut dst[..max_len];

        let hdr_len = self.dng_template.header_len() as u64;
        let mut written = 0usize;

        // 1. Serve header portion if `offset < hdr_len`
        if offset < hdr_len {
            let hdr = self.dng_template.build_header(&self.clip, frame_index);
            let hdr_off = offset as usize;
            let take = (hdr.len() - hdr_off).min(dst.len());
            dst[..take].copy_from_slice(&hdr[hdr_off..hdr_off + take]);
            written += take;
        }

        // 2. Serve payload portion if needed
        if written < dst.len() {
            let payload_off = (offset + written as u64).saturating_sub(hdr_len);
            let rem_dst = &mut dst[written..];

            if self.dng_template.is_direct_payload_sliceable(&self.clip) {
                let n = self
                    .clip
                    .read_raw_frame_slice(frame_index, payload_off, rem_dst)?;
                written += n;
            } else if self.dng_template.is_byte_swapped_sliceable(&self.clip) {
                if (payload_off & 1) == 0 && (rem_dst.len() & 1) == 0 {
                    let n = self
                        .clip
                        .read_raw_frame_slice(frame_index, payload_off, rem_dst)?
                        & !1;
                    for pair in rem_dst[..n].chunks_exact_mut(2) {
                        pair.swap(0, 1);
                    }
                    written += n;
                } else {
                    let aligned_off = payload_off & !1;
                    let head_skip = (payload_off - aligned_off) as usize;
                    let aligned_len = (head_skip + rem_dst.len() + 1) & !1;
                    let mut tmp = vec![0u8; aligned_len];
                    let n = self
                        .clip
                        .read_raw_frame_slice(frame_index, aligned_off, &mut tmp)?
                        & !1;
                    for pair in tmp[..n].chunks_exact_mut(2) {
                        pair.swap(0, 1);
                    }
                    if n > head_skip {
                        let take = (n - head_skip).min(rem_dst.len());
                        rem_dst[..take].copy_from_slice(&tmp[head_skip..head_skip + take]);
                        written += take;
                    }
                }
            } else {
                let key = CacheKey {
                    clip_id: self.clip_id,
                    frame_index,
                };
                let payload = cache.get_or_try_compute(key, || {
                    self.dng_template.build_payload(&self.clip, frame_index)
                })?;

                // Trigger background read-ahead prefetching when reading frame start
                if payload_off == 0 {
                    let lookahead = 6;
                    let upcoming = cache.record_access_and_plan_prefetch(
                        self.clip_id,
                        frame_index,
                        self.clip.frames.len(),
                        lookahead,
                    );
                    for next_idx in upcoming {
                        let clip_arc = Arc::clone(self);
                        let cache_arc = Arc::clone(cache);
                        let pf_key = CacheKey {
                            clip_id: self.clip_id,
                            frame_index: next_idx,
                        };
                        cache.enqueue_prefetch(move || {
                            match clip_arc
                                .dng_template
                                .build_payload(&clip_arc.clip, next_idx)
                            {
                                Ok(buf) => cache_arc.insert(pf_key, Arc::new(buf), true),
                                Err(_) => cache_arc.cancel_in_flight(pf_key),
                            }
                        });
                    }
                }

                let p_off = payload_off as usize;
                if p_off < payload.len() {
                    let take = (payload.len() - p_off).min(rem_dst.len());
                    rem_dst[..take].copy_from_slice(&payload[p_off..p_off + take]);
                    written += take;
                }
            }
        }

        Ok(written)
    }
}

/// Mutable overlay file stored in RAM (with automatic `.xmp` sidecar persistence next to the source clip).
#[derive(Debug, Clone)]
pub struct OverlayFile {
    pub node_id: u64,
    pub parent_dir_id: u64,
    pub name: String,
    pub data: Vec<u8>,
    pub timestamp_unix_secs: u64,
}

#[derive(Default)]
struct OverlayStore {
    next_id: u64,
    by_id: HashMap<u64, OverlayFile>,
    by_dir_name: HashMap<(u64, String), u64>,
    dir_entries: HashMap<u64, Vec<u64>>,
}

impl OverlayStore {
    fn create_or_get(
        &mut self,
        parent_dir_id: u64,
        name: String,
        initial_data: Option<Vec<u8>>,
        truncate_if_exists: bool,
        ts: u64,
    ) -> VfsNodeAttr {
        let key = (parent_dir_id, name.clone());
        if let Some(&existing_id) = self.by_dir_name.get(&key) {
            if let Some(f) = self.by_id.get_mut(&existing_id) {
                if let Some(data) = initial_data {
                    f.data = data;
                    f.timestamp_unix_secs = ts;
                } else if truncate_if_exists {
                    f.data.clear();
                    f.timestamp_unix_secs = ts;
                }
                return VfsNodeAttr {
                    node_id: f.node_id,
                    kind: EntryKind::File,
                    size: f.data.len() as u64,
                    nlink: 1,
                    timestamp_unix_secs: f.timestamp_unix_secs,
                };
            }
        }

        self.next_id += 1;
        let node_id = NODE_OVERLAY_FLAG | self.next_id;
        let data = initial_data.unwrap_or_default();
        let size = data.len() as u64;
        let file = OverlayFile {
            node_id,
            parent_dir_id,
            name,
            data,
            timestamp_unix_secs: ts,
        };
        self.by_id.insert(node_id, file);
        self.by_dir_name.insert(key, node_id);
        self.dir_entries
            .entry(parent_dir_id)
            .or_default()
            .push(node_id);

        VfsNodeAttr {
            node_id,
            kind: EntryKind::File,
            size,
            nlink: 1,
            timestamp_unix_secs: ts,
        }
    }

    fn remove(&mut self, parent_dir_id: u64, name: &str) -> Option<OverlayFile> {
        let exact_key = (parent_dir_id, name.to_string());
        let node_id = if let Some(id) = self.by_dir_name.remove(&exact_key) {
            id
        } else {
            let found_key = self
                .by_dir_name
                .keys()
                .find(|(d, n)| *d == parent_dir_id && n.eq_ignore_ascii_case(name))
                .cloned()?;
            self.by_dir_name.remove(&found_key)?
        };
        if let Some(vec) = self.dir_entries.get_mut(&parent_dir_id) {
            vec.retain(|&id| id != node_id);
        }
        self.by_id.remove(&node_id)
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_700_000_000)
}

/// Top-level virtual filesystem holding one or more mounted clips.
pub struct CineFs {
    pub clips: Vec<Arc<MountedClip>>,
    pub cache: Arc<PayloadCache>,
    overlay: RwLock<OverlayStore>,
}

impl CineFs {
    pub fn new(cache_size_bytes: usize) -> Self {
        Self {
            clips: Vec::new(),
            cache: Arc::new(PayloadCache::new(cache_size_bytes)),
            overlay: RwLock::new(OverlayStore::default()),
        }
    }

    /// Mounts a single `.MLV`, `.mcraw` or `.braw` file, automatically picking a unique virtual folder name.
    pub fn mount_mlv(&mut self, path: impl AsRef<Path>, mode: DngPayloadMode) -> Result<usize> {
        let clip_id = self.clips.len();
        let mut clip_number = 0usize;
        let mut mounted = MountedClip::open_with_clip_number(&path, clip_id, clip_number, mode)?;
        while self
            .clips
            .iter()
            .any(|c| c.folder_name == mounted.folder_name)
        {
            clip_number += 1;
            mounted.folder_name = format!(
                "{}_1_{}_0001_C{:04}",
                mounted.clip.base_name,
                mounted.clip.rtci.date_ymd_string(),
                clip_number
            );
            mounted.dng_template =
                DngTemplate::new(&mounted.clip, mode, mounted.folder_name.clone());
            mounted.wav = VirtualWav::new(&mounted.clip, &mounted.folder_name);
        }

        // Pre-populate Adobe Camera Raw `.xmp` sidecar if one was previously saved next to the source clip
        if let Some(primary_path) = mounted.clip.chunk_paths.first() {
            let sidecar_path = primary_path.with_extension("xmp");
            if sidecar_path.is_file() {
                if let Ok(xmp_bytes) = fs::read(&sidecar_path) {
                    if !xmp_bytes.is_empty() && xmp_bytes.len() <= 4 * 1024 * 1024 {
                        let dir_node_id = Self::encode_node_id(clip_id, SUB_DIR);
                        let xmp_name = format!("{}_000000.xmp", mounted.folder_name);
                        if let Ok(mut ov) = self.overlay.write() {
                            ov.create_or_get(
                                dir_node_id,
                                xmp_name,
                                Some(xmp_bytes),
                                true,
                                mounted.base_timestamp_secs,
                            );
                        }
                    }
                }
            }
        }

        self.clips.push(Arc::new(mounted));
        Ok(clip_id)
    }

    /// Mounts either a single `.MLV`/`.mcraw`/`.braw` file or all such files inside a directory.
    pub fn mount_path(
        &mut self,
        path: impl AsRef<Path>,
        mode: DngPayloadMode,
    ) -> Result<Vec<usize>> {
        let path = path.as_ref();
        if path.is_dir() {
            let mut mlv_files = collect_mlv_files_in_dir(path)?;
            mlv_files.sort();
            if mlv_files.is_empty() {
                return Err(Error::NotFound(format!(
                    "No .MLV, .mcraw or .braw files found in directory: {}",
                    path.display()
                )));
            }
            let mut ids = Vec::with_capacity(mlv_files.len());
            for mlv in mlv_files {
                if let Ok(id) = self.mount_mlv(&mlv, mode) {
                    ids.push(id);
                }
            }
            if ids.is_empty() {
                return Err(Error::InvalidFormat(format!(
                    "Failed to open any valid .MLV, .mcraw or .braw files in {}",
                    path.display()
                )));
            }
            Ok(ids)
        } else {
            let id = self.mount_mlv(path, mode)?;
            Ok(vec![id])
        }
    }

    /// Total virtual size in bytes across all mounted clips and total virtual file count.
    pub fn total_volume_stats(&self) -> (u64, u64) {
        let mut total_bytes = 0u64;
        let mut total_files = 1u64; // .metadata_never_index
        for c in &self.clips {
            for i in 0..c.clip.frames.len() {
                total_bytes += c.dng_template.file_size(&c.clip, i);
                total_files += 1;
            }
            if let Some(wav) = &c.wav {
                total_bytes += wav.file_size();
                total_files += 1;
            }
            total_bytes += c.info_json.len() as u64;
            total_files += 1;
        }
        (total_bytes, total_files)
    }

    // =========================================================================
    // O(1) Inode (`node_id`) API for NFSv3 / FSKit / WinFsp
    // =========================================================================

    #[inline]
    pub fn is_overlay_node(node_id: u64) -> bool {
        (node_id & NODE_OVERLAY_FLAG) != 0
    }

    #[inline]
    pub fn encode_node_id(clip_id: usize, sub_id: u64) -> u64 {
        (((clip_id as u64) + 1) << 32) | sub_id
    }

    #[inline]
    pub fn decode_node_id(node_id: u64) -> Option<(usize, u64)> {
        if Self::is_overlay_node(node_id) {
            return None;
        }
        let high = node_id >> 32;
        if high == 0 {
            None
        } else {
            Some(((high as usize) - 1, node_id & 0xFFFF_FFFF))
        }
    }

    /// Returns attributes for `node_id` in `O(1)` time without any string parsing or pixel reads.
    pub fn stat_by_id(&self, node_id: u64) -> Result<VfsNodeAttr> {
        if Self::is_overlay_node(node_id) {
            let ov = self
                .overlay
                .read()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            let f = ov
                .by_id
                .get(&node_id)
                .ok_or_else(|| Error::NotFound(format!("Overlay node {node_id} not found")))?;
            return Ok(VfsNodeAttr {
                node_id: f.node_id,
                kind: EntryKind::File,
                size: f.data.len() as u64,
                nlink: 1,
                timestamp_unix_secs: f.timestamp_unix_secs,
            });
        }

        if node_id == NODE_ROOT {
            return Ok(VfsNodeAttr {
                node_id: NODE_ROOT,
                kind: EntryKind::Directory,
                size: 4096,
                nlink: (2 + self.clips.len()) as u32,
                timestamp_unix_secs: 1_700_000_000,
            });
        }
        if node_id == NODE_METADATA_NEVER_INDEX {
            return Ok(VfsNodeAttr {
                node_id: NODE_METADATA_NEVER_INDEX,
                kind: EntryKind::File,
                size: 0,
                nlink: 1,
                timestamp_unix_secs: 1_700_000_000,
            });
        }

        let (clip_id, sub_id) = Self::decode_node_id(node_id)
            .ok_or_else(|| Error::NotFound(format!("Invalid node_id {node_id}")))?;
        let clip = self
            .clips
            .get(clip_id)
            .ok_or_else(|| Error::NotFound(format!("Clip {clip_id} not found")))?;

        match sub_id {
            SUB_DIR => Ok(VfsNodeAttr {
                node_id,
                kind: EntryKind::Directory,
                size: 4096,
                nlink: 2,
                timestamp_unix_secs: clip.base_timestamp_secs,
            }),
            SUB_INFO_JSON => Ok(VfsNodeAttr {
                node_id,
                kind: EntryKind::File,
                size: clip.info_json.len() as u64,
                nlink: 1,
                timestamp_unix_secs: clip.base_timestamp_secs,
            }),
            SUB_WAV => {
                let wav = clip
                    .wav
                    .as_ref()
                    .ok_or_else(|| Error::NotFound(String::from("WAV not present")))?;
                Ok(VfsNodeAttr {
                    node_id,
                    kind: EntryKind::File,
                    size: wav.file_size(),
                    nlink: 1,
                    timestamp_unix_secs: clip.base_timestamp_secs,
                })
            }
            n if n >= SUB_FRAME_BASE => {
                let frame_idx = (n - SUB_FRAME_BASE) as usize;
                if frame_idx >= clip.clip.frames.len() {
                    return Err(Error::NotFound(format!("Frame {frame_idx} out of range")));
                }
                Ok(VfsNodeAttr {
                    node_id,
                    kind: EntryKind::File,
                    size: clip.dng_template.file_size(&clip.clip, frame_idx),
                    nlink: 1,
                    timestamp_unix_secs: clip.frame_timestamp_secs(frame_idx),
                })
            }
            _ => Err(Error::NotFound(format!("Unknown sub_id {sub_id}"))),
        }
    }

    /// Looks up a child entry by name inside directory `dir_node_id` in `O(1)` time.
    pub fn lookup_in_dir(&self, dir_node_id: u64, name: &str) -> Result<VfsNodeAttr> {
        if name == "." {
            return self.stat_by_id(dir_node_id);
        }
        if name == ".." {
            return self.stat_by_id(NODE_ROOT);
        }

        if dir_node_id == NODE_ROOT {
            if name == ".metadata_never_index" {
                return self.stat_by_id(NODE_METADATA_NEVER_INDEX);
            }
        if !name.starts_with('.') {
            if let Some(clip) = self
                .clips
                .iter()
                .find(|c| c.folder_name == name || c.folder_name.eq_ignore_ascii_case(name))
            {
                return self.stat_by_id(Self::encode_node_id(clip.clip_id, SUB_DIR));
            }
        }
        if let Ok(ov) = self.overlay.read() {
            if let Some(&ov_id) = ov.by_dir_name.get(&(NODE_ROOT, name.to_string())) {
                return self.stat_by_id(ov_id);
            }
            if let Some((&(_, _), &ov_id)) = ov
                .by_dir_name
                .iter()
                .find(|((d, n), _)| *d == NODE_ROOT && n.eq_ignore_ascii_case(name))
            {
                return self.stat_by_id(ov_id);
            }
        }
        return Err(Error::NotFound(format!("Entry not found in root: {name}")));
    }

    let (clip_id, sub_id) = Self::decode_node_id(dir_node_id)
        .ok_or_else(|| Error::NotFound(format!("Not a directory: {dir_node_id}")))?;
    if sub_id != SUB_DIR {
        return Err(Error::NotFound(format!("Not a directory: {dir_node_id}")));
    }
    let clip = self
        .clips
        .get(clip_id)
        .ok_or_else(|| Error::NotFound(format!("Clip {clip_id} not found")))?;

    if !name.starts_with('.') {
        if name.eq_ignore_ascii_case("info.json") {
            return self.stat_by_id(Self::encode_node_id(clip_id, SUB_INFO_JSON));
        }
        if clip.wav.is_some() && name.eq_ignore_ascii_case(&clip.wav_file_name()) {
            return self.stat_by_id(Self::encode_node_id(clip_id, SUB_WAV));
        }
        if let Some(frame_idx) = parse_frame_index(&clip.folder_name, name) {
            if frame_idx < clip.clip.frames.len() {
                return self.stat_by_id(Self::encode_node_id(
                    clip_id,
                    SUB_FRAME_BASE + frame_idx as u64,
                ));
            }
        }
    }

    if let Ok(ov) = self.overlay.read() {
        if let Some(&ov_id) = ov.by_dir_name.get(&(dir_node_id, name.to_string())) {
            return self.stat_by_id(ov_id);
        }
        if let Some((&(_, _), &ov_id)) = ov
            .by_dir_name
            .iter()
            .find(|((d, n), _)| *d == dir_node_id && n.eq_ignore_ascii_case(name))
        {
            return self.stat_by_id(ov_id);
        }
    }

    Err(Error::NotFound(format!(
        "Entry '{name}' not found in clip {}",
        clip.folder_name
    )))
}

    /// Returns the `index`-th directory entry (`0`-based, including `"."` and `".."`) for `dir_node_id` in `O(1)` time.
    pub fn dir_entry_at(
        &self,
        dir_node_id: u64,
        index: usize,
    ) -> Result<Option<(String, VfsNodeAttr)>> {
        if dir_node_id == NODE_ROOT {
            return match index {
                0 => Ok(Some((String::from("."), self.stat_by_id(NODE_ROOT)?))),
                1 => Ok(Some((String::from(".."), self.stat_by_id(NODE_ROOT)?))),
                2 => Ok(Some((
                    String::from(".metadata_never_index"),
                    self.stat_by_id(NODE_METADATA_NEVER_INDEX)?,
                ))),
                idx => {
                    let clip_idx = idx - 3;
                    if let Some(clip) = self.clips.get(clip_idx) {
                        let attr = self.stat_by_id(Self::encode_node_id(clip.clip_id, SUB_DIR))?;
                        Ok(Some((clip.folder_name.clone(), attr)))
                    } else {
                        let ov_idx = clip_idx - self.clips.len();
                        self.overlay_dir_entry_at(NODE_ROOT, ov_idx)
                    }
                }
            };
        }

        let (clip_id, sub_id) = Self::decode_node_id(dir_node_id)
            .ok_or_else(|| Error::NotFound(format!("Not a directory: {dir_node_id}")))?;
        if sub_id != SUB_DIR {
            return Err(Error::NotFound(format!("Not a directory: {dir_node_id}")));
        }
        let clip = self
            .clips
            .get(clip_id)
            .ok_or_else(|| Error::NotFound(format!("Clip {clip_id} not found")))?;

        match index {
            0 => Ok(Some((String::from("."), self.stat_by_id(dir_node_id)?))),
            1 => Ok(Some((String::from(".."), self.stat_by_id(NODE_ROOT)?))),
            2 => Ok(Some((
                String::from("info.json"),
                self.stat_by_id(Self::encode_node_id(clip_id, SUB_INFO_JSON))?,
            ))),
            idx => {
                let has_wav = clip.wav.is_some();
                if has_wav && idx == 3 {
                    let attr = self.stat_by_id(Self::encode_node_id(clip_id, SUB_WAV))?;
                    return Ok(Some((clip.wav_file_name(), attr)));
                }
                let frame_idx = if has_wav { idx - 4 } else { idx - 3 };
                if frame_idx < clip.clip.frames.len() {
                    let attr = self.stat_by_id(Self::encode_node_id(
                        clip_id,
                        SUB_FRAME_BASE + frame_idx as u64,
                    ))?;
                    Ok(Some((clip.frame_file_name(frame_idx), attr)))
                } else {
                    let ov_idx = frame_idx - clip.clip.frames.len();
                    self.overlay_dir_entry_at(dir_node_id, ov_idx)
                }
            }
        }
    }

    fn overlay_dir_entry_at(
        &self,
        dir_node_id: u64,
        ov_idx: usize,
    ) -> Result<Option<(String, VfsNodeAttr)>> {
        let Ok(ov) = self.overlay.read() else {
            return Ok(None);
        };
        let Some(entries) = ov.dir_entries.get(&dir_node_id) else {
            return Ok(None);
        };
        let Some(&node_id) = entries.get(ov_idx) else {
            return Ok(None);
        };
        let Some(f) = ov.by_id.get(&node_id) else {
            return Ok(None);
        };
        Ok(Some((
            f.name.clone(),
            VfsNodeAttr {
                node_id: f.node_id,
                kind: EntryKind::File,
                size: f.data.len() as u64,
                nlink: 1,
                timestamp_unix_secs: f.timestamp_unix_secs,
            },
        )))
    }

    /// Reads `[offset .. offset + dst.len()]` from `node_id` in `O(1)` time, returning `(bytes_read, file_attr)`.
    pub fn read_by_id(
        &self,
        node_id: u64,
        offset: u64,
        dst: &mut [u8],
    ) -> Result<(usize, VfsNodeAttr)> {
        if Self::is_overlay_node(node_id) {
            let ov = self
                .overlay
                .read()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            let f = ov
                .by_id
                .get(&node_id)
                .ok_or_else(|| Error::NotFound(format!("Overlay node {node_id} not found")))?;
            let attr = VfsNodeAttr {
                node_id: f.node_id,
                kind: EntryKind::File,
                size: f.data.len() as u64,
                nlink: 1,
                timestamp_unix_secs: f.timestamp_unix_secs,
            };
            if offset >= f.data.len() as u64 || dst.is_empty() {
                return Ok((0, attr));
            }
            let off = offset as usize;
            let take = (f.data.len() - off).min(dst.len());
            dst[..take].copy_from_slice(&f.data[off..off + take]);
            return Ok((take, attr));
        }

        let attr = self.stat_by_id(node_id)?;
        if node_id == NODE_METADATA_NEVER_INDEX {
            return Ok((0, attr));
        }
        let (clip_id, sub_id) = Self::decode_node_id(node_id)
            .ok_or_else(|| Error::NotFound(format!("Not a readable file node: {node_id}")))?;
        let clip = self
            .clips
            .get(clip_id)
            .ok_or_else(|| Error::NotFound(format!("Clip {clip_id} not found")))?;

        let bytes_read = match sub_id {
            SUB_INFO_JSON => {
                if offset >= clip.info_json.len() as u64 || dst.is_empty() {
                    0
                } else {
                    let off = offset as usize;
                    let take = (clip.info_json.len() - off).min(dst.len());
                    dst[..take].copy_from_slice(&clip.info_json[off..off + take]);
                    take
                }
            }
            SUB_WAV => {
                let wav = clip
                    .wav
                    .as_ref()
                    .ok_or_else(|| Error::NotFound(String::from("WAV not present")))?;
                wav.read_at(&clip.clip, offset, dst)?
            }
            n if n >= SUB_FRAME_BASE => {
                let frame_idx = (n - SUB_FRAME_BASE) as usize;
                clip.read_dng_at(frame_idx, offset, dst, &self.cache)?
            }
            _ => return Err(Error::NotFound(format!("Cannot read directory {node_id}"))),
        };

        Ok((bytes_read, attr))
    }

    // =========================================================================
    // Hybrid Write Overlay API (for Adobe Camera Raw `.xmp`, `.pek`, Finder)
    // =========================================================================

    /// Resolves the next `dir_entry_at` index immediately following `marker` in `O(1)` time for `.dng` frames.
    pub fn resolve_dir_marker_index(&self, dir_node_id: u64, marker: &str, min_index: usize) -> usize {
        if marker.is_empty() {
            return min_index;
        }
        if dir_node_id != NODE_ROOT {
            if marker == "." {
                return 1.max(min_index);
            }
            if marker == ".." {
                return 2.max(min_index);
            }
            if let Some((clip_id, SUB_DIR)) = Self::decode_node_id(dir_node_id) {
                if let Some(clip) = self.clips.get(clip_id) {
                    if marker.eq_ignore_ascii_case("info.json") {
                        return 3.max(min_index);
                    }
                    let has_wav = clip.wav.is_some();
                    if has_wav && marker.eq_ignore_ascii_case(&clip.wav_file_name()) {
                        return 4.max(min_index);
                    }
                    if let Some(frame_idx) = parse_frame_index(&clip.folder_name, marker) {
                        if frame_idx < clip.clip.frames.len() {
                            let base = if has_wav { 4 } else { 3 };
                            return (base + frame_idx + 1).max(min_index);
                        }
                    }
                }
            }
        }
        let mut idx = min_index;
        while let Ok(Some((entry_name, _))) = self.dir_entry_at(dir_node_id, idx) {
            if entry_name == marker || entry_name.eq_ignore_ascii_case(marker) {
                return idx + 1;
            }
            idx += 1;
        }
        idx
    }

    fn is_builtin_entry_in_dir(&self, dir_node_id: u64, name: &str) -> bool {
        if name == "." || name == ".." {
            return true;
        }
        if dir_node_id == NODE_ROOT {
            if name.eq_ignore_ascii_case(".metadata_never_index") {
                return true;
            }
            return self
                .clips
                .iter()
                .any(|c| c.folder_name.eq_ignore_ascii_case(name));
        }
        let Some((clip_id, sub_id)) = Self::decode_node_id(dir_node_id) else {
            return false;
        };
        if sub_id != SUB_DIR {
            return false;
        };
        let Some(clip) = self.clips.get(clip_id) else {
            return false;
        };
        if name.eq_ignore_ascii_case("info.json") {
            return true;
        }
        if clip.wav.is_some() && name.eq_ignore_ascii_case(&clip.wav_file_name()) {
            return true;
        }
        if let Some(frame_idx) = parse_frame_index(&clip.folder_name, name) {
            if frame_idx < clip.clip.frames.len() {
                return true;
            }
        }
        false
    }

    fn sync_sidecar_if_xmp(&self, dir_node_id: u64, name: &str, data: Option<&[u8]>) {
        if name.starts_with('.') {
            return;
        }
        let is_xmp = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xmp"))
            .unwrap_or(false);
        if !is_xmp {
            return;
        }
        let Some((clip_id, sub_id)) = Self::decode_node_id(dir_node_id) else {
            return;
        };
        if sub_id != SUB_DIR {
            return;
        }
        let Some(clip) = self.clips.get(clip_id) else {
            return;
        };
        let Some(primary_path) = clip.clip.chunk_paths.first() else {
            return;
        };
        let sidecar_path = primary_path.with_extension("xmp");
        match data {
            Some(bytes) if !bytes.is_empty() => {
                let _ = fs::write(&sidecar_path, bytes);
            }
            None => {
                let _ = fs::remove_file(&sidecar_path);
            }
            _ => {}
        }
    }

    /// Creates (or truncates) a mutable overlay file in `dir_node_id`.
    /// Built-in `.dng`, `.wav`, and `info.json` files remain protected and cannot be overwritten.
    pub fn create_in_dir(
        &self,
        dir_node_id: u64,
        name: &str,
        truncate_if_exists: bool,
    ) -> Result<VfsNodeAttr> {
        let dir_attr = self.stat_by_id(dir_node_id)?;
        if dir_attr.kind != EntryKind::Directory {
            return Err(Error::InvalidFormat(format!(
                "Node {dir_node_id} is not a directory"
            )));
        }
        if name.is_empty() || self.is_builtin_entry_in_dir(dir_node_id, name) {
            return Err(Error::InvalidFormat(format!(
                "Cannot overwrite built-in virtual entry '{name}'"
            )));
        }
        let mut ov = self
            .overlay
            .write()
            .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
        let attr = ov.create_or_get(
            dir_node_id,
            name.to_string(),
            None,
            truncate_if_exists,
            now_unix_secs(),
        );
        Ok(attr)
    }

    /// Writes `data` at `offset` into an overlay file node.
    pub fn write_by_id(
        &self,
        node_id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(usize, VfsNodeAttr)> {
        if !Self::is_overlay_node(node_id) {
            return Err(Error::InvalidFormat(String::from(
                "Cannot write to immutable virtual file",
            )));
        }
        let end = (offset as usize).saturating_add(data.len());
        if end > MAX_OVERLAY_FILE_SIZE {
            return Err(Error::InvalidFormat(String::from(
                "Overlay file exceeds 64 MB limit",
            )));
        }
        let (parent_dir_id, name, snapshot, attr) = {
            let mut ov = self
                .overlay
                .write()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            let f = ov
                .by_id
                .get_mut(&node_id)
                .ok_or_else(|| Error::NotFound(format!("Overlay node {node_id} not found")))?;
            if end > f.data.len() {
                f.data.resize(end, 0);
            }
            let off = offset as usize;
            f.data[off..end].copy_from_slice(data);
            f.timestamp_unix_secs = now_unix_secs();
            let attr = VfsNodeAttr {
                node_id: f.node_id,
                kind: EntryKind::File,
                size: f.data.len() as u64,
                nlink: 1,
                timestamp_unix_secs: f.timestamp_unix_secs,
            };
            (f.parent_dir_id, f.name.clone(), f.data.clone(), attr)
        };
        self.sync_sidecar_if_xmp(parent_dir_id, &name, Some(&snapshot));
        Ok((data.len(), attr))
    }

    /// Truncates or resizes an overlay file (used by `NFSPROC3_SETATTR`).
    pub fn truncate_by_id(&self, node_id: u64, new_size: u64) -> Result<VfsNodeAttr> {
        if !Self::is_overlay_node(node_id) {
            return self.stat_by_id(node_id);
        }
        let sz = (new_size as usize).min(MAX_OVERLAY_FILE_SIZE);
        let (parent_dir_id, name, snapshot, attr) = {
            let mut ov = self
                .overlay
                .write()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            let f = ov
                .by_id
                .get_mut(&node_id)
                .ok_or_else(|| Error::NotFound(format!("Overlay node {node_id} not found")))?;
            f.data.resize(sz, 0);
            f.timestamp_unix_secs = now_unix_secs();
            let attr = VfsNodeAttr {
                node_id: f.node_id,
                kind: EntryKind::File,
                size: f.data.len() as u64,
                nlink: 1,
                timestamp_unix_secs: f.timestamp_unix_secs,
            };
            (f.parent_dir_id, f.name.clone(), f.data.clone(), attr)
        };
        if !snapshot.is_empty() {
            self.sync_sidecar_if_xmp(parent_dir_id, &name, Some(&snapshot));
        }
        Ok(attr)
    }

    /// Removes a mutable overlay file from `dir_node_id`.
    pub fn remove_in_dir(&self, dir_node_id: u64, name: &str) -> Result<()> {
        if self.is_builtin_entry_in_dir(dir_node_id, name) {
            return Err(Error::InvalidFormat(format!(
                "Cannot remove built-in virtual file '{name}'"
            )));
        }
        let removed = {
            let mut ov = self
                .overlay
                .write()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            ov.remove(dir_node_id, name)
        };
        if removed.is_some() {
            self.sync_sidecar_if_xmp(dir_node_id, name, None);
            Ok(())
        } else {
            Err(Error::NotFound(format!("Overlay file '{name}' not found")))
        }
    }

    /// Renames a mutable overlay file (supports atomic save `CREATE temp -> WRITE -> RENAME to .xmp`).
    pub fn rename_in_dir(
        &self,
        from_dir_id: u64,
        from_name: &str,
        to_dir_id: u64,
        to_name: &str,
    ) -> Result<()> {
        if self.is_builtin_entry_in_dir(from_dir_id, from_name)
            || self.is_builtin_entry_in_dir(to_dir_id, to_name)
        {
            return Err(Error::InvalidFormat(String::from(
                "Cannot rename built-in virtual files",
            )));
        }
        let to_dir_attr = self.stat_by_id(to_dir_id)?;
        if to_dir_attr.kind != EntryKind::Directory {
            return Err(Error::InvalidFormat(String::from(
                "Target is not a directory",
            )));
        }
        let snapshot = {
            let mut ov = self
                .overlay
                .write()
                .map_err(|_| Error::InvalidFormat(String::from("Overlay lock poisoned")))?;
            let Some(mut file) = ov.remove(from_dir_id, from_name) else {
                return Err(Error::NotFound(format!(
                    "Overlay file '{from_name}' not found"
                )));
            };
            let _ = ov.remove(to_dir_id, to_name);
            file.parent_dir_id = to_dir_id;
            file.name = to_name.to_string();
            file.timestamp_unix_secs = now_unix_secs();
            let data_clone = file.data.clone();
            let node_id = file.node_id;
            ov.by_id.insert(node_id, file);
            ov.by_dir_name
                .insert((to_dir_id, to_name.to_string()), node_id);
            ov.dir_entries.entry(to_dir_id).or_default().push(node_id);
            data_clone
        };
        self.sync_sidecar_if_xmp(to_dir_id, to_name, Some(&snapshot));
        Ok(())
    }

    // =========================================================================
    // Path-based API
    // =========================================================================

    /// Lists entries in a virtual directory (`""` or `"/"` for root, or `"<folder_name>"`).
    pub fn list(&self, dir_path: &str) -> Result<Vec<VfsAttr>> {
        let trimmed = dir_path.trim_matches('/');
        if trimmed.is_empty() {
            let mut out: Vec<VfsAttr> = self
                .clips
                .iter()
                .map(|c| VfsAttr {
                    name: c.folder_name.clone(),
                    kind: EntryKind::Directory,
                    size: 0,
                    timestamp_unix_secs: c.base_timestamp_secs,
                })
                .collect();
            if let Ok(ov) = self.overlay.read() {
                if let Some(ids) = ov.dir_entries.get(&NODE_ROOT) {
                    for &id in ids {
                        if let Some(f) = ov.by_id.get(&id) {
                            out.push(VfsAttr {
                                name: f.name.clone(),
                                kind: EntryKind::File,
                                size: f.data.len() as u64,
                                timestamp_unix_secs: f.timestamp_unix_secs,
                            });
                        }
                    }
                }
            }
            return Ok(out);
        }

        let clip = self
            .clips
            .iter()
            .find(|c| c.folder_name == trimmed)
            .ok_or_else(|| Error::NotFound(format!("Directory not found: {dir_path}")))?;

        let mut entries = Vec::with_capacity(clip.clip.frames.len() + 3);
        for i in 0..clip.clip.frames.len() {
            entries.push(VfsAttr {
                name: clip.frame_file_name(i),
                kind: EntryKind::File,
                size: clip.dng_template.file_size(&clip.clip, i),
                timestamp_unix_secs: clip.clip.frames[i].timestamp_us / 1_000_000,
            });
        }
        if let Some(wav) = &clip.wav {
            entries.push(VfsAttr {
                name: clip.wav_file_name(),
                kind: EntryKind::File,
                size: wav.file_size(),
                timestamp_unix_secs: clip.base_timestamp_secs,
            });
        }
        entries.push(VfsAttr {
            name: String::from("info.json"),
            kind: EntryKind::File,
            size: clip.info_json.len() as u64,
            timestamp_unix_secs: clip.base_timestamp_secs,
        });

        let dir_node_id = Self::encode_node_id(clip.clip_id, SUB_DIR);
        if let Ok(ov) = self.overlay.read() {
            if let Some(ids) = ov.dir_entries.get(&dir_node_id) {
                for &id in ids {
                    if let Some(f) = ov.by_id.get(&id) {
                        entries.push(VfsAttr {
                            name: f.name.clone(),
                            kind: EntryKind::File,
                            size: f.data.len() as u64,
                            timestamp_unix_secs: f.timestamp_unix_secs,
                        });
                    }
                }
            }
        }

        Ok(entries)
    }

    /// Returns attributes for any virtual path in `O(1)` without reading pixel data.
    pub fn stat(&self, path: &str) -> Result<VfsAttr> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(VfsAttr {
                name: String::from("/"),
                kind: EntryKind::Directory,
                size: 0,
                timestamp_unix_secs: 0,
            });
        }

        let mut parts = trimmed.splitn(2, '/');
        let folder = parts.next().unwrap();
        let clip = self
            .clips
            .iter()
            .find(|c| c.folder_name == folder)
            .ok_or_else(|| Error::NotFound(format!("Clip folder not found: {folder}")))?;

        let Some(file_name) = parts.next() else {
            return Ok(VfsAttr {
                name: clip.folder_name.clone(),
                kind: EntryKind::Directory,
                size: 0,
                timestamp_unix_secs: clip.base_timestamp_secs,
            });
        };

        if file_name == "info.json" {
            return Ok(VfsAttr {
                name: String::from("info.json"),
                kind: EntryKind::File,
                size: clip.info_json.len() as u64,
                timestamp_unix_secs: clip.base_timestamp_secs,
            });
        }

        if let Some(wav) = &clip.wav {
            if file_name == clip.wav_file_name() {
                return Ok(VfsAttr {
                    name: file_name.to_string(),
                    kind: EntryKind::File,
                    size: wav.file_size(),
                    timestamp_unix_secs: clip.base_timestamp_secs,
                });
            }
        }

        if let Some(frame_idx) = parse_frame_index(&clip.folder_name, file_name) {
            if frame_idx < clip.clip.frames.len() {
                return Ok(VfsAttr {
                    name: file_name.to_string(),
                    kind: EntryKind::File,
                    size: clip.dng_template.file_size(&clip.clip, frame_idx),
                    timestamp_unix_secs: clip.clip.frames[frame_idx].timestamp_us / 1_000_000,
                });
            }
        }

        let dir_node_id = Self::encode_node_id(clip.clip_id, SUB_DIR);
        if let Ok(ov) = self.overlay.read() {
            if let Some(&ov_id) = ov.by_dir_name.get(&(dir_node_id, file_name.to_string())) {
                if let Some(f) = ov.by_id.get(&ov_id) {
                    return Ok(VfsAttr {
                        name: f.name.clone(),
                        kind: EntryKind::File,
                        size: f.data.len() as u64,
                        timestamp_unix_secs: f.timestamp_unix_secs,
                    });
                }
            }
        }

        Err(Error::NotFound(format!("Virtual file not found: {path}")))
    }

    /// Reads `[offset .. offset + dst.len()]` from a virtual file path.
    pub fn read(&self, path: &str, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let trimmed = path.trim_matches('/');
        let (folder, file_name) = trimmed
            .split_once('/')
            .ok_or_else(|| Error::NotFound(format!("Not a file path: {path}")))?;

        let clip = self
            .clips
            .iter()
            .find(|c| c.folder_name == folder)
            .ok_or_else(|| Error::NotFound(format!("Clip folder not found: {folder}")))?;

        if file_name == "info.json" {
            if offset >= clip.info_json.len() as u64 || dst.is_empty() {
                return Ok(0);
            }
            let off = offset as usize;
            let take = (clip.info_json.len() - off).min(dst.len());
            dst[..take].copy_from_slice(&clip.info_json[off..off + take]);
            return Ok(take);
        }

        if let Some(wav) = &clip.wav {
            if file_name == clip.wav_file_name() {
                return wav.read_at(&clip.clip, offset, dst);
            }
        }

        if let Some(frame_idx) = parse_frame_index(&clip.folder_name, file_name) {
            return clip.read_dng_at(frame_idx, offset, dst, &self.cache);
        }

        let dir_node_id = Self::encode_node_id(clip.clip_id, SUB_DIR);
        if let Ok(ov) = self.overlay.read() {
            if let Some(&ov_id) = ov.by_dir_name.get(&(dir_node_id, file_name.to_string())) {
                drop(ov);
                let (n, _) = self.read_by_id(ov_id, offset, dst)?;
                return Ok(n);
            }
        }

        Err(Error::NotFound(format!("Virtual file not found: {path}")))
    }
}

fn collect_mlv_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() {
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("mlv")
                    || ext.eq_ignore_ascii_case("mcraw")
                    || ext.eq_ignore_ascii_case("braw")
                {
                    out.push(p);
                }
            }
        } else if p.is_dir() {
            if let Ok(sub) = collect_mlv_files_in_dir(&p) {
                out.extend(sub);
            }
        }
    }
    Ok(out)
}

fn parse_frame_index(folder_name: &str, file_name: &str) -> Option<usize> {
    if let Some(prefix) = file_name.strip_prefix(folder_name) {
        let rest = prefix.strip_prefix('_')?;
        let digits = rest
            .strip_suffix(".dng")
            .or_else(|| rest.strip_suffix(".DNG"))?;
        return digits.parse::<usize>().ok();
    }
    if file_name.len() > folder_name.len() + 5
        && file_name[..folder_name.len()].eq_ignore_ascii_case(folder_name)
        && file_name.as_bytes()[folder_name.len()] == b'_'
    {
        let rest = &file_name[folder_name.len() + 1..];
        if rest.len() > 4 && rest[rest.len() - 4..].eq_ignore_ascii_case(".dng") {
            let digits = &rest[..rest.len() - 4];
            return digits.parse::<usize>().ok();
        }
    }
    None
}

fn build_info_json(clip: &MlvClip, dng_template: &DngTemplate) -> Vec<u8> {
    let meta0 = &clip.dynamic_metas[0];
    let shutter_sec = (meta0.shutter_us as f64) / 1_000_000.0;
    let rotation_code = (clip.rotation_degrees.rem_euclid(360)) / 90;
    let (has_audio, sample_rate, channels, audio_frames) = match clip.audio_info {
        Some(a) => {
            let frame_bytes = (a.channels as u64) * ((a.bits_per_sample.max(16) / 8) as u64);
            let frames = if frame_bytes > 0 {
                a.total_pcm_bytes / frame_bytes
            } else {
                0
            };
            (true, a.sample_rate, a.channels, frames)
        }
        None => (false, 0, 0, 0),
    };

    format!(
        "{{\"frameCount\":{},\"fps\":{:.6},\"width\":{},\"height\":{},\"bitsPerPixel\":{},\
         \"rotationCode\":{},\"hasAudio\":{},\"audioSampleRate\":{},\"audioChannels\":{},\
         \"audioFrameCount\":{},\"isoValue\":{},\"shutterSeconds\":{:.8}}}\n",
        clip.frames.len(),
        clip.fps_f64(),
        dng_template.out_width,
        dng_template.out_height,
        dng_template.dng_bits_per_sample,
        rotation_code,
        has_audio,
        sample_rate,
        channels,
        audio_frames,
        meta0.iso_value,
        shutter_sec
    )
    .into_bytes()
}
