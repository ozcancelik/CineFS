//! Audio playback for the player and the master clock that the picture follows.
//!
//! * A **feeder thread** reads PCM from the clip (`MlvClip::read_audio_pcm_slice`, the same bytes the
//!   virtual `.wav` serves), converts it to the device's channel layout and sample rate (linear
//!   resampling: good enough for a preview) and keeps a short queue filled. The real-time callback
//!   never touches the disk.
//! * The **output callback** (cpal: CoreAudio / WASAPI) pops samples from that queue and records how
//!   many device frames it has handed over, when, and the output latency it was told about.
//! * [`AudioCore::elapsed`] turns that into "seconds of this playback run that are audible right now",
//!   which the engine uses instead of the wall clock. Picture therefore waits for (and follows) sound.
//!
//! PCM sample 0 is taken to line up with video frame 0, exactly as in the virtual `.wav` that NLEs use.

use crate::engine::AudioSync;
use cinefs_core::vfs::MountedClip;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How much converted audio the feeder keeps queued ahead of the callback.
const QUEUE_SECS: f64 = 0.25;
/// Output frames converted per feeder iteration.
const CHUNK_FRAMES: usize = 1024;
/// Source frames read from the clip per disk access.
const SOURCE_BLOCK_FRAMES: usize = 8192;
/// If the device has not asked for audio this long after a start, fall back to the wall clock.
const FIRST_CALLBACK_TIMEOUT: Duration = Duration::from_millis(500);

/// Source PCM description.
#[derive(Clone, Copy, Debug)]
pub struct SourceFormat {
    pub rate: u32,
    pub channels: usize,
    pub bytes_per_sample: usize,
    pub total_frames: u64,
}

impl SourceFormat {
    pub fn for_clip(clip: &MountedClip) -> Option<Self> {
        let info = clip.clip.audio_info?;
        let bytes_per_sample = match info.bits_per_sample {
            16 => 2,
            24 => 3,
            32 => 4,
            _ => return None,
        };
        let channels = info.channels as usize;
        if info.sample_rate == 0 || channels == 0 {
            return None;
        }
        let total_frames = info.total_pcm_bytes / (channels * bytes_per_sample) as u64;
        (total_frames > 0).then_some(Self { rate: info.sample_rate, channels, bytes_per_sample, total_frames })
    }
}

/// Where the feeder reads source frames from (the clip, or a synthetic source in tests).
pub trait PcmSource: Send + Sync + 'static {
    /// Reads interleaved little-endian signed PCM bytes starting at `byte_offset`; returns bytes read.
    fn read(&self, byte_offset: u64, dst: &mut [u8]) -> usize;
}

struct ClipPcm(Arc<MountedClip>);

impl PcmSource for ClipPcm {
    fn read(&self, byte_offset: u64, dst: &mut [u8]) -> usize {
        self.0.clip.read_audio_pcm_slice(byte_offset, dst).unwrap_or(0)
    }
}

/// Everything the callback, the feeder and the engine share (one short-held lock).
struct State {
    /// Bumped by every start/stop so stale feeder output is never queued.
    generation: u64,
    running: bool,
    /// Media time (seconds) of the first sample of this run, and the loop length if looping.
    start_secs: f64,
    loop_secs: Option<f64>,
    started_at: Instant,
    /// Interleaved device-format samples waiting for the callback.
    queue: VecDeque<f32>,
    /// Device frames the callback has handed over in this run (silence on underrun included).
    rendered: u64,
    /// Snapshot of the latest callback, for interpolating between callbacks.
    last_cb: Option<CallbackSnapshot>,
    /// Never report less than this (keeps the picture from stepping backwards).
    last_elapsed: f64,
}

#[derive(Clone, Copy)]
struct CallbackSnapshot {
    at: Instant,
    frames_before: u64,
    frames_in_buffer: u64,
    latency_secs: f64,
}

pub struct AudioCore {
    state: Mutex<State>,
    device_rate: u32,
    device_channels: usize,
    gain_bits: AtomicU32,
    failed: AtomicBool,
    shutdown: AtomicBool,
}

