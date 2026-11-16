//! Playback engine: a wall-clock transport (play / pause / seek / loop) in front of a pool of decoder
//! threads that keep a window of upcoming frames ready. Frames that arrive too late are skipped, never
//! waited for, so the picture keeps real-time speed. When the clip has sound, the audio output is the
//! master clock (see [`AudioSync`]); the wall clock is the fallback.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// A clock that is driven by audio output. The engine starts it at a media position and then asks how
/// much of that run is audible; returning `None` makes the engine use the wall clock instead.
pub trait AudioSync: Send + Sync {
    /// Begin playing from `media_secs`; with `loop_secs`, media time wraps at that length.
    fn start(&self, media_secs: f64, loop_secs: Option<f64>);
    fn stop(&self);
    /// Seconds of the current run that have been heard so far.
    fn elapsed(&self) -> Option<f64>;
}

/// Source of decoded Bayer frames (host-order `u16` samples, focus pixels already corrected).
pub trait FrameProvider: Send + Sync + 'static {
    fn frame_count(&self) -> usize;
    fn fps(&self) -> f64;
    fn decode(&self, index: usize) -> Result<Vec<u16>, String>;
}

struct State {
    ready: HashMap<usize, Arc<Vec<u16>>>,
    inflight: HashSet<usize>,
    failed: HashSet<usize>,
    /// Frames the transport wants, most urgent first.
    wanted: Vec<usize>,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    stop: AtomicBool,
    decoded: AtomicU64,
}

struct Clock {
    playing: bool,
    looping: bool,
    base_instant: Instant,
    /// Position (in frames) at `base_instant`.
    base_pos: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub decoded: u64,
    pub shown: u64,
    /// Frames skipped because they were not decoded in time while playing.
    pub dropped: u64,
}

pub struct Engine {
    provider: Arc<dyn FrameProvider>,
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    clock: Mutex<Clock>,
    window: usize,
    shown: AtomicU64,
    dropped: AtomicU64,
    last_shown: Mutex<Option<usize>>,
    audio: Option<Arc<dyn AudioSync>>,
}

