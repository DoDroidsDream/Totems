//! Background workers: per-file header info (for the Length column) and
//! waveform overviews. Both favour the most recent request so the UI stays
//! responsive while scrolling.

use crate::decode::{Info, Source};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

type Repaint = Arc<dyn Fn() + Send + Sync>;

// ------------------------------------------------------------------ info

/// Probed files kept (~200 B each); the oldest are dropped first. Far more
/// than any screen of rows, so only long scroll sessions ever re-probe.
const INFO_CAP: usize = 20_000;

#[derive(Default)]
struct InfoState {
    done: HashMap<PathBuf, Option<Info>>,
    /// Keys of `done` in insertion order, for eviction.
    order: VecDeque<PathBuf>,
    queue: VecDeque<PathBuf>,
    queued: HashSet<PathBuf>,
}

impl InfoState {
    fn insert(&mut self, path: PathBuf, info: Option<Info>) {
        if self.done.insert(path.clone(), info).is_none() {
            self.order.push_back(path);
            while self.order.len() > INFO_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.done.remove(&old);
                }
            }
        }
    }
}

pub struct InfoCache {
    state: Mutex<InfoState>,
    cv: Condvar,
}

impl InfoCache {
    pub fn new(threads: usize, repaint: Repaint) -> Arc<Self> {
        let me = Arc::new(Self { state: Mutex::new(InfoState::default()), cv: Condvar::new() });
        for i in 0..threads {
            let me = me.clone();
            let repaint = repaint.clone();
            std::thread::Builder::new()
                .name(format!("info{i}"))
                .spawn(move || {
                    loop {
                        let path = {
                            let mut s = me.state.lock();
                            loop {
                                if let Some(p) = s.queue.pop_back() {
                                    s.queued.remove(&p);
                                    break p;
                                }
                                me.cv.wait(&mut s);
                            }
                        };
                        let info = Source::open(&path).ok().map(|s| s.info);
                        me.state.lock().insert(path, info);
                        repaint();
                    }
                })
                .expect("spawn info worker");
        }
        me
    }

    /// `None` while unknown (and queues a probe); `Some(None)` if unreadable.
    pub fn get(&self, path: &Path) -> Option<Option<Info>> {
        let mut s = self.state.lock();
        if let Some(i) = s.done.get(path) {
            return Some(i.clone());
        }
        if !s.queued.contains(path) {
            s.queued.insert(path.to_path_buf());
            s.queue.push_back(path.to_path_buf());
            while s.queue.len() > 256 {
                if let Some(old) = s.queue.pop_front() {
                    s.queued.remove(&old);
                }
            }
            self.cv.notify_one();
        }
        None
    }

    /// Record info the player already has, unless this file is known.
    pub fn put(&self, path: &Path, info: &Info) {
        let mut s = self.state.lock();
        if !matches!(s.done.get(path), Some(Some(_))) {
            s.insert(path.to_path_buf(), Some(info.clone()));
        }
    }

}

// -------------------------------------------------------------- waveform

pub struct Wave {
    /// (min, max) per block of `block` frames.
    pub peaks: RwLock<Vec<(f32, f32)>>,
    pub block: RwLock<usize>,
    pub expected_blocks: RwLock<Option<usize>>,
    pub done: AtomicBool,
    pub failed: AtomicBool,
}

pub struct Waves {
    cache: Mutex<VecDeque<(PathBuf, Arc<Wave>)>>,
    tx: crossbeam_channel::Sender<(PathBuf, Arc<Wave>)>,
}

