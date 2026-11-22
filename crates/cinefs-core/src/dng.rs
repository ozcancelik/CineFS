//! Zero-overhead CinemaDNG 1.4 TIFF/EXIF template engine and frame synthesizer.
//!
//! Each clip pre-builds a ~760-byte `DngTemplate` once on open. Generating the header
//! for frame `N` is a sub-microsecond slice clone + in-place patch of `StripByteCounts`
//! and `TimeCodes` (plus dynamic ISO/shutter/WB if they changed mid-take).

use crate::lj92::decode_lj92_to_u16_le;
use crate::mlv::MlvClip;
use crate::resample::stretch_bayer_u16;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::unpack::{
    dng_strip_byte_count, mlv_packed_to_dng_packed, pack_u16_le_to_dng_packed,
    unpack_mlv_to_u16_le,
};
use crate::{DngPayloadMode, Error, Result};

const TYPE_BYTE: u16 = 1;
const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;
const TYPE_SRATIONAL: u16 = 10;

static STRETCH_BAKE: AtomicBool = AtomicBool::new(true);

/// Anamorphic stretch method for clips opened afterwards: `true` = bake the stretch into the DNG
/// pixels (universal, ~3x data for 3x1 modes, no LJ92 passthrough); `false` = only write the
/// `DefaultScale` tag (fast, but applications may ignore it).
pub fn set_stretch_bake(on: bool) {
    STRETCH_BAKE.store(on, Ordering::Relaxed);
}

pub fn stretch_bake() -> bool {
    STRETCH_BAKE.load(Ordering::Relaxed)
}

/// Raw-domain resampling plan for clips whose pixels are anamorphic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakePlan {
    pub src_w: usize,
    pub src_h: usize,
    pub out_w: usize,
    pub out_h: usize,
}

impl BakePlan {
    /// `ratio` = pixel height / pixel width (`MlvClip::stretch_y`): `< 1` stretches horizontally,
    /// `> 1` vertically. Stretched dimension is rounded to a multiple of 4 (CFA + packing safe).
    pub fn for_clip(src_w: usize, src_h: usize, ratio: f64) -> Option<Self> {
        if !ratio.is_finite() || ratio <= 0.0 || (ratio - 1.0).abs() < 0.01 {
            return None;
        }
        let round4 = |v: f64| ((v / 4.0).round() as usize * 4).max(4);
        if ratio < 1.0 {
            Some(Self { src_w, src_h, out_w: round4(src_w as f64 / ratio), out_h: src_h })
        } else {
            Some(Self { src_w, src_h, out_w: src_w, out_h: round4(src_h as f64 * ratio) })
        }
    }
}

/// Concrete payload representation chosen for a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPayloadMode {
    /// Source LJ92 stream is passed straight through as DNG `Compression = 7` (zero pixel CPU work).
    Lj92Passthrough,
    /// Uncompressed DNG (`Compression = 1`) at native bit depth (`8, 10, 12, 14, 16`).
    NativePacked,
    /// Uncompressed 16-bit DNG (`Compression = 1`, `BitsPerSample = 16`).
    Unpacked16,
}

impl ResolvedPayloadMode {
    pub fn resolve(requested: DngPayloadMode, clip: &MlvClip) -> Self {
        match requested {
            DngPayloadMode::Auto => {
                if clip.is_lj92 && clip.is_lj92_passthrough_compatible() {
                    ResolvedPayloadMode::Lj92Passthrough
                } else if clip.is_mcraw {
                    ResolvedPayloadMode::Unpacked16
                } else {
                    ResolvedPayloadMode::NativePacked
                }
            }
            DngPayloadMode::Lj92Passthrough => {
                if clip.is_lj92 && clip.is_lj92_passthrough_compatible() {
                    ResolvedPayloadMode::Lj92Passthrough
                } else if clip.is_mcraw {
                    ResolvedPayloadMode::Unpacked16
                } else {
                    ResolvedPayloadMode::NativePacked
                }
            }
            DngPayloadMode::NativePacked => ResolvedPayloadMode::NativePacked,
            DngPayloadMode::Unpacked16 => ResolvedPayloadMode::Unpacked16,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ResolvedPayloadMode::Lj92Passthrough => "LJ92 Passthrough (Compression=7, zero decode)",
            ResolvedPayloadMode::NativePacked => "Native Bit-Depth Packed (Compression=1)",
            ResolvedPayloadMode::Unpacked16 => "Unpacked 16-bit Little-Endian (Compression=1)",
        }
    }
}