impl AudioCore {
    fn new(device_rate: u32, device_channels: usize) -> Self {
        Self {
            state: Mutex::new(State {
                generation: 0,
                running: false,
                start_secs: 0.0,
                loop_secs: None,
                started_at: Instant::now(),
                queue: VecDeque::new(),
                rendered: 0,
                last_cb: None,
                last_elapsed: 0.0,
            }),
            device_rate,
            device_channels,
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            failed: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.gain_bits.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    /// The real-time half: fill `out` (interleaved, device channels) and advance the clock.
    fn render(&self, out: &mut [f32], latency_secs: f64) {
        let ch = self.device_channels.max(1);
        let frames = (out.len() / ch) as u64;
        let gain = self.volume();
        let mut st = self.state.lock().unwrap();
        if !st.running {
            out.fill(0.0);
            return;
        }
        for s in out.iter_mut() {
            *s = st.queue.pop_front().unwrap_or(0.0) * gain;
        }
        let before = st.rendered;
        st.rendered += frames;
        st.last_cb = Some(CallbackSnapshot {
            at: Instant::now(),
            frames_before: before,
            frames_in_buffer: frames,
            latency_secs,
        });
    }

    /// Seconds of the current run audible at `now` (see module docs). `None` = use the wall clock.
    fn elapsed_at(&self, now: Instant) -> Option<f64> {
        if self.failed.load(Ordering::Relaxed) {
            return None;
        }
        let mut st = self.state.lock().unwrap();
        if !st.running {
            return None;
        }
        let rate = self.device_rate as f64;
        let e = match st.last_cb {
            None => {
                if now.duration_since(st.started_at) > FIRST_CALLBACK_TIMEOUT {
                    return None; // device is not pulling audio: do not freeze the picture
                }
                0.0
            }
            Some(cb) => {
                let since = now.saturating_duration_since(cb.at).as_secs_f64();
                let within = since.min(cb.frames_in_buffer as f64 / rate);
                (cb.frames_before as f64 / rate + within - cb.latency_secs).max(0.0)
            }
        };
        let e = e.max(st.last_elapsed);
        st.last_elapsed = e;
        Some(e)
    }
}

impl AudioSync for AudioCore {
    fn start(&self, media_secs: f64, loop_secs: Option<f64>) {
        let mut st = self.state.lock().unwrap();
        st.generation += 1;
        st.running = true;
        st.start_secs = media_secs.max(0.0);
        st.loop_secs = loop_secs.filter(|d| *d > 0.0);
        st.started_at = Instant::now();
        st.queue.clear();
        st.rendered = 0;
        st.last_cb = None;
        st.last_elapsed = 0.0;
    }

    fn stop(&self) {
        let mut st = self.state.lock().unwrap();
        st.generation += 1;
        st.running = false;
        st.queue.clear();
        st.last_cb = None;
    }

    fn elapsed(&self) -> Option<f64> {
        self.elapsed_at(Instant::now())
    }
}

/// Converts source PCM to device frames for one playback run.
struct Converter {
    src: Arc<dyn PcmSource>,
    fmt: SourceFormat,
    device_rate: u32,
    device_channels: usize,
    /// Cached block of decoded source frames (interleaved, source channels).
    block: Vec<f32>,
    block_start: u64,
    block_frames: u64,
    raw: Vec<u8>,
}

impl Converter {
    fn new(src: Arc<dyn PcmSource>, fmt: SourceFormat, device_rate: u32, device_channels: usize) -> Self {
        Self {
            src,
            fmt,
            device_rate,
            device_channels,
            block: Vec::new(),
            block_start: 0,
            block_frames: 0,
            raw: Vec::new(),
        }
    }

    fn load_block(&mut self, first: u64) {
        let ch = self.fmt.channels;
        let bps = self.fmt.bytes_per_sample;
        let frames = (SOURCE_BLOCK_FRAMES as u64).min(self.fmt.total_frames.saturating_sub(first)) as usize;
        self.raw.resize(frames * ch * bps, 0);
        let got = self.src.read(first * (ch * bps) as u64, &mut self.raw) / (ch * bps);
        self.block.clear();
        self.block.reserve(got * ch);
        for s in self.raw[..got * ch * bps].chunks_exact(bps) {
            let v = match bps {
                2 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                3 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
                _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            };
            self.block.push(v);
        }
        self.block_start = first;
        self.block_frames = got as u64;
    }

    /// Source sample `frame`/`channel`, or silence outside the recording.
    fn sample(&mut self, frame: u64, channel: usize) -> f32 {
        if frame >= self.fmt.total_frames {
            return 0.0;
        }
        if frame < self.block_start || frame >= self.block_start + self.block_frames {
            self.load_block(frame);
            if self.block_frames == 0 {
                return 0.0;
            }
        }
        self.block[(frame - self.block_start) as usize * self.fmt.channels + channel]
    }

    fn source_value(&mut self, pos: f64, channel: usize) -> f32 {
        let i0 = pos.floor().max(0.0) as u64;
        let t = (pos - i0 as f64) as f32;
        let a = self.sample(i0, channel);
        if t == 0.0 {
            return a;
        }
        let b = self.sample(i0 + 1, channel);
        a + (b - a) * t
    }

    /// Appends output frames `[first_out, first_out + n)` of a run starting at `start_secs`.
    fn render(&mut self, start_secs: f64, loop_secs: Option<f64>, first_out: u64, n: usize, out: &mut Vec<f32>) {
        let src_ch = self.fmt.channels;
        let dev_ch = self.device_channels;
        let dev_rate = self.device_rate as f64;
        let src_rate = self.fmt.rate as f64;
        for k in 0..n as u64 {
            let mut media = start_secs + (first_out + k) as f64 / dev_rate;
            if let Some(d) = loop_secs {
                media = media.rem_euclid(d);
            }
            let pos = media * src_rate;
            if src_ch == 1 {
                let v = self.source_value(pos, 0);
                out.extend(std::iter::repeat(v).take(dev_ch));
            } else if dev_ch == 1 {
                let sum: f32 = (0..src_ch).map(|c| self.source_value(pos, c)).sum();
                out.push(sum / src_ch as f32);
            } else {
                for c in 0..dev_ch {
                    out.push(if c < src_ch { self.source_value(pos, c) } else { 0.0 });
                }
            }
        }
    }
}

fn feeder(core: Arc<AudioCore>, mut conv: Converter) {
    let target = (QUEUE_SECS * core.device_rate as f64) as usize * core.device_channels;
    let mut generation = u64::MAX;
    let mut produced: u64 = 0;
    let mut buf = Vec::with_capacity(CHUNK_FRAMES * core.device_channels);
    while !core.shutdown.load(Ordering::Relaxed) {
        let job = {
            let st = core.state.lock().unwrap();
            if !st.running || st.queue.len() >= target {
                None
            } else {
                if st.generation != generation {
                    generation = st.generation;
                    produced = 0;
                }
                Some((st.generation, st.start_secs, st.loop_secs))
            }
        };
        let Some((gen, start, looping)) = job else {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        };
        buf.clear();
        conv.render(start, looping, produced, CHUNK_FRAMES, &mut buf);
        let mut st = core.state.lock().unwrap();
        if st.generation == gen {
            st.queue.extend(buf.iter().copied());
            produced += CHUNK_FRAMES as u64;
        }
    }
}

/// Owns the output stream and the feeder thread for one player. Not `Send`: lives on the main thread.
pub struct AudioOutput {
    pub core: Arc<AudioCore>,
    stream: cpal::Stream,
    stream_active: bool,
    feeder: Option<JoinHandle<()>>,
}

impl AudioOutput {
    /// Opens the default output device for `clip`. `Ok(None)` when the clip has no (usable) audio.
    pub fn for_clip(clip: &Arc<MountedClip>) -> Result<Option<Self>, String> {
        let Some(fmt) = SourceFormat::for_clip(clip) else {
            return Ok(None);
        };
        Self::open(Arc::new(ClipPcm(clip.clone())), fmt).map(Some)
    }

    pub fn open(src: Arc<dyn PcmSource>, fmt: SourceFormat) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let supported = device.default_output_config().map_err(|e| format!("output config: {e}"))?;
        let config: cpal::StreamConfig = supported.config();
        let core = Arc::new(AudioCore::new(config.sample_rate, config.channels as usize));

        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, core.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, core.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, core.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, core.clone()),
            other => Err(format!("unsupported output sample format {other}")),
        }?;
        // Keep the device idle until playback starts (an active stream keeps the Mac awake).
        let _ = stream.pause();