impl Waves {
    pub fn new(repaint: Repaint) -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<(PathBuf, Arc<Wave>)>();
        let me = Arc::new(Self { cache: Mutex::new(VecDeque::new()), tx });
        let weak = Arc::downgrade(&me);
        std::thread::Builder::new()
            .name("waveform".into())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    // Latest request wins.
                    while let Ok(j) = rx.try_recv() {
                        job = j;
                    }
                    let (path, wave) = job;
                    let complete = compute(&path, &wave, &rx, &repaint);
                    if !complete
                        && let Some(me) = weak.upgrade()
                    {
                        // Cancelled midway: drop it so it's recomputed next time.
                        me.cache.lock().retain(|(p, w)| !(p == &path && Arc::ptr_eq(w, &wave)));
                    }
                    repaint();
                }
            })
            .expect("spawn waveform worker");
        me
    }

    pub fn get(&self, path: &Path) -> Arc<Wave> {
        let mut c = self.cache.lock();
        if let Some(i) = c.iter().position(|(p, _)| p == path) {
            let item = c.remove(i).unwrap();
            let w = item.1.clone();
            c.push_back(item);
            return w;
        }
        let w = Arc::new(Wave {
            peaks: RwLock::new(Vec::new()),
            block: RwLock::new(256),
            expected_blocks: RwLock::new(None),
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        c.push_back((path.to_path_buf(), w.clone()));
        while c.len() > 64 {
            c.pop_front();
        }
        let _ = self.tx.send((path.to_path_buf(), w.clone()));
        w
    }
}

/// Most peaks a wave holds. The block size comes from the header's frame
/// count (~4096 blocks), but a header can lie: past this, pairs of peaks
/// merge and the block doubles, so memory and per-frame drawing stay bounded.
const MAX_PEAKS: usize = 8192;

/// Returns false if cancelled by a newer request.
fn compute(
    path: &Path,
    wave: &Wave,
    rx: &crossbeam_channel::Receiver<(PathBuf, Arc<Wave>)>,
    repaint: &Repaint,
) -> bool {
    let Ok(mut src) = Source::open(path) else {
        wave.failed.store(true, Ordering::Release);
        wave.done.store(true, Ordering::Release);
        return true;
    };
    let mut block = match src.info.frames {
        Some(f) if f > 0 => ((f / 4096) as usize).max(16),
        _ => 256,
    };
    *wave.block.write() = block;
    *wave.expected_blocks.write() = src.info.frames.map(|f| (f as usize).div_ceil(block));
    let mut buf = Vec::new();
    let mut local: Vec<(f32, f32)> = Vec::new();
    let (mut lo, mut hi, mut n) = (0f32, 0f32, 0usize);
    let mut packets = 0usize;
    let mut published = 0usize;
    let mut last_flush = std::time::Instant::now();
    while let Some(ch) = src.next(&mut buf) {
        for f in buf.chunks_exact(ch) {
            for &s in f {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            n += 1;
            if n == block {
                local.push((lo, hi));
                lo = 0.0;
                hi = 0.0;
                n = 0;
                if published + local.len() >= MAX_PEAKS {
                    // The header undercounted: halve the resolution.
                    let mut p = wave.peaks.write();
                    p.extend(local.drain(..));
                    if p.len() % 2 == 1 {
                        // The odd last block carries on as half a new one.
                        (lo, hi) = p.pop().unwrap_or_default();
                        n = block;
                    }
                    let merged: Vec<(f32, f32)> =
                        p.chunks_exact(2).map(|c| (c[0].0.min(c[1].0), c[0].1.max(c[1].1))).collect();
                    *p = merged;
                    published = p.len();
                    block *= 2;
                    *wave.block.write() = block;
                    *wave.expected_blocks.write() = None; // header was wrong
                }
            }
        }
        packets += 1;
        if packets % 32 == 0 {
            if !rx.is_empty() {
                return false;
            }
            if last_flush.elapsed().as_millis() > 40 {
                published += local.len();
                wave.peaks.write().extend(local.drain(..));
                last_flush = std::time::Instant::now();
                repaint();
            }
        }
    }
    if n > 0 {
        local.push((lo, hi));
    }
    let mut p = wave.peaks.write();
    p.extend(local);
    *wave.expected_blocks.write() = Some(p.len());
    wave.done.store(true, Ordering::Release);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_cache_is_capped_oldest_first() {
        let c = InfoCache::new(0, Arc::new(|| {}));
        let p = |i: usize| PathBuf::from(format!("/x/{i}.wav"));
        for i in 0..INFO_CAP + 10 {
            c.put(&p(i), &Info::default());
        }
        // Re-putting a known file neither duplicates nor reorders it.
        c.put(&p(INFO_CAP + 9), &Info::default());
        let s = c.state.lock();
        assert_eq!(s.done.len(), INFO_CAP);
        assert_eq!(s.order.len(), INFO_CAP);
        assert!(!s.done.contains_key(&p(9)));
        assert!(s.done.contains_key(&p(10)));
        assert!(s.done.contains_key(&p(INFO_CAP + 9)));
    }

    /// B20: the peak count came from the header's frame count, so a FLAC
    /// claiming 16 frames gave block 16 and 360k peaks for 2 minutes (612 MB
    /// for 6 hours), all walked by the UI every frame. Needs ffmpeg.
    #[test]
    fn waveform_size_ignores_lying_frame_count() {
        let p = std::env::temp_dir().join(format!("totems-wave-lie-{}.flac", std::process::id()));
        let ok = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "sine=f=100:r=48000:d=120", "-ac", "1", "-c:a", "flac"])
            .arg(&p)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("ffmpeg not found; skipping");
            return;
        }
        let mut b = std::fs::read(&p).unwrap(); // STREAMINFO total_samples: low 36 bits of bytes 18..26
        let mut v = u64::from_be_bytes(b[18..26].try_into().unwrap());
        v = (v & !((1u64 << 36) - 1)) | 16;
        b[18..26].copy_from_slice(&v.to_be_bytes());
        std::fs::write(&p, b).unwrap();
        let wave = Wave {
            peaks: RwLock::new(Vec::new()),
            block: RwLock::new(256),
            expected_blocks: RwLock::new(None),
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let (_tx, rx) = crossbeam_channel::unbounded();
        let repaint: Repaint = Arc::new(|| {});
        assert!(compute(&p, &wave, &rx, &repaint));
        let peaks = wave.peaks.read();
        let block = *wave.block.read();
        assert!(peaks.len() <= MAX_PEAKS, "{} peaks for 2 min of audio (block {block})", peaks.len());
        // Still the whole file, at the published block size.
        assert_eq!(peaks.len(), (120 * 48000usize).div_ceil(block));
        assert_eq!(*wave.expected_blocks.read(), Some(peaks.len()));
        // ffmpeg's sine is at 1/8 scale; every whole block spans a period.
        assert!(peaks[..peaks.len() - 1].iter().all(|&(lo, hi)| lo < -0.12 && hi > 0.12));
        let _ = std::fs::remove_file(&p);
    }
}