/// Pre-computed DNG header template for a clip.
#[derive(Debug, Clone)]
pub struct DngTemplate {
    pub mode: ResolvedPayloadMode,
    /// `Some` when the anamorphic stretch is baked into the pixels (square-pixel DNG output).
    pub bake: Option<BakePlan>,
    /// `Some` when focus pixels are corrected in the raw data (disables LJ92 passthrough).
    pub fix: Option<std::sync::Arc<crate::focus_fix::FocusFix>>,
    /// `Some` when Dual ISO recovery is active (outputs 16-bit linear `0..65535`).
    pub dual_iso: Option<std::sync::Arc<crate::dual_iso::DualIsoCalibration>>,
    /// `Some` when static hot/dead pixels are repaired in raw space (disables LJ92 passthrough).
    pub bad_pixels: Option<std::sync::Arc<crate::bad_pixels::BadPixelFix>>,
    /// `Some` when 8-column vertical stripes are corrected in raw space (disables LJ92 passthrough).
    pub stripes: Option<std::sync::Arc<crate::stripes::StripeFix>>,
    /// Dimensions of the generated DNG (differ from the source clip when `bake` is set).
    pub out_width: u32,
    pub out_height: u32,
    pub reel_name: String,
    pub dng_bits_per_sample: u16,
    pub fixed_strip_byte_count: Option<usize>,
    header_bytes: Vec<u8>,
    strip_byte_counts_offset: usize,
    timecode_offset: usize,
    as_shot_neutral_offset: usize,
    exposure_time_offset: usize,
    iso_offset: usize,
}

#[derive(Clone)]
struct IfdEntry {
    tag: u16,
    typ: u16,
    count: u32,
    bytes: Vec<u8>,
}

impl DngTemplate {
    /// Builds the DNG header template for `clip` using `requested_mode` and `reel_name`.
    pub fn new(clip: &MlvClip, requested_mode: DngPayloadMode, reel_name: String) -> Self {
        Self::new_with_processing(
            clip,
            requested_mode,
            reel_name,
            stretch_bake(),
            clip.stretch_y,
            clip.focus_fix.clone(),
            clip.dual_iso.clone(),
            clip.bad_pixels.clone(),
            clip.stripes.clone(),
        )
    }

