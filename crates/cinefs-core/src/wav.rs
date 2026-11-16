//! Broadcast WAV (`RIFF`/`WAVE` with `bext` and `iXML` timecode chunks) virtual file stream.
//!
//! Pre-builds the ~1.2 KB RIFF/BWF header once on clip open and serves arbitrary byte-range
//! reads directly from the `.MLV` file's indexed `AUDF` blocks in `O(log N)` without buffering
//! the full audio track in memory.

use crate::mlv::{MlvAudioInfo, MlvClip};
use crate::Result;

#[derive(Debug, Clone)]
pub struct VirtualWav {
    pub header_bytes: Vec<u8>,
    pub total_pcm_bytes: u64,
}

impl VirtualWav {
    pub fn new(clip: &MlvClip, reel_name: &str) -> Option<Self> {
        let info = clip.audio_info?;
        if info.total_pcm_bytes == 0 || info.sample_rate == 0 || info.channels == 0 {
            return None;
        }
        let header_bytes = build_bwf_wav_header(&info, clip, reel_name);
        Some(Self {
            header_bytes,
            total_pcm_bytes: info.total_pcm_bytes,
        })
    }

    #[inline]
    pub fn file_size(&self) -> u64 {
        (self.header_bytes.len() as u64) + self.total_pcm_bytes
    }

    /// Reads `[offset .. offset + dst.len()]` from the virtual WAV file.
    pub fn read_at(&self, clip: &MlvClip, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let total_size = self.file_size();
        if offset >= total_size || dst.is_empty() {
            return Ok(0);
        }
        let max_len = (total_size - offset).min(dst.len() as u64) as usize;
        let dst = &mut dst[..max_len];
        let hdr_len = self.header_bytes.len() as u64;

        let mut written = 0usize;
        if offset < hdr_len {
            let hdr_off = offset as usize;
            let take = (self.header_bytes.len() - hdr_off).min(dst.len());
            dst[..take].copy_from_slice(&self.header_bytes[hdr_off..hdr_off + take]);
            written += take;
        }

        if written < dst.len() {
            let pcm_off = (offset + written as u64).saturating_sub(hdr_len);
            let n = clip.read_audio_pcm_slice(pcm_off, &mut dst[written..])?;
            written += n;
        }

        Ok(written)
    }
}

fn build_bwf_wav_header(info: &MlvAudioInfo, clip: &MlvClip, reel_name: &str) -> Vec<u8> {
    let sample_rate = info.sample_rate;
    let channels = info.channels;
    let bits_per_sample = info.bits_per_sample.max(16);
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * (block_align as u32);

    let seconds_since_midnight = clip.rtci.seconds_since_midnight() as u64;
    let time_reference = seconds_since_midnight * (sample_rate as u64);
    let date_str = clip.rtci.date_ymd_string();
    let time_str = clip.rtci.time_hms_string();
    let fps = clip.fps_rounded();

    // 1. Build `bext` chunk body (EBU Tech 3285 v1)
    let coding_history = format!(
        "A=PCM,F={sample_rate},W={bits_per_sample},T=CineFS\r\n"
    );
    let coding_bytes = coding_history.as_bytes();
    let mut bext = Vec::with_capacity(602 + coding_bytes.len());
    push_fixed_ascii(&mut bext, "CineFS", 256); // Description
    push_fixed_ascii(&mut bext, "CineFS", 32); // Originator
    push_fixed_ascii(&mut bext, reel_name, 32); // OriginatorReference
    push_fixed_ascii(&mut bext, &date_str, 10); // OriginationDate
    push_fixed_ascii(&mut bext, &time_str, 8); // OriginationTime
    bext.extend_from_slice(&(time_reference as u32).to_le_bytes()); // TimeReferenceLow
    bext.extend_from_slice(&((time_reference >> 32) as u32).to_le_bytes()); // TimeReferenceHigh
    bext.extend_from_slice(&1u16.to_le_bytes()); // Version = 1
    bext.resize(bext.len() + 64 + 10 + 180, 0); // UMID(64) + Loudness(10) + Reserved(180)
    bext.extend_from_slice(coding_bytes);

    // 2. Build `iXML` chunk body
    let ixml_str = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><BWFXML>\
         <IXML_VERSION>1.5</IXML_VERSION>\
         <PROJECT>CineFS</PROJECT><TAPE>1</TAPE><SCENE>1</SCENE><TAKE>1</TAKE>\
         <SPEED>\
         <MASTER_SPEED>{fps}/1</MASTER_SPEED><CURRENT_SPEED>{fps}/1</CURRENT_SPEED>\
         <TIMECODE_RATE>{fps}/1</TIMECODE_RATE><TIMECODE_FLAG>NDF</TIMECODE_FLAG>\
         <FILE_SAMPLE_RATE>{sample_rate}</FILE_SAMPLE_RATE>\
         <TIMESTAMP_SAMPLE_RATE>{sample_rate}</TIMESTAMP_SAMPLE_RATE>\
         <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>{}</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_HI>\
         <TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>{}</TIMESTAMP_SAMPLES_SINCE_MIDNIGHT_LO>\
         </SPEED></BWFXML>",
        time_reference >> 32,
        time_reference & 0xFFFF_FFFF
    );
    let ixml = ixml_str.into_bytes();

    let mut extra_chunks = Vec::new();
    append_riff_chunk(&mut extra_chunks, b"bext", &bext);
    append_riff_chunk(&mut extra_chunks, b"iXML", &ixml);

    let data_size = info.total_pcm_bytes.min(u32::MAX as u64) as u32;
    let riff_size = (36 + extra_chunks.len() as u64 + info.total_pcm_bytes).min(u32::MAX as u64) as u32;

    let mut header = Vec::with_capacity(44 + extra_chunks.len());
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(&extra_chunks);
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM = 1
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&sample_rate.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&bits_per_sample.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_size.to_le_bytes());
    header
}

fn push_fixed_ascii(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    let copy_len = bytes.len().min(len);
    out.extend_from_slice(&bytes[..copy_len]);
    if copy_len < len {
        out.resize(out.len() + (len - copy_len), 0);
    }
}

fn append_riff_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 != 0 {
        out.push(0);
    }
}
