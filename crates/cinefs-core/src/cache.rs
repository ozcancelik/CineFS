//! Thread-safe LRU decoded payload cache with sequential access detection and background prefetching.
//!
//! When DaVinci Resolve, QuickLook, or an NLE plays back a virtual CinemaDNG sequence, it often:
//! 1. Reads a single `.dng` file in multiple OS page/chunk requests (e.g. 64 KB or 1 MB NFS READs).
//! 2. Reads frames sequentially (`N`, `N+1`, `N+2`, ...).
//!
//! Caching the decoded strip payload in an `Arc<Vec<u8>>` ensures that sub-frame chunked reads
//! decode the frame at most once, and background prefetch workers pre-populate upcoming frames
//! (`N+1 .. N+4`) across CPU cores before the NLE requests them.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub clip_id: usize,
    pub frame_index: usize,
}

struct CacheEntry {
    key: CacheKey,
    data: Arc<Vec<u8>>,
}

struct CacheInner {
    entries: VecDeque<CacheEntry>,
    in_flight: HashSet<CacheKey>,
    last_accessed_frame: Option<(usize, usize)>,
    max_bytes: usize,
    current_bytes: usize,
    hits: u64,
    misses: u64,
    prefetched: u64,
}

type PrefetchJob = Box<dyn FnOnce() + Send + 'static>;

struct PrefetchQueue {
    jobs: VecDeque<PrefetchJob>,
    shutdown: bool,
}

pub struct PayloadCache {
    inner: Mutex<CacheInner>,
    ready_cvar: Condvar,
    prefetch_queue: Arc<(Mutex<PrefetchQueue>, Condvar)>,
}

impl PayloadCache {
    /// Creates a new payload cache with `max_bytes` capacity (e.g. 256 MB) and background prefetch threads.
    pub fn new(max_bytes: usize) -> Self {
        let prefetch_queue = Arc::new((
            Mutex::new(PrefetchQueue {
                jobs: VecDeque::new(),
                shutdown: false,
            }),
            Condvar::new(),
        ));

        let worker_count = thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(2, 6))
            .unwrap_or(2);

        for i in 0..worker_count {
            let q = Arc::clone(&prefetch_queue);
            let _ = thread::Builder::new()
                .name(format!("cinefs-prefetch-{i}"))
                .spawn(move || loop {
                    let job = {
                        let (lock, cvar) = &*q;
                        let mut guard = lock.lock().unwrap();
                        while guard.jobs.is_empty() && !guard.shutdown {
                            guard = cvar.wait(guard).unwrap();
                        }
                        if guard.shutdown {
                            return;
                        }
                        guard.jobs.pop_front()
                    };
                    if let Some(task) = job {
                        task();
                    }
                });
        }

