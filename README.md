# CineFS

**CineFS** is a high-performance, zero-conversion virtual filesystem that mounts raw video containers (`.MLV`, `.mcraw` and, experimentally, `.braw`) as virtual **CinemaDNG** folders ready to open directly in DaVinci Resolve, Adobe Premiere Pro, and After Effects.

## Highlights

- **O(1) Frame & Metadata Indexing**: Single-pass container indexing at open time; no `.IDX` linear scanning or repeated file open/close overhead.
- **Header / Payload Separation**: Reading DNG TIFF/EXIF headers (for OS thumbnails or NLE media scanning) never triggers pixel decoding.
- **Zero-Decode LJ92 Passthrough**: Lossless JPEG (LJ92) frames in `.MLV` containers that match DNG's 2-component CFA predictor-1 layout are wrapped directly as `Compression = 7` DNG strips without decoding or re-encoding pixels.
- **Zero-Unpack Native Bit Depth**: Uncompressed 8/10/12/14/16-bit `.MLV` frames are served at their native bit depth without inflating to 16-bit.
- **Native MotionCam `.mcraw` Support**: Pure-Rust MotionCam container v3 parser and block-level bitplane decoder (`compressionType = 7` and legacy `6`), mapping dual-illuminant `ColorMatrix1/2`, `ForwardMatrix1/2`, `AsShotNeutral`, dynamic black/white levels, and audio packets directly into virtual CinemaDNG + BWF WAV.
- **Experimental Blackmagic RAW `.braw` Support**: Independent, pure-Rust reader for `.braw` clips that decodes frames to 12-bit Bayer and serves them as virtual CinemaDNG. Black/white levels and white balance are approximated, so colour may not match Blackmagic's own software. See [Trademarks & affiliation](#trademarks--affiliation).
- **Broadcast WAV (BWF) Audio**: `WAVI` + `AUDF` (and `.mcraw` `AUDIO_DATA`) blocks are presented as a virtual `.wav` file with embedded `bext` and `iXML` timecode chunks so DaVinci Resolve automatically syncs audio to the CinemaDNG sequence.
- **Zero-Extension macOS Mount**: Built-in user-space NFSv3 + MOUNTv3 server (`crates/cinefs-core/src/nfs.rs`) mounts virtual CinemaDNG volumes natively via `/sbin/mount_nfs` — no `macFUSE`, no kernel extensions (`kext`), no recovery mode, and no `sudo` required.
- **Zero-Dependency Windows WinFsp Adapter**: Built-in WinFsp C ABI dispatcher (`crates/cinefs-core/src/winfsp.rs`) dynamically loads `winfsp-x64.dll` / `winfsp-a64.dll` at runtime and mounts virtual CinemaDNG volumes to drive letters (`Z:`, `Y:`) or folders.
- **Adobe Premiere Pro & After Effects Compatible**: Full DNG 1.4 tag set (`DefaultCropSize`, `ActiveArea`, `DNGBackwardVersion`, `ForwardMatrix1/2`) plus a hybrid write overlay that persists Adobe Camera Raw `.xmp` sidecar settings next to the source clip while keeping `.pek`/`.DS_Store` temporary files in RAM.

## Workspace Layout

- `crates/cinefs-core` — Format parsers (`.MLV` + multi-part `.M00`.., MotionCam `.mcraw`), LJ92 inspector/decoder, bit unpacker/packer, DNG template engine, BWF WAV virtual stream, LRU cache with single-flight deduplication and read-ahead prefetching, user-space NFSv3 server, Windows WinFsp adapter, and C FFI bridge.
- `crates/cinefs-cli` — Cross-platform command-line utility (`mount`, `unmount`, `mount-bench`, `info`, `ls`, `export`, `bench`).
- `crates/cinefs-win` — Native Windows Desktop + System Tray application (`CineFS.exe`) with drag-and-drop, automatic drive-letter mounting, and one-click DaVinci Resolve `fuscript.exe` Media Pool integration.
- `macos/CineFS` — Native SwiftUI macOS Menu Bar + Desktop application (`CineFS.app`) linking `libcinefs_core.a` directly.
- `windows/` — Windows multi-resolution icon (`AppIcon.ico`), Inno Setup installer (`CineFS.iss`), and PowerShell build script (`build_windows.ps1`).

## Quick Start

### macOS App (`CineFS.app` & `CineFS.dmg`)
```bash
./macos/build_app.sh
open macos/build/CineFS.app
```

### Windows App & Installer (`CineFS.exe` & `CineFS-Setup.exe`)
```powershell
powershell -ExecutionPolicy Bypass -File .\windows\build_windows.ps1
```

### Command Line (`cinefs-cli`)
```bash
# Mount a single .MLV clip or an entire folder of .MLV clips as virtual CinemaDNG folders
cargo run --release -p cinefs-cli -- mount path/to/clips_dir ~/CineFS/MyShoot --open

# Benchmark live kernel NFSv3 mount playback throughput
cargo run --release -p cinefs-cli -- mount-bench path/to/clip.MLV

# Inspect an MLV file
cargo run --release -p cinefs-cli -- info path/to/clip.MLV

# Benchmark virtual DNG stat, header-only read, and full-frame read speed
cargo run --release -p cinefs-cli -- bench path/to/clip.MLV

# Export a clip to a real CinemaDNG folder on disk
cargo run --release -p cinefs-cli -- export path/to/clip.MLV ./out_dir
```

## License

CineFS is free and open source. Anyone may use, modify, redistribute and build on it, for personal
or commercial work, with no strings attached beyond keeping the copyright notice.

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

### Acknowledgements

CineFS is a bridge tool; the hard work of understanding Magic Lantern RAW video was done by others.
Thanks to the Magic Lantern team and community (the MLV format and `mlv_dump`), MLVFS, MLV App and
MotionCam, whose public documentation and behaviour guided this project. The experimental `.braw`
decoder relies on the bitstream tables and scan orders reverse-engineered by Paul B Mahol for
FFmpeg's (unmerged) `braw` decoder.

### Trademarks & affiliation

CineFS is an independent project. It is **not affiliated with, endorsed by, or sponsored by
Blackmagic Design Pty. Ltd.**, and it does not use or include the Blackmagic RAW SDK. Blackmagic RAW
and DaVinci Resolve are trademarks of Blackmagic Design. Adobe, Premiere Pro and After Effects are
trademarks of Adobe Inc. Other names are trademarks of their respective owners and are used only to
describe compatibility.

### Contributing: keep the licence clean

Raw corrections (focus pixels, bad pixels, stripe fixes, ...) are re-implemented from the published
behaviour of those tools. Please **do not copy source code or data files from GPL-licensed projects**
into this repository: doing so would force the whole project to become GPL and defeat the goal of it
being usable by anyone. Describe the algorithm, then write it independently.