    /// Builds a DNG header template with explicit per-clip processing overrides (used for live
    /// per-clip reconfiguration without touching global defaults).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_processing(
        clip: &MlvClip,
        requested_mode: DngPayloadMode,
        reel_name: String,
        bake_stretch: bool,
        stretch_y: f64,
        fix: Option<std::sync::Arc<crate::focus_fix::FocusFix>>,
        dual_iso: Option<std::sync::Arc<crate::dual_iso::DualIsoCalibration>>,
        bad_pixels: Option<std::sync::Arc<crate::bad_pixels::BadPixelFix>>,
        stripes: Option<std::sync::Arc<crate::stripes::StripeFix>>,
    ) -> Self {
        let mut mode = ResolvedPayloadMode::resolve(requested_mode, clip);
        let bake = if bake_stretch {
            BakePlan::for_clip(clip.width as usize, clip.height as usize, stretch_y)
        } else {
            None
        };
        if dual_iso.is_some() {
            // Dual ISO dynamic range exceeds 14-bit; always output 16-bit linear (0..65535).
            mode = ResolvedPayloadMode::Unpacked16;
        } else if (bake.is_some() || fix.is_some() || bad_pixels.is_some() || stripes.is_some())
            && mode == ResolvedPayloadMode::Lj92Passthrough
        {
            mode = ResolvedPayloadMode::NativePacked; // pixels must be touched, so passthrough is impossible
        }
        let (width, height) = match bake {
            Some(b) => (b.out_w, b.out_h),
            None => (clip.width as usize, clip.height as usize),
        };

        let (dng_bits_per_sample, compression, fixed_strip_byte_count) = match mode {
            ResolvedPayloadMode::Lj92Passthrough => {
                let bpp = clip
                    .lj92_info
                    .map(|i| i.bits as u16)
                    .unwrap_or(clip.bits_per_pixel as u16);
                (bpp, 7u16, None)
            }
            ResolvedPayloadMode::NativePacked => {
                let bpp = clip.bits_per_pixel.clamp(8, 16);
                let sz = dng_strip_byte_count(width, height, bpp);
                (bpp as u16, 1u16, Some(sz))
            }
            ResolvedPayloadMode::Unpacked16 => {
                let sz = width * height * 2;
                (16u16, 1u16, Some(sz))
            }
        };

        let max_for_depth = ((1u32 << dng_bits_per_sample) - 1) as i32;
        let levels = &clip.levels;
        let (black_level, white_level) = if dual_iso.is_some() {
            (0u32, 65535u32)
        } else {
            let bl = levels.black;
            let wl = if dng_bits_per_sample < 16 {
                (levels.white as i32).min(max_for_depth).max(1) as u32
            } else {
                levels.white.max(1)
            };
            (bl, wl)
        };

        let meta0 = &clip.dynamic_metas[0];
        let as_shot_neutral_bytes = encode_as_shot_neutral(
            meta0.wb_gain_r,
            meta0.wb_gain_g,
            meta0.wb_gain_b,
        );
        let cm1_bytes = encode_srational_matrix(&clip.color_matrix1);
        let dt_bytes = ascii_z(&clip.rtci.tiff_datetime_string());
        let tc0_bytes = encode_smpte_timecode(
            0,
            clip.rtci.seconds_since_midnight(),
            clip.fps_rounded(),
        );
        let fps_bytes = if clip.fps_denom > 0 && clip.fps_nom > 0 {
            encode_srational(clip.fps_nom as i32, clip.fps_denom as i32)
        } else {
            encode_srational(24, 1)
        };

        let make_bytes = ascii_z("CineFS");
        let model_bytes = ascii_z(&clip.camera_name);
        let software_bytes = ascii_z("CineFS");
        let reel_bytes = ascii_z(&reel_name);

        let initial_strip_bytes = fixed_strip_byte_count.unwrap_or(0) as u32;

        let mut ifd0 = vec![
            IfdEntry::new(254, TYPE_LONG, 1, u32_le(0)), // NewSubfileType
            IfdEntry::new(256, TYPE_LONG, 1, u32_le(width as u32)), // ImageWidth
            IfdEntry::new(257, TYPE_LONG, 1, u32_le(height as u32)), // ImageLength
            IfdEntry::new(258, TYPE_SHORT, 1, u16_le(dng_bits_per_sample)), // BitsPerSample
            IfdEntry::new(259, TYPE_SHORT, 1, u16_le(compression)), // Compression
            IfdEntry::new(262, TYPE_SHORT, 1, u16_le(32803)), // PhotometricInterpretation: CFA
            IfdEntry::new(271, TYPE_ASCII, make_bytes.len() as u32, make_bytes), // Make
            IfdEntry::new(272, TYPE_ASCII, model_bytes.len() as u32, model_bytes.clone()), // Model
            IfdEntry::new(273, TYPE_LONG, 1, u32_le(0)), // StripOffsets (patched after layout)
            IfdEntry::new(
                274,
                TYPE_SHORT,
                1,
                u16_le(tiff_orientation(clip.rotation_degrees)),
            ), // Orientation
            IfdEntry::new(277, TYPE_SHORT, 1, u16_le(1)), // SamplesPerPixel
            IfdEntry::new(278, TYPE_LONG, 1, u32_le(height as u32)), // RowsPerStrip
            IfdEntry::new(279, TYPE_LONG, 1, u32_le(initial_strip_bytes)), // StripByteCounts
            IfdEntry::new(284, TYPE_SHORT, 1, u16_le(1)), // PlanarConfiguration
            IfdEntry::new(305, TYPE_ASCII, software_bytes.len() as u32, software_bytes), // Software
            IfdEntry::new(306, TYPE_ASCII, dt_bytes.len() as u32, dt_bytes), // DateTime
            IfdEntry::new(33421, TYPE_SHORT, 2, u16_slice_le(&[2, 2])), // CFARepeatPatternDim
            IfdEntry::new(33422, TYPE_BYTE, 4, clip.cfa_pattern.dng_bytes().to_vec()), // CFAPattern
            IfdEntry::new(34665, TYPE_LONG, 1, u32_le(0)), // ExifIFD (patched after layout)
            IfdEntry::new(50706, TYPE_BYTE, 4, vec![1, 4, 0, 0]), // DNGVersion 1.4.0.0
            IfdEntry::new(50707, TYPE_BYTE, 4, vec![1, 1, 0, 0]), // DNGBackwardVersion 1.1.0.0
            IfdEntry::new(50708, TYPE_ASCII, model_bytes.len() as u32, model_bytes), // UniqueCameraModel
            IfdEntry::new(50711, TYPE_SHORT, 1, u16_le(1)), // CFALayout: Rectangular
            IfdEntry::new(50713, TYPE_SHORT, 2, u16_slice_le(&[1, 1])), // BlackLevelRepeatDim
            IfdEntry::new(50714, TYPE_LONG, 1, u32_le(black_level)), // BlackLevel
            IfdEntry::new(50717, TYPE_LONG, 1, u32_le(white_level)), // WhiteLevel
            IfdEntry::new(
                50718,
                TYPE_RATIONAL,
                2,
                encode_rational_slice(&default_scale_pairs(if bake.is_some() { 1.0 } else { stretch_y })),
            ), // DefaultScale (horizontal, vertical) = factor that restores square pixels
            IfdEntry::new(50719, TYPE_LONG, 2, u32_slice_le(&[0, 0])), // DefaultCropOrigin (H, V)
            IfdEntry::new(
                50720,
                TYPE_LONG,
                2,
                u32_slice_le(&[width as u32, height as u32]),
            ), // DefaultCropSize (Width, Height)
            IfdEntry::new(50721, TYPE_SRATIONAL, 9, cm1_bytes), // ColorMatrix1
            IfdEntry::new(
                50727,
                TYPE_RATIONAL,
                3,
                encode_rational_slice(&[(1, 1), (1, 1), (1, 1)]),
            ), // AnalogBalance
            IfdEntry::new(50730, TYPE_SRATIONAL, 1, encode_srational(0, 100)), // BaselineExposure
            IfdEntry::new(50731, TYPE_RATIONAL, 1, encode_rational(100, 100)), // BaselineNoise
            IfdEntry::new(50732, TYPE_RATIONAL, 1, encode_rational(100, 100)), // BaselineSharpness
            IfdEntry::new(50733, TYPE_LONG, 1, u32_le(0)), // BayerGreenSplit
            IfdEntry::new(50734, TYPE_RATIONAL, 1, encode_rational(100, 100)), // LinearResponseLimit
            IfdEntry::new(50738, TYPE_RATIONAL, 1, encode_rational(100, 100)), // AntiAliasStrength
            IfdEntry::new(50739, TYPE_RATIONAL, 1, encode_rational(1, 1)), // ShadowScale
            IfdEntry::new(
                50829,
                TYPE_LONG,
                4,
                u32_slice_le(&[0, 0, height as u32, width as u32]),
            ), // ActiveArea (Top, Left, Bottom, Right)
        ];

        if let (Some(illum2), Some(cm2)) = (&clip.calibration_illuminant2, &clip.color_matrix2) {
            ifd0.push(IfdEntry::new(
                50722,
                TYPE_SRATIONAL,
                9,
                encode_srational_matrix(cm2),
            )); // ColorMatrix2
            ifd0.push(IfdEntry::new(50779, TYPE_SHORT, 1, u16_le(*illum2))); // CalibrationIlluminant2
        }

        if let Some(fm1) = &clip.forward_matrix1 {
            ifd0.push(IfdEntry::new(
                50964,
                TYPE_SRATIONAL,
                9,
                encode_srational_matrix(fm1),
            )); // ForwardMatrix1
        }

        if let Some(fm2) = &clip.forward_matrix2 {
            ifd0.push(IfdEntry::new(
                50965,
                TYPE_SRATIONAL,
                9,
                encode_srational_matrix(fm2),
            )); // ForwardMatrix2
        }

        ifd0.push(IfdEntry::new(50728, TYPE_RATIONAL, 3, as_shot_neutral_bytes.to_vec())); // AsShotNeutral
        ifd0.push(IfdEntry::new(
            50778,
            TYPE_SHORT,
            1,
            u16_le(clip.calibration_illuminant1),
        )); // CalibrationIlluminant1
        ifd0.push(IfdEntry::new(51043, TYPE_BYTE, 8, tc0_bytes.to_vec())); // TimeCodes
        ifd0.push(IfdEntry::new(51044, TYPE_SRATIONAL, 1, fps_bytes)); // FrameRate
        if !reel_name.is_empty() {
            ifd0.push(IfdEntry::new(
                51081,
                TYPE_ASCII,
                reel_bytes.len() as u32,
                reel_bytes,
            )); // ReelName
        }

        ifd0.sort_by_key(|e| e.tag);

        // Exif SubIFD
        let mut exif = vec![
            IfdEntry::new(
                33434,
                TYPE_RATIONAL,
                1,
                encode_rational(meta0.shutter_us.max(1) as u32, 1_000_000),
            ), // ExposureTime
            IfdEntry::new(
                34855,
                TYPE_SHORT,
                1,
                u16_le(meta0.iso_value.min(65535) as u16),
            ), // ISOSpeedRatings
        ];
        if meta0.aperture_x100 > 0 {
            exif.push(IfdEntry::new(
                33437,
                TYPE_RATIONAL,
                1,
                encode_rational(meta0.aperture_x100 as u32, 100),
            )); // FNumber
        }
        if meta0.focal_length_mm > 0 {
            exif.push(IfdEntry::new(
                37386,
                TYPE_RATIONAL,
                1,
                encode_rational((meta0.focal_length_mm as u32) * 100, 100),
            )); // FocalLength
        }
        let lens_str = if meta0.lens_name.is_empty() {
            "Unknown Lens"
        } else {
            &meta0.lens_name
        };
        let lens_bytes = ascii_z(lens_str);
        exif.push(IfdEntry::new(
            42036,
            TYPE_ASCII,
            lens_bytes.len() as u32,
            lens_bytes,
        )); // LensModel
        exif.sort_by_key(|e| e.tag);

        // Serialize header + IFD0 + IFD0 overflow + ExifSubIFD + Exif overflow
        let mut header_bytes = Vec::with_capacity(1024);
        header_bytes.extend_from_slice(b"II");
        header_bytes.extend_from_slice(&42u16.to_le_bytes());
        header_bytes.extend_from_slice(&8u32.to_le_bytes()); // IFD0 starts at byte 8

        let ifd0_size = 2 + ifd0.len() * 12 + 4;
        let mut pointer = 8 + ifd0_size;
        let mut ifd0_extra = Vec::new();
        let mut ifd0_overflow_offsets = vec![0usize; ifd0.len()];

        for (i, e) in ifd0.iter().enumerate() {
            if e.bytes.len() > 4 {
                if pointer % 2 != 0 {
                    ifd0_extra.push(0);
                    pointer += 1;
                }
                ifd0_overflow_offsets[i] = pointer;
                ifd0_extra.extend_from_slice(&e.bytes);
                pointer += e.bytes.len();
            }
        }

        let exif_ifd_offset = pointer;
        let exif_size = 2 + exif.len() * 12 + 4;
        pointer += exif_size;

        let mut exif_extra = Vec::new();
        let mut exif_overflow_offsets = vec![0usize; exif.len()];
        for (i, e) in exif.iter().enumerate() {
            if e.bytes.len() > 4 {
                if pointer % 2 != 0 {
                    exif_extra.push(0);
                    pointer += 1;
                }
                exif_overflow_offsets[i] = pointer;
                exif_extra.extend_from_slice(&e.bytes);
                pointer += e.bytes.len();
            }
        }
        if pointer % 2 != 0 {
            pointer += 1;
        }
        let strip_offset = pointer;

        let mut strip_byte_counts_offset = 0usize;
        let mut timecode_offset = 0usize;
        let mut as_shot_neutral_offset = 0usize;

        // Write IFD0
        header_bytes.extend_from_slice(&(ifd0.len() as u16).to_le_bytes());
        for (i, e) in ifd0.iter().enumerate() {
            header_bytes.extend_from_slice(&e.tag.to_le_bytes());
            header_bytes.extend_from_slice(&e.typ.to_le_bytes());
            header_bytes.extend_from_slice(&e.count.to_le_bytes());

            let value_pos = if e.bytes.len() <= 4 {
                header_bytes.len()
            } else {
                ifd0_overflow_offsets[i]
            };
            match e.tag {
                279 => strip_byte_counts_offset = value_pos,
                50728 => as_shot_neutral_offset = value_pos,
                51043 => timecode_offset = value_pos,
                _ => {}
            }

            if e.tag == 273 {
                header_bytes.extend_from_slice(&(strip_offset as u32).to_le_bytes());
            } else if e.tag == 34665 {
                header_bytes.extend_from_slice(&(exif_ifd_offset as u32).to_le_bytes());
            } else if e.bytes.len() <= 4 {
                let mut padded = [0u8; 4];
                padded[..e.bytes.len()].copy_from_slice(&e.bytes);
                header_bytes.extend_from_slice(&padded);
            } else {
                header_bytes.extend_from_slice(&(ifd0_overflow_offsets[i] as u32).to_le_bytes());
            }
        }
        header_bytes.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
        header_bytes.extend_from_slice(&ifd0_extra);

        let mut exposure_time_offset = 0usize;
        let mut iso_offset = 0usize;

        // Write Exif SubIFD
        header_bytes.extend_from_slice(&(exif.len() as u16).to_le_bytes());
        for (i, e) in exif.iter().enumerate() {
            header_bytes.extend_from_slice(&e.tag.to_le_bytes());
            header_bytes.extend_from_slice(&e.typ.to_le_bytes());
            header_bytes.extend_from_slice(&e.count.to_le_bytes());

            let value_pos = if e.bytes.len() <= 4 {
                header_bytes.len()
            } else {
                exif_overflow_offsets[i]
            };
            match e.tag {
                33434 => exposure_time_offset = value_pos,
                34855 => iso_offset = value_pos,
                _ => {}
            }

            if e.bytes.len() <= 4 {
                let mut padded = [0u8; 4];
                padded[..e.bytes.len()].copy_from_slice(&e.bytes);
                header_bytes.extend_from_slice(&padded);
            } else {
                header_bytes.extend_from_slice(&(exif_overflow_offsets[i] as u32).to_le_bytes());
            }
        }
        header_bytes.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
        header_bytes.extend_from_slice(&exif_extra);

        if header_bytes.len() < strip_offset {
            header_bytes.resize(strip_offset, 0);
        }

        Self {
            mode,
            bake,
            fix,
            dual_iso,
            bad_pixels,
            stripes,
            out_width: width as u32,
            out_height: height as u32,
            reel_name,
            dng_bits_per_sample,
            fixed_strip_byte_count,
            header_bytes,
            strip_byte_counts_offset,
            timecode_offset,
            as_shot_neutral_offset,
            exposure_time_offset,
            iso_offset,
        }
    }

    /// Length of the DNG header in bytes (also equal to `StripOffsets`).
    #[inline]
    pub fn header_len(&self) -> usize {
        self.header_bytes.len()
    }

    /// Exact byte size of the pixel strip for `frame_index` (O(1), no disk I/O).
    #[inline]
    pub fn strip_byte_count(&self, clip: &MlvClip, frame_index: usize) -> usize {
        match self.fixed_strip_byte_count {
            Some(sz) => sz,
            None => clip.frames[frame_index].payload_size as usize,
        }
    }

    /// Exact total `.dng` file size in bytes for `frame_index` (O(1), no disk I/O).
    #[inline]
    pub fn file_size(&self, clip: &MlvClip, frame_index: usize) -> u64 {
        (self.header_len() + self.strip_byte_count(clip, frame_index)) as u64
    }

    /// Returns `true` if the virtual `.dng` payload bytes are 1:1 identical to the on-disk
    /// `.MLV` `VIDF` payload bytes (enabling zero-copy direct `pread` slicing without buffering).
    #[inline]
    pub fn is_direct_payload_sliceable(&self, clip: &MlvClip) -> bool {
        if self.bake.is_some() || self.fix.is_some() {
            return false;
        }
        if clip.is_braw {
            return false;
        }
        if clip.is_mcraw {
            return clip.mcraw_compression_type == 0
                && clip.mcraw_stored_format == 16
                && self.dng_bits_per_sample == 16;
        }
        match self.mode {
            ResolvedPayloadMode::Lj92Passthrough => true,
            ResolvedPayloadMode::NativePacked | ResolvedPayloadMode::Unpacked16 => {
                !clip.is_lj92 && clip.bits_per_pixel == 16
            }
        }
    }

    /// Returns `true` if any arbitrary slice of the `.dng` payload can be served directly
    /// via `pread` from the `.MLV` file followed by an in-place 16-bit byte-swap (`[b0, b1] -> [b1, b0]`),
    /// without decoding or allocating the full frame.
    #[inline]
    pub fn is_byte_swapped_sliceable(&self, clip: &MlvClip) -> bool {
        self.bake.is_none()
            && self.fix.is_none()
            && self.dual_iso.is_none()
            && self.bad_pixels.is_none()
            && self.stripes.is_none()
            && !clip.is_codec_decoded()
            && matches!(self.mode, ResolvedPayloadMode::NativePacked)
            && !clip.is_lj92
            && clip.bits_per_pixel < 16
            && ((clip.width as usize) * (self.dng_bits_per_sample as usize)) % 16 == 0
    }

    /// Synthesizes the complete DNG header for `frame_index` in `< 1 µs` without touching pixel data.
    pub fn build_header(&self, clip: &MlvClip, frame_index: usize) -> Vec<u8> {
        let mut hdr = self.header_bytes.clone();
        let strip_len = self.strip_byte_count(clip, frame_index) as u32;
        hdr[self.strip_byte_counts_offset..self.strip_byte_counts_offset + 4]
            .copy_from_slice(&strip_len.to_le_bytes());

        let tc = encode_smpte_timecode(
            frame_index as u64,
            clip.rtci.seconds_since_midnight(),
            clip.fps_rounded(),
        );
        hdr[self.timecode_offset..self.timecode_offset + 8].copy_from_slice(&tc);

        let meta_idx = clip.frames[frame_index].meta_idx as usize;
        if meta_idx > 0 {
            if let Some(meta) = clip.dynamic_metas.get(meta_idx) {
                let neutral =
                    encode_as_shot_neutral(meta.wb_gain_r, meta.wb_gain_g, meta.wb_gain_b);
                hdr[self.as_shot_neutral_offset..self.as_shot_neutral_offset + 24]
                    .copy_from_slice(&neutral);
                let exp = encode_rational(meta.shutter_us.max(1) as u32, 1_000_000);
                hdr[self.exposure_time_offset..self.exposure_time_offset + 8].copy_from_slice(&exp);
                let iso = (meta.iso_value.min(65535) as u16).to_le_bytes();
                hdr[self.iso_offset..self.iso_offset + 2].copy_from_slice(&iso);
            }
        }

        hdr
    }

    /// Decodes the source frame to 16-bit samples, stretches the Bayer mosaic (linear, same-colour
    /// neighbours only) and repacks it at the template's bit depth.
    fn build_baked_payload(
        &self,
        clip: &MlvClip,
        frame_index: usize,
        plan: BakePlan,
        raw_len: usize,
    ) -> Result<Vec<u8>> {
        let mut src = self.decode_to_u16_le(clip, frame_index, raw_len)?;
        if let Some(fix) = &self.fix {
            fix.apply_le_ex(&mut src, plan.src_w, plan.src_h, self.dual_iso.is_some()); // before stretching, in sensor space
        }
        if let Some(bp) = &self.bad_pixels {
            bp.apply_le_ex(&mut src, plan.src_w, plan.src_h, self.dual_iso.is_some());
        }
        if let Some(diso) = &self.dual_iso {
            diso.apply_le(&mut src, plan.src_w, plan.src_h); // before stretching, in sensor space
        }
        if let Some(st) = &self.stripes {
            st.apply_le(&mut src, plan.src_w, plan.src_h);
        }

        let stretched = stretch_bayer_u16(&src, plan.src_w, plan.src_h, plan.out_w, plan.out_h);
        let bpp = self.dng_bits_per_sample as u32;
        if bpp == 16 {
            return Ok(stretched);
        }
        let strip_len = self
            .fixed_strip_byte_count
            .unwrap_or_else(|| dng_strip_byte_count(plan.out_w, plan.out_h, bpp));
        let mut out = vec![0u8; strip_len];
        pack_u16_le_to_dng_packed(&stretched, plan.out_w, plan.out_h, bpp, &mut out);
        Ok(out)
    }

    /// Decodes the source frame to little-endian 16-bit samples (any container / packing).
    fn decode_to_u16_le(&self, clip: &MlvClip, frame_index: usize, raw_len: usize) -> Result<Vec<u8>> {
        let (w, h) = (clip.width as usize, clip.height as usize);
        let pixels = w * h;
        let mut raw = vec![0u8; raw_len];
        clip.read_raw_frame_payload(frame_index, &mut raw)?;
        let mut src = vec![0u8; pixels * 2];
        if clip.is_codec_decoded() {
            clip.decode_codec_frame_to_u16_le(&raw, &mut src)?;
        } else if clip.is_lj92 {
            decode_lj92_to_u16_le(&raw, pixels, &mut src)?;
        } else if clip.bits_per_pixel == 16 && raw.len() >= pixels * 2 {
            src.copy_from_slice(&raw[..pixels * 2]);
        } else {
            unpack_mlv_to_u16_le(&raw, pixels, clip.bits_per_pixel, &mut src);
        }
        Ok(src)
    }

    /// Focus-pixel and/or Dual ISO corrected payload (no stretch): decode, fix in sensor space, re-pack.
    fn build_fixed_payload(&self, clip: &MlvClip, frame_index: usize, raw_len: usize) -> Result<Vec<u8>> {
        let (w, h) = (clip.width as usize, clip.height as usize);
        let mut px = self.decode_to_u16_le(clip, frame_index, raw_len)?;
        if let Some(fix) = &self.fix {
            fix.apply_le_ex(&mut px, w, h, self.dual_iso.is_some());
        }
        if let Some(bp) = &self.bad_pixels {
            bp.apply_le_ex(&mut px, w, h, self.dual_iso.is_some());
        }
        if let Some(diso) = &self.dual_iso {
            diso.apply_le(&mut px, w, h);
        }
        if let Some(st) = &self.stripes {
            st.apply_le(&mut px, w, h);
        }
        let bpp = self.dng_bits_per_sample as u32;
        if bpp == 16 {
            return Ok(px);
        }
        let strip_len = self.fixed_strip_byte_count.unwrap_or_else(|| dng_strip_byte_count(w, h, bpp));
        let mut out = vec![0u8; strip_len];
        pack_u16_le_to_dng_packed(&px, w, h, bpp, &mut out);
        Ok(out)
    }

    /// Generates the pixel strip payload for `frame_index` according to `self.mode`.
    pub fn build_payload(&self, clip: &MlvClip, frame_index: usize) -> Result<Vec<u8>> {
        let entry = clip
            .frames
            .get(frame_index)
            .ok_or_else(|| Error::NotFound(format!("Frame {frame_index} out of range")))?;
        let raw_len = entry.payload_size as usize;

        if let Some(plan) = self.bake {
            return self.build_baked_payload(clip, frame_index, plan, raw_len);
        }
        if self.fix.is_some() || self.bad_pixels.is_some() || self.dual_iso.is_some() || self.stripes.is_some() {
            return self.build_fixed_payload(clip, frame_index, raw_len);
        }

        match self.mode {
            ResolvedPayloadMode::Lj92Passthrough => {
                let mut out = vec![0u8; raw_len];
                clip.read_raw_frame_payload(frame_index, &mut out)?;
                Ok(out)
            }
            ResolvedPayloadMode::NativePacked => {
                let width = clip.width as usize;
                let height = clip.height as usize;
                let bpp = self.dng_bits_per_sample as u32;
                let strip_len = self.strip_byte_count(clip, frame_index);

                let mut raw = vec![0u8; raw_len];
                clip.read_raw_frame_payload(frame_index, &mut raw)?;

                if clip.is_codec_decoded() {
                    let pixels = width * height;
                    let mut u16_buf = vec![0u8; pixels * 2];
                    clip.decode_codec_frame_to_u16_le(&raw, &mut u16_buf)?;
                    if bpp == 16 {
                        return Ok(u16_buf);
                    }
                    let mut out = vec![0u8; strip_len];
                    pack_u16_le_to_dng_packed(&u16_buf, width, height, bpp, &mut out);
                    Ok(out)
                } else if !clip.is_lj92 {
                    if bpp == 16 && raw.len() == strip_len {
                        return Ok(raw);
                    }
                    let mut out = vec![0u8; strip_len];
                    mlv_packed_to_dng_packed(&raw, width, height, bpp, &mut out);
                    Ok(out)
                } else {
                    let pixels = width * height;
                    let mut u16_buf = vec![0u8; pixels * 2];
                    decode_lj92_to_u16_le(&raw, pixels, &mut u16_buf)?;
                    if bpp == 16 {
                        return Ok(u16_buf);
                    }
                    let mut out = vec![0u8; strip_len];
                    pack_u16_le_to_dng_packed(&u16_buf, width, height, bpp, &mut out);
                    Ok(out)
                }
            }
            ResolvedPayloadMode::Unpacked16 => {
                let pixels = (clip.width as usize) * (clip.height as usize);
                let strip_len = pixels * 2;
                let mut raw = vec![0u8; raw_len];
                clip.read_raw_frame_payload(frame_index, &mut raw)?;

                if clip.is_codec_decoded() {
                    let mut out = vec![0u8; strip_len];
                    clip.decode_codec_frame_to_u16_le(&raw, &mut out)?;
                    Ok(out)
                } else if clip.is_lj92 {
                    let mut out = vec![0u8; strip_len];
                    decode_lj92_to_u16_le(&raw, pixels, &mut out)?;
                    Ok(out)
                } else if clip.bits_per_pixel == 16 && raw.len() >= strip_len {
                    raw.truncate(strip_len);
                    Ok(raw)
                } else {
                    let mut out = vec![0u8; strip_len];
                    unpack_mlv_to_u16_le(&raw, pixels, clip.bits_per_pixel, &mut out);
                    Ok(out)
                }
            }
        }
    }
}