        Self {
            inner: Mutex::new(CacheInner {
                entries: VecDeque::new(),
                in_flight: HashSet::new(),
                last_accessed_frame: None,
                max_bytes,
                current_bytes: 0,
                hits: 0,
                misses: 0,
                prefetched: 0,
            }),
            ready_cvar: Condvar::new(),
            prefetch_queue,
        }
    }

    pub fn get(&self, key: CacheKey) -> Option<Arc<Vec<u8>>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(pos) = guard.entries.iter().position(|e| e.key == key) {
            let entry = guard.entries.remove(pos).unwrap();
            let arc = Arc::clone(&entry.data);
            guard.entries.push_back(entry);
            guard.hits += 1;
            Some(arc)
        } else {
            guard.misses += 1;
            None
        }
    }

    /// Atomically fetches `key` from cache, waits if another thread is currently decoding `key`,
    /// or marks `key` as `in_flight` and runs `compute` exactly once across all threads.
    pub fn get_or_try_compute<F>(&self, key: CacheKey, compute: F) -> crate::Result<Arc<Vec<u8>>>
    where
        F: FnOnce() -> crate::Result<Vec<u8>>,
    {
        {
            let mut guard = self.inner.lock().unwrap();
            loop {
                if let Some(pos) = guard.entries.iter().position(|e| e.key == key) {
                    let entry = guard.entries.remove(pos).unwrap();
                    let arc = Arc::clone(&entry.data);
                    guard.entries.push_back(entry);
                    guard.hits += 1;
                    return Ok(arc);
                }
                if !guard.in_flight.contains(&key) {
                    guard.in_flight.insert(key);
                    guard.misses += 1;
                    break;
                }
                guard = self.ready_cvar.wait(guard).unwrap();
            }
        }

        match compute() {
            Ok(vec) => {
                let arc = Arc::new(vec);
                self.insert(key, Arc::clone(&arc), false);
                Ok(arc)
            }
            Err(e) => {
                self.cancel_in_flight(key);
                Err(e)
            }
        }
    }

    pub fn contains_or_in_flight(&self, key: CacheKey) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.in_flight.contains(&key) || guard.entries.iter().any(|e| e.key == key)
    }

    /// Records frame access and returns a list of upcoming frame indices (`frame_index + 1 ..= +lookahead`)
    /// that should be prefetched if sequential forward playback is detected.
    pub fn record_access_and_plan_prefetch(
        &self,
        clip_id: usize,
        frame_index: usize,
        total_frames: usize,
        lookahead: usize,
    ) -> Vec<usize> {
        let mut guard = self.inner.lock().unwrap();
        let prev = guard.last_accessed_frame;
        guard.last_accessed_frame = Some((clip_id, frame_index));

        let is_sequential = match prev {
            Some((prev_clip, prev_frame)) if prev_clip == clip_id => {
                frame_index >= prev_frame && frame_index <= prev_frame + 2
            }
            _ => true,
        };

        if !is_sequential || guard.max_bytes == 0 {
            return Vec::new();
        }

        let mut to_prefetch = Vec::new();
        let end = (frame_index + 1 + lookahead).min(total_frames);
        for next_idx in (frame_index + 1)..end {
            let key = CacheKey {
                clip_id,
                frame_index: next_idx,
            };
            if !guard.in_flight.contains(&key) && !guard.entries.iter().any(|e| e.key == key) {
                guard.in_flight.insert(key);
                to_prefetch.push(next_idx);
            }
        }
        to_prefetch
    }

    pub fn enqueue_prefetch<F>(&self, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let (lock, cvar) = &*self.prefetch_queue;
        let mut guard = lock.lock().unwrap();
        if guard.jobs.len() < 16 {
            guard.jobs.push_back(Box::new(job));
            cvar.notify_one();
        }
    }

    pub fn cancel_in_flight(&self, key: CacheKey) {
        let mut guard = self.inner.lock().unwrap();
        guard.in_flight.remove(&key);
        self.ready_cvar.notify_all();
    }

    pub fn insert(&self, key: CacheKey, data: Arc<Vec<u8>>, is_prefetch: bool) {
        let len = data.len();
        let mut guard = self.inner.lock().unwrap();
        guard.in_flight.remove(&key);
        if len > guard.max_bytes {
            self.ready_cvar.notify_all();
            return;
        }
        if let Some(pos) = guard.entries.iter().position(|e| e.key == key) {
            if let Some(old) = guard.entries.remove(pos) {
                guard.current_bytes = guard.current_bytes.saturating_sub(old.data.len());
            }
        }
        while guard.current_bytes + len > guard.max_bytes {
            if let Some(evicted) = guard.entries.pop_front() {
                guard.current_bytes = guard.current_bytes.saturating_sub(evicted.data.len());
            } else {
                break;
            }
        }
        guard.current_bytes += len;
        if is_prefetch {
            guard.prefetched += 1;
        }
        guard.entries.push_back(CacheEntry { key, data });
        self.ready_cvar.notify_all();
    }

    pub fn stats(&self) -> (u64, u64, u64, usize) {
        let guard = self.inner.lock().unwrap();
        (guard.hits, guard.misses, guard.prefetched, guard.current_bytes)
    }
}

impl Drop for PayloadCache {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.prefetch_queue;
        if let Ok(mut guard) = lock.lock() {
            guard.shutdown = true;
            guard.jobs.clear();
            cvar.notify_all();
        }
    }
}
