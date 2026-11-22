mod png;

use cinefs_core::focus_probe::{self, ProbeAccum, ProbeConfig};
use cinefs_core::nfs::{unmount_path, MountedVolume};
use cinefs_core::vfs::{CineFs, MountedClip};
use cinefs_core::{DngPayloadMode, Result};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

#[cfg(unix)]
extern "C" fn handle_signal(_sig: i32) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn SetConsoleCtrlHandler(
        HandlerRoutine: Option<unsafe extern "system" fn(u32) -> i32>,
        Add: i32,
    ) -> i32;
}

#[cfg(windows)]
unsafe extern "system" fn handle_win_ctrl(_ctrl_type: u32) -> i32 {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    1
}

fn install_signal_handlers() {
    #[cfg(unix)]
    unsafe {
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        signal(SIGINT, handle_signal);
        signal(SIGTERM, handle_signal);
    }
    #[cfg(windows)]
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handle_win_ctrl), 1);
    }
}

fn print_usage() {
    eprintln!(
        "CineFS CLI v{}\n\
         Usage:\n  \
           cinefs-cli info <mlv_mcraw_or_braw_path>\n  \
           cinefs-cli ls <clip_or_dir>\n  \
           cinefs-cli bench <mlv_mcraw_or_braw_path> [--mode auto|passthrough|packed|unpacked16]\n  \
           cinefs-cli export <mlv_mcraw_or_braw_path> <out_dir> [--mode auto|passthrough|packed|unpacked16] [--frames N]\n  \
           cinefs-cli mount <clip_or_dir> <mount_point> [--mode auto|passthrough|packed|unpacked16] [--cache-mb N] [--open]\n  \
           cinefs-cli mount-bench <mlv_mcraw_or_braw_path> [--mode auto|passthrough|packed|unpacked16]\n  \
           cinefs-cli focus-probe <mlv_mcraw_or_braw_path> <out_dir> [--frames N]\n  \
           cinefs-cli unmount <mount_point>",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_mode(args: &[String]) -> DngPayloadMode {
    for i in 0..args.len() {
        if args[i] == "--mode" && i + 1 < args.len() {
            return match args[i + 1].as_str() {
                "passthrough" => DngPayloadMode::Lj92Passthrough,
                "packed" => DngPayloadMode::NativePacked,
                "unpacked16" | "16" => DngPayloadMode::Unpacked16,
                _ => DngPayloadMode::Auto,
            };
        }
    }
    DngPayloadMode::Auto
}

fn parse_frame_limit(args: &[String]) -> Option<usize> {
    for i in 0..args.len() {
        if args[i] == "--frames" && i + 1 < args.len() {
            return args[i + 1].parse::<usize>().ok();
        }
    }
    None
}

fn parse_cache_mb(args: &[String]) -> usize {
    for i in 0..args.len() {
        if args[i] == "--cache-mb" && i + 1 < args.len() {
            if let Ok(mb) = args[i + 1].parse::<usize>() {
                return mb;
            }
        }
    }
    512
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn cmd_info(mlv_path: &Path, mode: DngPayloadMode) -> Result<()> {
    let t0 = Instant::now();
    let mounted = MountedClip::open(mlv_path, 0, mode)?;
    let open_elapsed = t0.elapsed();
    let clip = &mounted.clip;
    let meta0 = &clip.dynamic_metas[0];

    println!("=== CineFS Clip Summary ===");
    println!("Source File       : {}", mlv_path.display());
    println!("Chunks Discovered : {}", clip.chunk_paths.len());
    println!(
        "Index Time        : {:.2} ms",
        open_elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "MLV Version       : {} (GUID: 0x{:016X})",
        clip.version_string, clip.video_guid
    );
    println!(
        "Camera Model      : {} (ID: 0x{:08X})",
        clip.camera_name, clip.camera_model_id
    );
    println!(
        "Resolution        : {}x{} ({}-bit, CFA: {}, Rotation: {}°)",
        clip.width,
        clip.height,
        clip.bits_per_pixel,
        clip.cfa_pattern.name(),
        clip.rotation_degrees
    );
    println!(
        "Levels            : Black = {}, White = {} (file) -> Black = {}, White = {} [{}] {}",
        clip.black_level, clip.white_level, clip.levels.black, clip.levels.white, clip.levels.source, clip.levels.note
    );
    println!(
        "Frame Rate        : {:.3} fps ({}/{}) | Frames: {}",
        clip.fps_f64(),
        clip.fps_nom,
        clip.fps_denom,
        clip.frames.len()
    );
    println!(
        "Compression       : video_class=0x{:04X} (LJ92={})",
        clip.video_class, clip.is_lj92
    );
    if let Some(lj) = clip.lj92_info {
        println!(
            "LJ92 Stream       : {}-bit, SOF3={}x{}x{} comps, predictor={}, DNG passthrough={}",
            lj.bits,
            lj.sof_width,
            lj.sof_height,
            lj.components,
            lj.predictor,
            clip.is_lj92_passthrough_compatible()
        );
    }
    println!(
        "Exposure / WB     : ISO {}, Shutter {} us, WB {:.3} / {:.3} / {:.3} ({}K)",
        meta0.iso_value,
        meta0.shutter_us,
        meta0.wb_gain_r,
        meta0.wb_gain_g,
        meta0.wb_gain_b,
        meta0.wb_kelvin
    );
    println!(
        "Lens              : '{}' ({} mm, f/{:.2})",
        meta0.lens_name,
        meta0.focal_length_mm,
        (meta0.aperture_x100 as f32) / 100.0
    );
    println!(
        "ColorMatrix1      : Illuminant={}, Source={} [{:.4}, {:.4}, {:.4}, ...]",
        clip.calibration_illuminant1,
        clip.color_matrix1_source,
        clip.color_matrix1[0],
        clip.color_matrix1[1],
        clip.color_matrix1[2]
    );
    if let (Some(illum2), Some(cm2)) = (clip.calibration_illuminant2, clip.color_matrix2) {
        let cm2_src = if clip.is_mcraw {
            "MCRAW colorMatrix2"
        } else {
            "CKCM"
        };
        println!(
            "ColorMatrix2      : Illuminant={}, Source={} [{:.4}, {:.4}, {:.4}, ...]",
            illum2, cm2_src, cm2[0], cm2[1], cm2[2]
        );
    }
    if let Some(fm1) = clip.forward_matrix1 {
        println!(
            "ForwardMatrix1    : [{:.4}, {:.4}, {:.4}, ...]",
            fm1[0], fm1[1], fm1[2]
        );
    }
    println!(
        "RTCI Timestamp    : {} (Start TC: {}s since midnight)",
        clip.rtci.tiff_datetime_string(),
        clip.rtci.seconds_since_midnight()
    );
    if let Some(aud) = clip.audio_info {
        println!(
            "Audio Track       : {} Hz, {} ch, {}-bit PCM ({} AUDF chunks, {:.2} KB)",
            aud.sample_rate,
            aud.channels,
            aud.bits_per_sample,
            clip.audio_entries.len(),
            (aud.total_pcm_bytes as f64) / 1024.0
        );
    } else {
        println!("Audio Track       : None");
    }
    let rt = mounted.runtime();
    println!("Block Counts      : {:?}", clip.block_counts);
    println!("--- Virtual CinemaDNG Layout ---");
    println!("Virtual Folder    : /{}/", mounted.folder_name);
    println!(
        "DNG Payload Mode  : {}",
        rt.dng_template.mode.description()
    );
    println!(
        "DNG Header Size   : {} bytes (StripOffsets = {})",
        rt.dng_template.header_len(),
        rt.dng_template.header_len()
    );
    println!(
        "Frame 0 DNG Size  : {} bytes ({:.2} MB)",
        rt.dng_template.file_size(clip, 0),
        (rt.dng_template.file_size(clip, 0) as f64) / (1024.0 * 1024.0)
    );
    Ok(())
}

fn cmd_ls(path: &Path, mode: DngPayloadMode) -> Result<()> {
    let mut vfs = CineFs::new(64 * 1024 * 1024);
    vfs.mount_path(path, mode)?;
    let root_entries = vfs.list("/")?;
    for dir in root_entries {
        println!("d {:>12}  /{}/", 0, dir.name);
        let children = vfs.list(&dir.name)?;
        let preview_count = children.len().min(8);
        for item in &children[..preview_count] {
            println!("f {:>12}  /{}/{}", item.size, dir.name, item.name);
        }
        if children.len() > preview_count {
            println!("  ... ({} more entries) ...", children.len() - preview_count);
            for item in &children[children.len().saturating_sub(3)..] {
                println!("f {:>12}  /{}/{}", item.size, dir.name, item.name);
            }
        }
    }
    Ok(())
}

fn cmd_bench(mlv_path: &Path, mode: DngPayloadMode) -> Result<()> {
    let t_open = Instant::now();
    let mounted = Arc::new(MountedClip::open(mlv_path, 0, mode)?);
    let open_ms = t_open.elapsed().as_secs_f64() * 1000.0;
    let frame_count = mounted.clip.frames.len();

    println!(
        "Benchmarking '{}' ({}x{}, {}-bit, {} frames)",
        mlv_path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        mounted.clip.width,
        mounted.clip.height,
        mounted.clip.bits_per_pixel,
        frame_count
    );
    let rt = mounted.runtime();
    println!(
        "Active Mode       : {}",
        rt.dng_template.mode.description()
    );
    println!("Container Index   : {:.2} ms", open_ms);

    // 1. Benchmark stat() across all frames (1000 iterations)
    let stat_iters = 1000usize;
    let t_stat = Instant::now();
    let mut total_size_acc = 0u64;
    for _ in 0..stat_iters {
        for i in 0..frame_count {
            total_size_acc =
                total_size_acc.wrapping_add(rt.dng_template.file_size(&mounted.clip, i));
        }
    }
    let stat_elapsed = t_stat.elapsed().as_secs_f64();
    let total_stats = (stat_iters * frame_count) as f64;
    println!(
        "1. VFS stat()     : {:.2} million ops/s ({:.1} ns/frame, checksum={})",
        (total_stats / stat_elapsed) / 1e6,
        (stat_elapsed * 1e9) / total_stats,
        total_size_acc & 0xFFFF
    );

    // 2. Benchmark Header-Only read (4 KB read at offset 0 — simulates Finder/Explorer/Resolve scan)
    let hdr_iters = 100usize;
    let t_hdr = Instant::now();
    let mut hdr_bytes_acc = 0usize;
    for _ in 0..hdr_iters {
        for i in 0..frame_count {
            let hdr = rt.dng_template.build_header(&mounted.clip, i);
            hdr_bytes_acc += hdr.len();
        }
    }
    let hdr_elapsed = t_hdr.elapsed().as_secs_f64();
    let total_hdrs = (hdr_iters * frame_count) as f64;
    println!(
        "2. Header-only    : {:.0} frames/s ({:.2} us/frame, {} bytes/hdr)",
        total_hdrs / hdr_elapsed,
        (hdr_elapsed * 1e6) / total_hdrs,
        hdr_bytes_acc / (hdr_iters * frame_count)
    );

    // 3. Benchmark Full-Frame DNG synthesis (Single-threaded)
    let t_single = Instant::now();
    let mut total_bytes_single = 0u64;
    for i in 0..frame_count {
        let hdr = rt.dng_template.build_header(&mounted.clip, i);
        let payload = rt.dng_template.build_payload(&mounted.clip, i)?;
        total_bytes_single += (hdr.len() + payload.len()) as u64;
    }
    let single_elapsed = t_single.elapsed().as_secs_f64();
    let single_fps = (frame_count as f64) / single_elapsed;
    let single_gbps = (total_bytes_single as f64) / single_elapsed / (1024.0 * 1024.0 * 1024.0);
    println!(
        "3. Full DNG (1T)  : {:.1} fps ({:.2} GB/s, {:.2} ms/frame)",
        single_fps,
        single_gbps,
        (single_elapsed * 1000.0) / (frame_count as f64)
    );

    // 4. Benchmark Full-Frame DNG synthesis (Multi-threaded across available cores)
    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let next_frame = Arc::new(AtomicUsize::new(0));
    let t_mt = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let m = Arc::clone(&mounted);
        let counter = Arc::clone(&next_frame);
        handles.push(thread::spawn(move || -> Result<u64> {
            let rt = m.runtime();
            let mut bytes = 0u64;
            loop {
                let idx = counter.fetch_add(1, Ordering::Relaxed);
                if idx >= frame_count {
                    break;
                }
                let hdr = rt.dng_template.build_header(&m.clip, idx);
                let payload = rt.dng_template.build_payload(&m.clip, idx)?;
                bytes += (hdr.len() + payload.len()) as u64;
            }
            Ok(bytes)
        }));
    }
    let mut total_bytes_mt = 0u64;
    for h in handles {
        total_bytes_mt += h.join().unwrap()?;
    }
    let mt_elapsed = t_mt.elapsed().as_secs_f64();
    let mt_fps = (frame_count as f64) / mt_elapsed;
    let mt_gbps = (total_bytes_mt as f64) / mt_elapsed / (1024.0 * 1024.0 * 1024.0);
    println!(
        "4. Full DNG ({}T)  : {:.1} fps ({:.2} GB/s, {:.2} ms/frame)",
        threads,
        mt_fps,
        mt_gbps,
        (mt_elapsed * 1000.0) / (frame_count as f64)
    );

    Ok(())
}

fn cmd_export(
    mlv_path: &Path,
    out_root: &Path,
    mode: DngPayloadMode,
    max_frames: Option<usize>,
) -> Result<()> {
    let t0 = Instant::now();
    let mounted = Arc::new(MountedClip::open(mlv_path, 0, mode)?);
    let clip_dir = out_root.join(&mounted.folder_name);
    fs::create_dir_all(&clip_dir)?;

    let total_frames = mounted
        .clip
        .frames
        .len()
        .min(max_frames.unwrap_or(usize::MAX));

    println!(
        "Exporting {} frames to {} ({})...",
        total_frames,
        clip_dir.display(),
        mounted.runtime().dng_template.mode.description()
    );

    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let next_frame = Arc::new(AtomicUsize::new(0));
    let clip_dir_arc = Arc::new(clip_dir.clone());
    let mut handles = Vec::with_capacity(threads);

    for _ in 0..threads {
        let m = Arc::clone(&mounted);
        let counter = Arc::clone(&next_frame);
        let dir = Arc::clone(&clip_dir_arc);
        handles.push(thread::spawn(move || -> Result<u64> {
            let rt = m.runtime();
            let mut written_bytes = 0u64;
            loop {
                let idx = counter.fetch_add(1, Ordering::Relaxed);
                if idx >= total_frames {
                    break;
                }
                let file_path = dir.join(m.frame_file_name(idx));
                let hdr = rt.dng_template.build_header(&m.clip, idx);
                let payload = rt.dng_template.build_payload(&m.clip, idx)?;
                let mut f = File::create(file_path)?;
                f.write_all(&hdr)?;
                f.write_all(&payload)?;
                written_bytes += (hdr.len() + payload.len()) as u64;
            }
            Ok(written_bytes)
        }));
    }

    let mut total_bytes = 0u64;
    for h in handles {
        total_bytes += h.join().unwrap()?;
    }

    if let Some(wav) = &mounted.wav {
        let wav_path = clip_dir.join(mounted.wav_file_name());
        let mut wf = File::create(wav_path)?;
        let mut buf = vec![0u8; wav.file_size() as usize];
        let n = wav.read_at(&mounted.clip, 0, &mut buf)?;
        wf.write_all(&buf[..n])?;
        total_bytes += n as u64;
    }

    let info_path = clip_dir.join("info.json");
    fs::write(info_path, &mounted.runtime().info_json)?;

    let elapsed = t0.elapsed().as_secs_f64();
    println!(
        "Done in {:.2}s ({:.1} fps, {:.2} MB written).",
        elapsed,
        (total_frames as f64) / elapsed,
        (total_bytes as f64) / (1024.0 * 1024.0)
    );
    Ok(())
}

fn cmd_mount(
    source_path: &Path,
    mount_point: &Path,
    mode: DngPayloadMode,
    cache_mb: usize,
    open_finder: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let mut vfs = CineFs::new(cache_mb * 1024 * 1024);
    let ids = vfs.mount_path(source_path, mode)?;
    let index_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let (total_bytes, total_files) = vfs.total_volume_stats();
    println!(
        "=== CineFS Virtual Mount ({} clip{}, {:.1} ms index time) ===",
        ids.len(),
        if ids.len() == 1 { "" } else { "s" },
        index_ms
    );
    for c in &vfs.clips {
        println!(
            "  + /{}/  ({}x{}, {}-bit, {} frames, {})",
            c.folder_name,
            c.clip.width,
            c.clip.height,
            c.clip.bits_per_pixel,
            c.clip.frames.len(),
            c.runtime().dng_template.mode.description()
        );
    }
    println!(
        "Virtual Volume    : {} files ({:.2} GB total, {} MB LRU cache + read-ahead prefetch)",
        total_files,
        (total_bytes as f64) / (1024.0 * 1024.0 * 1024.0),
        cache_mb
    );

    let vfs_arc = Arc::new(vfs);
    let mut mounted_vol = MountedVolume::mount(Arc::clone(&vfs_arc), mount_point)?;
    if mounted_vol.port() > 0 {
        println!(
            "Mounted at        : {} (NFSv3 127.0.0.1:{})",
            mounted_vol.mount_point.display(),
            mounted_vol.port()
        );
    } else {
        println!(
            "Mounted at        : {} (WinFsp Kernel Driver)",
            mounted_vol.mount_point.display()
        );
    }

    if open_finder {
        #[cfg(target_os = "macos")]
        let _ = Command::new("/usr/bin/open")
            .arg(&mounted_vol.mount_point)
            .status();
        #[cfg(windows)]
        let _ = Command::new("explorer.exe")
            .arg(&mounted_vol.mount_point)
            .status();
    }

    install_signal_handlers();
    println!("Press Ctrl+C to unmount and exit...");

    while !STOP_REQUESTED.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }

    let (hits, misses, prefetched, cached_bytes) = vfs_arc.cache.stats();
    println!(
        "\nUnmounting {} (cache hits={}, misses={}, prefetched={}, buffered={:.1} MB)...",
        mounted_vol.mount_point.display(),
        hits,
        misses,
        prefetched,
        (cached_bytes as f64) / (1024.0 * 1024.0)
    );
    mounted_vol.unmount()?;
    println!("Cleanly unmounted.");
    Ok(())
}

fn cmd_mount_bench(mlv_path: &Path, mode: DngPayloadMode) -> Result<()> {
    use std::io::Read;
    let mut vfs = CineFs::new(512 * 1024 * 1024);
    vfs.mount_mlv(mlv_path, mode)?;
    let clip_folder = vfs.clips[0].folder_name.clone();
    let frame_count = vfs.clips[0].clip.frames.len();
    let mode_desc = vfs.clips[0].runtime().dng_template.mode.description().to_string();

    let tmp_mp = std::env::temp_dir().join(format!("cinefs_bench_{}", std::process::id()));
    let vfs_arc = Arc::new(vfs);
    let mut vol = MountedVolume::mount(Arc::clone(&vfs_arc), &tmp_mp)?;
    let mounted_clip_dir = vol.mount_point.join(&clip_folder);

    println!(
        "=== Live macOS NFSv3 Mount Benchmark ({} frames, {}) ===",
        frame_count, mode_desc
    );

    // 1. Directory listing + stat all entries through kernel NFSv3 client
    let t_ls = Instant::now();
    let mut dng_paths: Vec<PathBuf> = fs::read_dir(&mounted_clip_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("dng"))
        .collect();
    dng_paths.sort();
    for p in &dng_paths {
        let _ = fs::metadata(p)?;
    }
    let ls_ms = t_ls.elapsed().as_secs_f64() * 1000.0;
    println!(
        "1. Kernel NFS readdir+stat ({} DNGs): {:.2} ms",
        dng_paths.len(),
        ls_ms
    );

    // 2. Header-only 4 KB reads across all mounted DNGs (simulating Resolve media pool scan)
    let t_hdr = Instant::now();
    let mut hdr_buf = [0u8; 4096];
    for p in &dng_paths {
        let mut f = File::open(p)?;
        let _ = f.read(&mut hdr_buf)?;
    }
    let hdr_ms = t_hdr.elapsed().as_secs_f64() * 1000.0;
    println!(
        "2. Kernel NFS 4KB header scan       : {:.2} ms ({:.0} frames/s)",
        hdr_ms,
        (dng_paths.len() as f64) / (hdr_ms / 1000.0)
    );

    // 3. Full sequential playback read of all mounted DNG files through POSIX File::read_to_end
    let t_seq = Instant::now();
    let mut total_bytes = 0u64;
    let mut buf = Vec::with_capacity(16 * 1024 * 1024);
    for p in &dng_paths {
        buf.clear();
        let mut f = File::open(p)?;
        total_bytes += f.read_to_end(&mut buf)? as u64;
    }
    let seq_s = t_seq.elapsed().as_secs_f64();
    println!(
        "3. Kernel NFS sequential playback   : {:.1} fps ({:.2} GB/s over loopback NFSv3)",
        (dng_paths.len() as f64) / seq_s,
        (total_bytes as f64) / seq_s / (1024.0 * 1024.0 * 1024.0)
    );

    let (hits, misses, prefetched, _) = vfs_arc.cache.stats();
    println!(
        "   Cache stats                      : hits={}, misses={}, prefetched={}",
        hits, misses, prefetched
    );

    vol.unmount()?;
    let _ = fs::remove_dir(&tmp_mp);
    Ok(())
}

/// Diagnostic: finds focus-pixel (PDAF) candidates from the data itself and writes
/// `persistence.png`, `zoom.png`, `candidates.csv` and `report.txt` to `out_dir`.
fn cmd_focus_probe(path: &Path, out_dir: &Path, max_frames: Option<usize>) -> Result<()> {
    let clip = cinefs_core::mlv::MlvClip::open(path)?;
    fs::create_dir_all(out_dir)?;
    let (w, h) = (clip.width as usize, clip.height as usize);
    let n = clip.frames.len();
    let want = max_frames.unwrap_or(32).clamp(2, 128).min(n);
    let picks: Vec<usize> = if want <= 1 { vec![0] } else { (0..want).map(|i| i * (n - 1) / (want - 1)).collect() };
    let cfg = ProbeConfig::for_bits(clip.bits_per_pixel, clip.levels.black as f32);
    println!(
        "{}: {w}x{h}, {} bit, black {}, camera '{}', probing {} of {n} frames",
        path.display(), clip.bits_per_pixel, clip.levels.black, clip.camera_name, picks.len()
    );

    let t0 = Instant::now();
    let mut acc = ProbeAccum::new(w, h);
    for idx in &picks {
        let px = focus_probe::decode_frame_u16(&clip, *idx)?;
        acc.add_frame(&px, cfg);
    }
    println!("analysed {} frames in {:.1} s", acc.frames, t0.elapsed().as_secs_f64());

    let cands = acc.candidates(0.8);
    let report = focus_probe::lattice_report(&cands);
    println!("{report}");
    fs::write(out_dir.join("report.txt"), &report)?;

    let mut csv = String::from("x,y,persistence,kind\n");
    for c in cands.iter().take(300_000) {
        csv.push_str(&format!("{},{},{:.3},{}\n", c.x, c.y, c.persistence, if c.dark { "dark" } else { "bright" }));
    }
    fs::write(out_dir.join("candidates.csv"), csv)?;

    // Persistence map: red = darker than neighbours, blue = brighter; only clearly persistent pixels.
    let colour = |x: usize, y: usize| -> [u8; 3] {
        let (p, dark) = acc.persistence_at(x, y);
        if p < 0.5 {
            return [0, 0, 0];
        }
        let v = (80.0 + 175.0 * ((p - 0.5) / 0.5)) as u8;
        if dark { [v, 0, 0] } else { [0, 0, v] }
    };
    let mut full = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            full.extend_from_slice(&colour(x, y));
        }
    }
    fs::write(out_dir.join("persistence.png"), png::encode_rgb(w, h, &full))?;

    // 4x zoom on the densest 64x64 cell of candidates.
    if !cands.is_empty() {
        let (zw, zh, k) = (256usize.min(w), 160usize.min(h), 4usize);
        let mut cells = std::collections::HashMap::<(u32, u32), usize>::new();
        for c in &cands {
            *cells.entry((c.x / 64, c.y / 64)).or_default() += 1;
        }
        let (&(cx, cy), _) = cells.iter().max_by_key(|(_, v)| **v).unwrap();
        let x0 = ((cx as usize * 64 + 32).saturating_sub(zw / 2)).min(w - zw) & !1;
        let y0 = ((cy as usize * 64 + 32).saturating_sub(zh / 2)).min(h - zh) & !1;
        let mut zoom = Vec::with_capacity(zw * k * zh * k * 3);
        for y in 0..zh * k {
            for x in 0..zw * k {
                zoom.extend_from_slice(&colour(x0 + x / k, y0 + y / k));
            }
        }
        fs::write(out_dir.join("zoom.png"), png::encode_rgb(zw * k, zh * k, &zoom))?;
        println!("zoom.png shows x {x0}..{} y {y0}..{} at {k}x", x0 + zw, y0 + zh);
    }
    println!("wrote persistence.png, zoom.png, candidates.csv, report.txt to {}", out_dir.display());
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        print_usage();
        std::process::exit(1);
    }

    let cmd = args[1].as_str();
    let path_arg = PathBuf::from(&args[2]);
    let mode = parse_mode(&args[3..]);

    let res = match cmd {
        "info" => cmd_info(&path_arg, mode),
        "ls" => cmd_ls(&path_arg, mode),
        "bench" => cmd_bench(&path_arg, mode),
        "export" => {
            if args.len() < 4 {
                print_usage();
                std::process::exit(1);
            }
            let out_dir = PathBuf::from(&args[3]);
            let max_frames = parse_frame_limit(&args[4..]);
            cmd_export(&path_arg, &out_dir, mode, max_frames)
        }
        "mount" => {
            if args.len() < 4 {
                print_usage();
                std::process::exit(1);
            }
            let mount_point = PathBuf::from(&args[3]);
            let cache_mb = parse_cache_mb(&args[4..]);
            let open_finder = has_flag(&args[4..], "--open");
            cmd_mount(&path_arg, &mount_point, mode, cache_mb, open_finder)
        }
        "focus-probe" => {
            if args.len() < 4 {
                print_usage();
                std::process::exit(1);
            }
            cmd_focus_probe(&path_arg, &PathBuf::from(&args[3]), parse_frame_limit(&args[4..]))
        }
        "mount-bench" => cmd_mount_bench(&path_arg, mode),
        "unmount" => unmount_path(&path_arg, true),
        _ => {
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(e) = res {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
