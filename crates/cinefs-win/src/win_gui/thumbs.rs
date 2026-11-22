//! Clip previews for the Windows clip list, matching the macOS app: every clip row shows the
//! clip's middle frame, and moving the mouse across that preview scrubs through the clip.
//!
//! The images come from `cinefs_core::thumbnail` (the same code the Mac app uses). Rendering one
//! takes ~50-150 ms, so it never happens on the UI thread:
//! - list previews: queued in order and rendered one by one by a worker thread (the core caches
//!   each clip's preview, so it is rendered once per mount);
//! - scrub frames: quantised to [`SCRUB_BUCKETS`] positions per clip and cached for the clip under
//!   the mouse; a second worker renders only the most recent request, so fast mouse moves never
//!   build up a backlog.
//! When an image is ready the worker repaints the main window.

use super::*;
use cinefs_core::thumbnail::Thumbnail;
use cinefs_core::vfs::MountedClip;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Weak};
use std::time::Duration;

/// Width the previews are rendered at (same as the Mac app); they are scaled down to the slot.
const PREVIEW_MAX_W: usize = 320;
/// Scrub positions per clip (same as the Mac app).
const SCRUB_BUCKETS: usize = 48;

/// Identifies one clip row: (mount id, clip index inside that mount).
pub type ClipKey = (u64, usize);

pub enum Preview {
    Image(Arc<Thumbnail>),
    Loading,
    Failed,
}

static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);

/// Window repainted when a preview becomes ready.
pub fn set_main_window(hwnd: *mut c_void) {
    MAIN_HWND.store(hwnd as usize, Ordering::Relaxed);
}