impl IfdEntry {
    fn new(tag: u16, typ: u16, count: u32, bytes: Vec<u8>) -> Self {
        Self {
            tag,
            typ,
            count,
            bytes,
        }
    }
}

fn u16_le(v: u16) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn u16_slice_le(vals: &[u16]) -> Vec<u8> {
    let mut b = Vec::with_capacity(vals.len() * 2);
    for &v in vals {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

fn u32_le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn u32_slice_le(vals: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(vals.len() * 4);
    for &v in vals {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

fn ascii_z(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

fn encode_rational(num: u32, den: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&num.to_le_bytes());
    b.extend_from_slice(&den.to_le_bytes());
    b
}

/// `DefaultScale` rationals `[(1,1), (num,den)]` for a vertical stretch factor (3.0 -> 3/1, 5/3 -> 5/3).
fn default_scale_pairs(ratio_y_over_x: f64) -> [(u32, u32); 2] {
    fn snap(f: f64) -> (u32, u32) {
        if (f - 5.0 / 3.0).abs() < 1e-2 {
            (5, 3)
        } else if (f - f.round()).abs() < 1e-2 {
            (f.round() as u32, 1)
        } else {
            ((f * 10000.0).round() as u32, 10000)
        }
    }
    let r = ratio_y_over_x;
    if !r.is_finite() || r <= 0.0 || (r - 1.0).abs() < 1e-6 {
        return [(1, 1), (1, 1)];
    }
    if r < 1.0 {
        // e.g. 3x1 binning (x sampled every 3rd pixel): pixels are 3x too narrow -> horizontal scale.
        [snap(1.0 / r), (1, 1)]
    } else {
        [(1, 1), snap(r)]
    }
}

fn encode_rational_slice(pairs: &[(u32, u32)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(pairs.len() * 8);
    for &(num, den) in pairs {
        b.extend_from_slice(&num.to_le_bytes());
        b.extend_from_slice(&den.to_le_bytes());
    }
    b
}

fn encode_srational(num: i32, den: i32) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.extend_from_slice(&num.to_le_bytes());
    b.extend_from_slice(&den.to_le_bytes());
    b
}

fn encode_srational_matrix(m: &[f64; 9]) -> Vec<u8> {
    let mut b = Vec::with_capacity(72);
    for &v in m {
        let num = (v * 10_000.0).round() as i32;
        b.extend_from_slice(&num.to_le_bytes());
        b.extend_from_slice(&10_000i32.to_le_bytes());
    }
    b
}

fn encode_as_shot_neutral(r_gain: f32, g_gain: f32, b_gain: f32) -> [u8; 24] {
    let mut out = [0u8; 24];
    for (i, gain) in [r_gain, g_gain, b_gain].into_iter().enumerate() {
        let g = gain.max(0.001) as f64;
        let num = (1024.0 / g).round() as u32;
        let den = 1024u32;
        let base = i * 8;
        out[base..base + 4].copy_from_slice(&num.to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&den.to_le_bytes());
    }
    out
}

fn tiff_orientation(degrees: i32) -> u16 {
    match degrees.rem_euclid(360) {
        90 => 6,
        180 => 3,
        270 => 8,
        _ => 1,
    }
}

/// Encodes SMPTE 12M 8-byte BCD timecode for CinemaDNG `TimeCodes` (tag 51043).
pub fn encode_smpte_timecode(frame_index: u64, start_seconds: u32, fps: u32) -> [u8; 8] {
    let rate = fps.max(1) as u64;
    let total_frames = (start_seconds as u64) * rate + frame_index;
    let frames = (total_frames % rate) as u8;
    let total_seconds = total_frames / rate;
    let s = (total_seconds % 60) as u8;
    let m = ((total_seconds / 60) % 60) as u8;
    let h = ((total_seconds / 3600) % 24) as u8;
    #[inline]
    fn bcd(v: u8) -> u8 {
        ((v / 10) << 4) | (v % 10)
    }
    [
        bcd(frames) & 0x3F,
        bcd(s) & 0x7F,
        bcd(m) & 0x7F,
        bcd(h) & 0x3F,
        0,
        0,
        0,
        0,
    ]
}

#[cfg(test)]
mod default_scale_tests {
    use super::default_scale_pairs;

    #[test]
    fn default_scale_directions() {
        assert_eq!(default_scale_pairs(1.0), [(1, 1), (1, 1)]);
        assert_eq!(default_scale_pairs(3.0), [(1, 1), (3, 1)]); // vertical x3
        assert_eq!(default_scale_pairs(5.0 / 3.0), [(1, 1), (5, 3)]); // vertical x1.67
        assert_eq!(default_scale_pairs(1.0 / 3.0), [(3, 1), (1, 1)]); // EOS M 3x1: horizontal x3
        assert_eq!(default_scale_pairs(0.0), [(1, 1), (1, 1)]);
    }
}

#[cfg(test)]
mod bake_tests {
    use super::BakePlan;

    #[test]
    fn bake_plan_dimensions() {
        // EOS M crop_rec 3x1: ratio 1/3 -> horizontal x3
        let p = BakePlan::for_clip(1408, 2538, 1.0 / 3.0).unwrap();
        assert_eq!((p.out_w, p.out_h), (4224, 2538));
        // 5D3 720p style: vertical x5/3
        let p = BakePlan::for_clip(1920, 1080, 5.0 / 3.0).unwrap();
        assert_eq!((p.out_w, p.out_h), (1920, 1800));
        // vertical x3
        let p = BakePlan::for_clip(1280, 480, 3.0).unwrap();
        assert_eq!((p.out_w, p.out_h), (1280, 1440));
        // no stretch / invalid
        assert!(BakePlan::for_clip(1920, 1080, 1.0).is_none());
        assert!(BakePlan::for_clip(1920, 1080, 0.0).is_none());
        // stretched dimension is always a multiple of 4
        let p = BakePlan::for_clip(1920, 508, 5.0 / 3.0).unwrap();
        assert_eq!(p.out_h % 4, 0);
    }
}