impl Engine {
    /// `threads` decoder threads, `window` frames decoded ahead of the playhead.
    pub fn new(provider: Arc<dyn FrameProvider>, threads: usize, window: usize) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { ready: HashMap::new(), inflight: HashSet::new(), failed: HashSet::new(), wanted: Vec::new() }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            decoded: AtomicU64::new(0),
        });
        let workers = (0..threads.max(1))
            .map(|i| {
                let shared = shared.clone();
                let provider = provider.clone();
                std::thread::Builder::new()
                    .name(format!("cinefs-decode-{i}"))
                    .spawn(move || worker(shared, provider))
                    .expect("spawn decoder thread")
            })
            .collect();
        Self {
            provider,
            shared,
            workers,
            clock: Mutex::new(Clock { playing: false, looping: true, base_instant: Instant::now(), base_pos: 0.0 }),
            window: window.max(2),
            shown: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_shown: Mutex::new(None),
            audio: None,
        }
    }

    /// Makes `audio` the master clock (set before playback starts).
    pub fn set_audio(&mut self, audio: Arc<dyn AudioSync>) {
        self.audio = Some(audio);
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn loop_secs(&self, looping: bool) -> Option<f64> {
        looping.then(|| self.frame_count() as f64 / self.fps())
    }

    /// (Re)starts the audio run at the clock's current base position.
    fn start_audio(&self, c: &Clock) {
        if let Some(a) = &self.audio {
            a.start(c.base_pos / self.fps(), self.loop_secs(c.looping));
        }
    }

    fn stop_audio(&self) {
        if let Some(a) = &self.audio {
            a.stop();
        }
    }

    /// Reasonable defaults for this machine and frame size (decode threads leave cores for the UI/GPU).
    pub fn auto_config(frame_bytes: usize, fps: f64) -> (usize, usize) {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let threads = cores.saturating_sub(1).clamp(2, 8);
        let by_time = (fps * 0.6).ceil() as usize;
        let by_mem = (384 * 1024 * 1024 / frame_bytes.max(1)).max(4);
        (threads, by_time.clamp(6, 24).min(by_mem))
    }

    pub fn frame_count(&self) -> usize {
        self.provider.frame_count()
    }

    pub fn fps(&self) -> f64 {
        self.provider.fps().max(1.0)
    }

    pub fn is_playing(&self) -> bool {
        self.clock.lock().unwrap().playing
    }

    pub fn is_looping(&self) -> bool {
        self.clock.lock().unwrap().looping
    }

    pub fn set_looping(&self, on: bool) {
        let mut c = self.clock.lock().unwrap();
        self.rebase(&mut c);
        c.looping = on;
        if c.playing {
            self.start_audio(&c);
        }
    }

    fn rebase(&self, c: &mut Clock) {
        let pos = self.position_locked(c);
        c.base_pos = pos;
        c.base_instant = Instant::now();
    }

    fn position_locked(&self, c: &Clock) -> f64 {
        let n = self.frame_count().max(1) as f64;
        let mut pos = c.base_pos;
        if c.playing {
            let secs = self
                .audio
                .as_ref()
                .and_then(|a| a.elapsed())
                .unwrap_or_else(|| c.base_instant.elapsed().as_secs_f64());
            pos += secs * self.fps();
        }
        if c.looping {
            pos.rem_euclid(n)
        } else {
            pos.clamp(0.0, n - 1.0)
        }
    }

    /// Playhead in frames (fractional).
    pub fn position(&self) -> f64 {
        self.position_locked(&self.clock.lock().unwrap())
    }

    pub fn play(&self) {
        let mut c = self.clock.lock().unwrap();
        if c.playing {
            return;
        }
        // Restart from the beginning when playing from the end of a non-looping clip.
        if !c.looping && c.base_pos >= self.frame_count().saturating_sub(1) as f64 {
            c.base_pos = 0.0;
        }
        c.base_instant = Instant::now();
        c.playing = true;
        self.start_audio(&c);
    }

    pub fn pause(&self) {
        let mut c = self.clock.lock().unwrap();
        if c.playing {
            self.rebase(&mut c);
            c.playing = false;
            self.stop_audio();
        }
    }

    pub fn toggle(&self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn seek(&self, frame: f64) {
        let mut c = self.clock.lock().unwrap();
        let n = self.frame_count().max(1) as f64;
        c.base_pos = frame.clamp(0.0, n - 1.0);
        c.base_instant = Instant::now();
        if c.playing {
            self.start_audio(&c);
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            decoded: self.shared.decoded.load(Ordering::Relaxed),
            shown: self.shown.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Call once per display refresh. Returns the frame to show (`index`, samples) when it differs from
    /// the previously returned one, `None` when nothing new is available.
    pub fn next_frame(&self) -> Option<(usize, Arc<Vec<u16>>)> {
        let n = self.frame_count();
        if n == 0 {
            return None;
        }
        let (pos, playing, looping) = {
            let mut c = self.clock.lock().unwrap();
            let pos = self.position_locked(&c);
            // A non-looping clip stops at its last frame.
            if c.playing && !c.looping && pos >= (n - 1) as f64 {
                c.base_pos = (n - 1) as f64;
                c.playing = false;
                self.stop_audio();
            }
            (pos, c.playing, c.looping)
        };
        let target = (pos.floor() as usize).min(n - 1);

        let wanted: Vec<usize> = (0..self.window)
            .map(|k| target + k)
            .filter_map(|i| if i < n { Some(i) } else if looping { Some(i % n) } else { None })
            .collect();

        let mut st = self.shared.state.lock().unwrap();
        // Keep a couple of already-shown frames (for backwards seeks of one or two frames).
        let keep_behind: Vec<usize> = (1..=2).filter_map(|k| target.checked_sub(k)).collect();
        st.ready.retain(|i, _| wanted.contains(i) || keep_behind.contains(i));
        st.failed.retain(|i| wanted.contains(i));
        st.wanted = wanted;
        self.shared.cv.notify_all();

        // Best available: the target, otherwise the nearest earlier frame within the window.
        let chosen = if st.ready.contains_key(&target) {
            Some(target)
        } else {
            (1..=self.window).filter_map(|k| target.checked_sub(k)).find(|i| st.ready.contains_key(i))
        };
        let (idx, data) = match chosen.and_then(|i| st.ready.get(&i).map(|d| (i, d.clone()))) {
            Some(x) => x,
            None => return None,
        };
        drop(st);

        let mut last = self.last_shown.lock().unwrap();
        if *last == Some(idx) {
            return None;
        }
        if playing {
            if let Some(prev) = *last {
                let step = if idx >= prev { idx - prev } else { idx + n - prev };
                if step > 1 && !(looping && idx < prev) {
                    self.dropped.fetch_add((step - 1) as u64, Ordering::Relaxed);
                }
            }
        }
        *last = Some(idx);
        self.shown.fetch_add(1, Ordering::Relaxed);
        Some((idx, data))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cv.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(shared: Arc<Shared>, provider: Arc<dyn FrameProvider>) {
    loop {
        let idx = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                let next = st
                    .wanted
                    .iter()
                    .copied()
                    .find(|i| !st.ready.contains_key(i) && !st.inflight.contains(i) && !st.failed.contains(i));
                if let Some(i) = next {
                    st.inflight.insert(i);
                    break i;
                }
                st = shared.cv.wait(st).unwrap();
            }
        };
        let result = provider.decode(idx);
        let mut st = shared.state.lock().unwrap();
        st.inflight.remove(&idx);
        match result {
            Ok(v) => {
                shared.decoded.fetch_add(1, Ordering::Relaxed);
                if st.wanted.contains(&idx) {
                    st.ready.insert(idx, Arc::new(v));
                }
            }
            Err(e) => {
                crate::log_warn(&format!("player: decode of frame {idx} failed: {e}"));
                st.failed.insert(idx);
            }
        }
        shared.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fake {
        n: usize,
        delay_ms: u64,
    }
    impl FrameProvider for Fake {
        fn frame_count(&self) -> usize {
            self.n
        }
        fn fps(&self) -> f64 {
            100.0
        }
        fn decode(&self, index: usize) -> Result<Vec<u16>, String> {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
            Ok(vec![index as u16; 16])
        }
    }

    fn wait_for<F: FnMut() -> bool>(mut f: F) -> bool {
        for _ in 0..400 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Audio clock driven by the test: `elapsed` is whatever was last set.
    #[derive(Default)]
    struct FakeAudio {
        log: Mutex<Vec<String>>,
        elapsed: Mutex<Option<f64>>,
    }
    impl AudioSync for FakeAudio {
        fn start(&self, media_secs: f64, loop_secs: Option<f64>) {
            self.log.lock().unwrap().push(format!("start {media_secs:.2} {loop_secs:?}"));
            *self.elapsed.lock().unwrap() = Some(0.0);
        }
        fn stop(&self) {
            self.log.lock().unwrap().push("stop".into());
        }
        fn elapsed(&self) -> Option<f64> {
            *self.elapsed.lock().unwrap()
        }
    }

    #[test]
    fn audio_clock_drives_the_playhead() {
        let audio = Arc::new(FakeAudio::default());
        let mut e = Engine::new(Arc::new(Fake { n: 100, delay_ms: 0 }), 2, 6);
        e.set_audio(audio.clone());
        e.seek(10.0);
        e.play();
        // Audio has not produced anything yet: the picture holds even though wall time passes.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(e.position(), 10.0);
        // 0.25 s of audio heard at 100 fps = 25 frames.
        *audio.elapsed.lock().unwrap() = Some(0.25);
        assert!((e.position() - 35.0).abs() < 1e-9);
        e.pause();
        assert!((e.position() - 35.0).abs() < 1e-9);
        let log = audio.log.lock().unwrap().clone();
        assert_eq!(log, vec!["start 0.10 Some(1.0)", "stop"]);
    }

    #[test]
    fn seek_and_loop_change_restart_audio_at_the_new_position() {
        let audio = Arc::new(FakeAudio::default());
        let mut e = Engine::new(Arc::new(Fake { n: 100, delay_ms: 0 }), 2, 6);
        e.set_audio(audio.clone());
        e.play();
        e.seek(50.0);
        *audio.elapsed.lock().unwrap() = Some(0.1);
        e.set_looping(false);
        let log = audio.log.lock().unwrap().clone();
        assert_eq!(log, vec!["start 0.00 Some(1.0)", "start 0.50 Some(1.0)", "start 0.60 None"]);
    }

    #[test]
    fn dead_audio_clock_falls_back_to_wall_clock() {
        let audio = Arc::new(FakeAudio::default());
        let mut e = Engine::new(Arc::new(Fake { n: 1000, delay_ms: 0 }), 2, 6);
        e.set_audio(audio.clone());
        e.play();
        *audio.elapsed.lock().unwrap() = None;
        std::thread::sleep(Duration::from_millis(60));
        assert!(e.position() >= 5.0, "{}", e.position()); // 100 fps wall clock kept going
    }

    #[test]
    fn seek_while_paused_delivers_exactly_that_frame() {
        let e = Engine::new(Arc::new(Fake { n: 50, delay_ms: 2 }), 3, 8);
        e.seek(20.0);
        let mut got = None;
        assert!(wait_for(|| {
            got = e.next_frame();
            got.is_some()
        }));
        let (idx, data) = got.unwrap();
        assert_eq!(idx, 20);
        assert_eq!(data[0], 20);
        // paused: same frame again is not re-delivered
        assert!(e.next_frame().is_none());
    }

    #[test]
    fn playback_advances_in_real_time_and_loops() {
        let e = Engine::new(Arc::new(Fake { n: 30, delay_ms: 1 }), 3, 12);
        e.play();
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_millis(500) {
            if let Some((i, _)) = e.next_frame() {
                seen.push(i);
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        // 100 fps for 0.5 s on a 30-frame loop: must have wrapped at least once and never gone backwards otherwise.
        assert!(seen.len() > 20, "only {} frames shown", seen.len());
        let wraps = seen.windows(2).filter(|w| w[1] < w[0]).count();
        assert!(wraps >= 1, "expected the loop to wrap: {seen:?}");
        assert!(e.stats().decoded >= 20);
    }

    #[test]
    fn non_looping_clip_stops_at_the_last_frame() {
        let e = Engine::new(Arc::new(Fake { n: 10, delay_ms: 1 }), 2, 6);
        e.set_looping(false);
        e.play();
        let start = Instant::now();
        let mut last = 0;
        while start.elapsed() < Duration::from_millis(400) {
            if let Some((i, _)) = e.next_frame() {
                last = i;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert_eq!(last, 9);
        assert!(!e.is_playing());
    }
}