        let conv = Converter::new(src, fmt, config.sample_rate, config.channels as usize);
        let feeder_core = core.clone();
        let feeder = std::thread::Builder::new()
            .name("cinefs-audio-feed".into())
            .spawn(move || feeder(feeder_core, conv))
            .map_err(|e| format!("audio thread: {e}"))?;

        crate::log_info(&format!(
            "player audio: source {} Hz x{} ({}-bit) -> device {} Hz x{} {:?}",
            fmt.rate,
            fmt.channels,
            fmt.bytes_per_sample * 8,
            config.sample_rate,
            config.channels,
            supported.sample_format()
        ));
        Ok(Self { core, stream, stream_active: false, feeder: Some(feeder) })
    }

    /// Runs the device only while playback is running. Call from the main thread (each tick).
    pub fn sync_device(&mut self) {
        let want = self.core.is_running();
        if want == self.stream_active {
            return;
        }
        let r = if want { self.stream.play() } else { self.stream.pause() };
        match r {
            Ok(()) => self.stream_active = want,
            Err(e) => {
                crate::log_warn(&format!("audio stream {}: {e}", if want { "play" } else { "pause" }));
                self.core.failed.store(true, Ordering::Relaxed);
                self.stream_active = want; // do not retry every tick
            }
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.core.shutdown.store(true, Ordering::Relaxed);
        let _ = self.stream.pause();
        if let Some(h) = self.feeder.take() {
            let _ = h.join();
        }
    }
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, core: Arc<AudioCore>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let err_core = core.clone();
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config.clone(),
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                let latency = ts.playback.saturating_duration_since(ts.callback).as_secs_f64();
                scratch.resize(data.len(), 0.0);
                core.render(&mut scratch, latency);
                for (d, s) in data.iter_mut().zip(scratch.iter()) {
                    *d = T::from_sample(*s);
                }
            },
            move |err: cpal::Error| {
                use cpal::ErrorKind::*;
                match err.kind() {
                    Xrun | DeviceChanged | RealtimeDenied => crate::log_warn(&format!("audio: {err}")),
                    _ => {
                        crate::log_warn(&format!("audio stream failed, picture uses the wall clock: {err}"));
                        err_core.failed.store(true, Ordering::Relaxed);
                    }
                }
            },
            None,
        )
        .map_err(|e| format!("audio stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16-bit mono ramp: sample i has value i.
    struct Ramp(u64);
    impl PcmSource for Ramp {
        fn read(&self, byte_offset: u64, dst: &mut [u8]) -> usize {
            let first = byte_offset / 2;
            let mut n = 0;
            for (k, pair) in dst.chunks_exact_mut(2).enumerate() {
                let i = first + k as u64;
                if i >= self.0 {
                    break;
                }
                pair.copy_from_slice(&(i as i16).to_le_bytes());
                n += 2;
            }
            n
        }
    }

    fn ramp_conv(frames: u64, src_rate: u32, dev_rate: u32, dev_ch: usize) -> Converter {
        let fmt = SourceFormat { rate: src_rate, channels: 1, bytes_per_sample: 2, total_frames: frames };
        Converter::new(Arc::new(Ramp(frames)), fmt, dev_rate, dev_ch)
    }

    fn val(i: i32) -> f32 {
        i as f32 / 32768.0
    }

    #[test]
    fn same_rate_is_sample_exact_and_mono_is_duplicated() {
        let mut c = ramp_conv(20000, 48000, 48000, 2);
        let mut out = Vec::new();
        c.render(0.0, None, 0, 4, &mut out);
        assert_eq!(out, vec![val(0), val(0), val(1), val(1), val(2), val(2), val(3), val(3)]);
        // Starting at 0.1 s lands on sample 4800.
        out.clear();
        c.render(0.1, None, 0, 1, &mut out);
        assert!((out[0] - val(4800)).abs() < 1e-6);
    }

    #[test]
    fn resampling_interpolates_between_source_samples() {
        let mut c = ramp_conv(20000, 48000, 96000, 1);
        let mut out = Vec::new();
        c.render(0.0, None, 0, 4, &mut out);
        let expect = [val(0), (val(0) + val(1)) / 2.0, val(1), (val(1) + val(2)) / 2.0];
        for (a, b) in out.iter().zip(expect) {
            assert!((a - b).abs() < 1e-6, "{out:?}");
        }
    }

    #[test]
    fn loop_wraps_to_the_start_and_end_is_silent() {
        // 1 s source at 1 kHz, loop length 0.5 s.
        let mut c = ramp_conv(1000, 1000, 1000, 1);
        let mut out = Vec::new();
        c.render(0.0, Some(0.5), 499, 3, &mut out);
        assert_eq!(out, vec![val(499), val(0), val(1)]);
        out.clear();
        c.render(0.0, None, 999, 3, &mut out);
        assert_eq!(out, vec![val(999), 0.0, 0.0]);
    }

    #[test]
    fn clock_waits_for_the_device_then_follows_rendered_frames() {
        let core = AudioCore::new(1000, 1);
        core.start(2.0, None);
        assert_eq!(core.elapsed(), Some(0.0)); // nothing rendered yet

        let mut buf = vec![0.0f32; 100];
        core.render(&mut buf, 0.05); // 0.1 s handed over, 50 ms latency
        let at = core.state.lock().unwrap().last_cb.unwrap().at;
        let e = core.elapsed_at(at + Duration::from_millis(80)).unwrap();
        assert!((e - 0.03).abs() < 1e-9, "{e}"); // 80 ms into the buffer, minus 50 ms latency
        // Never beyond what was handed over, and never backwards.
        let e2 = core.elapsed_at(at + Duration::from_millis(500)).unwrap();
        assert!((e2 - 0.05).abs() < 1e-9, "{e2}");
        assert!(core.elapsed_at(at).unwrap() >= e2);

        core.stop();
        assert_eq!(core.elapsed(), None);
    }

    #[test]
    fn failed_device_falls_back_to_wall_clock() {
        let core = AudioCore::new(1000, 1);
        core.start(0.0, None);
        core.failed.store(true, Ordering::Relaxed);
        assert_eq!(core.elapsed(), None);
    }

    /// Plays a real clip through the default output device and checks the audio clock advances at
    /// real-time speed. `CINEFS_AUDIO_SAMPLE=/path/clip.MLV cargo test -p cinefs-player -- --ignored`
    #[test]
    #[ignore]
    fn real_device_clock_runs_at_real_time() {
        let Ok(path) = std::env::var("CINEFS_AUDIO_SAMPLE") else { return };
        let clip = Arc::new(MountedClip::open(&path, 0, cinefs_core::DngPayloadMode::Auto).unwrap());
        let mut out = AudioOutput::for_clip(&clip).unwrap().expect("clip has audio");
        out.core.start(0.0, None);
        out.sync_device();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(1200));
        let wall = t0.elapsed().as_secs_f64();
        let heard = out.core.elapsed().expect("device is pulling audio");
        out.core.stop();
        out.sync_device();
        eprintln!("wall {wall:.3}s, audio clock {heard:.3}s");
        // The clock starts late by the output latency, otherwise tracks real time.
        assert!(heard > wall - 0.25 && heard <= wall + 0.01, "wall {wall}, audio {heard}");
    }

    #[test]
    fn restart_discards_queued_audio() {
        let core = AudioCore::new(1000, 1);
        core.start(0.0, None);
        core.state.lock().unwrap().queue.extend([0.5f32; 10]);
        core.start(1.0, None);
        let mut buf = vec![1.0f32; 4];
        core.render(&mut buf, 0.0);
        assert_eq!(buf, vec![0.0; 4]); // underrun -> silence, not the old run's samples
    }
}