fn repaint_main_window() {
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            InvalidateRect(h as *mut c_void, ptr::null(), 0);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------
// List previews (middle frame)
// ---------------------------------------------------------------------------------------------

enum Slot {
    Pending,
    Ready(Arc<Thumbnail>),
    Failed,
}

#[derive(Default)]
struct ListState {
    slots: HashMap<ClipKey, Slot>,
    queue: VecDeque<(ClipKey, Weak<MountedClip>)>,
    worker_running: bool,
}

static LIST: LazyLock<Mutex<ListState>> = LazyLock::new(|| Mutex::new(ListState::default()));

/// Preview for a clip row. The first call queues it for rendering and returns `Loading`.
pub fn list_preview(key: ClipKey, clip: &Arc<MountedClip>) -> Preview {
    let mut st = lock(&LIST);
    match st.slots.get(&key) {
        Some(Slot::Ready(t)) => return Preview::Image(Arc::clone(t)),
        Some(Slot::Failed) => return Preview::Failed,
        Some(Slot::Pending) => return Preview::Loading,
        None => {}
    }
    st.slots.insert(key, Slot::Pending);
    // Weak: a clip unmounted while waiting in the queue is simply skipped (and its file is not
    // kept open by the queue).
    st.queue.push_back((key, Arc::downgrade(clip)));
    if !st.worker_running {
        st.worker_running = true;
        std::thread::spawn(list_worker);
    }
    Preview::Loading
}

fn list_worker() {
    loop {
        let (key, weak) = {
            let mut st = lock(&LIST);
            match st.queue.pop_front() {
                Some(job) => job,
                None => {
                    st.worker_running = false;
                    return;
                }
            }
        };
        let Some(clip) = weak.upgrade() else {
            continue; // unmounted meanwhile
        };
        let rendered = catch_unwind(AssertUnwindSafe(|| clip.thumbnail(PREVIEW_MAX_W))).ok().flatten();
        drop(clip);
        {
            let mut st = lock(&LIST);
            // The entry is gone if the mount was removed while this was rendering.
            if st.slots.contains_key(&key) {
                st.slots.insert(key, rendered.map_or(Slot::Failed, Slot::Ready));
            }
        }
        repaint_main_window();
    }
}

// ---------------------------------------------------------------------------------------------
// Hover scrubbing
// ---------------------------------------------------------------------------------------------

struct ScrubRequest {
    key: ClipKey,
    bucket: usize,
    frame: usize,
    clip: Weak<MountedClip>,
}

#[derive(Default)]
struct ScrubState {
    /// Clip whose frames are in `cache` (only the clip under the mouse is cached).
    clip: Option<ClipKey>,
    cache: HashMap<usize, Arc<Thumbnail>>,
    /// Newest request; older ones are overwritten before the worker gets to them.
    wanted: Option<ScrubRequest>,
    worker_running: bool,
}

static SCRUB: LazyLock<(Mutex<ScrubState>, Condvar)> =
    LazyLock::new(|| (Mutex::new(ScrubState::default()), Condvar::new()));

/// Scrub position (0..number of buckets) for a mouse position `frac` (0..1) across the preview.
pub fn scrub_bucket(frac: f32, frame_count: usize) -> usize {
    let n = frame_count.clamp(1, SCRUB_BUCKETS);
    ((frac.clamp(0.0, 1.0) * n as f32) as usize).min(n - 1)
}

/// Frame shown for scrub position `bucket` of a clip with `frame_count` frames.
fn bucket_frame(bucket: usize, frame_count: usize) -> usize {
    let n = frame_count.clamp(1, SCRUB_BUCKETS);
    if n > 1 { bucket * (frame_count - 1) / (n - 1) } else { 0 }
}

/// Frame to show while the mouse is at `frac` (0..1) across the preview of `clip`: the cached
/// image if it is ready, otherwise `None` (the request is queued; the window is repainted when
/// the frame is ready, and the caller keeps showing the list preview meanwhile).
pub fn scrub_frame(key: ClipKey, clip: &Arc<MountedClip>, frac: f32) -> Option<Arc<Thumbnail>> {
    let frames = clip.clip.frames.len();
    if frames == 0 {
        return None;
    }
    let bucket = scrub_bucket(frac, frames);
    let (lock_m, cv) = &*SCRUB;
    let mut st = lock(lock_m);
    if st.clip != Some(key) {
        st.clip = Some(key);
        st.cache.clear();
    }
    if let Some(t) = st.cache.get(&bucket) {
        return Some(Arc::clone(t));
    }
    let frame = bucket_frame(bucket, frames);
    st.wanted = Some(ScrubRequest { key, bucket, frame, clip: Arc::downgrade(clip) });
    if !st.worker_running {
        st.worker_running = true;
        std::thread::spawn(scrub_worker);
    }
    cv.notify_one();
    None
}

/// The mouse left the preview: drop any pending request.
pub fn end_scrub() {
    let (lock_m, _) = &*SCRUB;
    lock(lock_m).wanted = None;
}

fn scrub_worker() {
    let (lock_m, cv) = &*SCRUB;
    loop {
        let req = {
            let mut st = lock(lock_m);
            loop {
                if let Some(r) = st.wanted.take() {
                    break r;
                }
                let (guard, timeout) = cv
                    .wait_timeout(st, Duration::from_secs(30))
                    .unwrap_or_else(|e| e.into_inner());
                st = guard;
                if timeout.timed_out() && st.wanted.is_none() {
                    st.worker_running = false; // idle: let the thread end
                    return;
                }
            }
        };
        let Some(clip) = req.clip.upgrade() else {
            continue;
        };
        let rendered = catch_unwind(AssertUnwindSafe(|| {
            cinefs_core::thumbnail::render_frame(&clip.clip, req.frame, PREVIEW_MAX_W).ok()
        }))
        .ok()
        .flatten();
        drop(clip);
        if let Some(t) = rendered {
            let mut st = lock(lock_m);
            if st.clip == Some(req.key) {
                st.cache.insert(req.bucket, Arc::new(t));
            }
        }
        repaint_main_window();
    }
}

// ---------------------------------------------------------------------------------------------

/// Forgets previews of mounts that no longer exist (called on every repaint with the live ids).
pub fn retain_mounts(live: &[u64]) {
    {
        let mut st = lock(&LIST);
        st.slots.retain(|k, _| live.contains(&k.0));
        st.queue.retain(|(k, _)| live.contains(&k.0));
    }
    let (lock_m, _) = &*SCRUB;
    let mut st = lock(lock_m);
    if st.clip.is_some_and(|k| !live.contains(&k.0)) {
        st.clip = None;
        st.cache.clear();
        st.wanted = None;
    }
}
