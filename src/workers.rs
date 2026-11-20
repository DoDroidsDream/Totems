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

/// A file's (size, mtime) from the index. Cached info and waveforms are
/// only good for the stamp they were made from: a file rewritten in place
/// (a re-rendered take) gets probed and drawn again.
pub type Stamp = (u64, i64);

// ------------------------------------------------------------------ info

/// Probed files kept (~200 B each); the oldest are dropped first. Far more
/// than any screen of rows, so only long scroll sessions ever re-probe.
const INFO_CAP: usize = 20_000;

#[derive(Default)]
struct InfoState {
    done: HashMap<PathBuf, (Stamp, Option<Info>)>,
    /// Keys of `done` in insertion order, for eviction.
    order: VecDeque<PathBuf>,
    queue: VecDeque<(PathBuf, Stamp)>,
    queued: HashSet<PathBuf>,
}

impl InfoState {
    fn insert(&mut self, path: PathBuf, stamp: Stamp, info: Option<Info>) {
        if self.done.insert(path.clone(), (stamp, info)).is_none() {
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
                        let (path, stamp) = {
                            let mut s = me.state.lock();
                            loop {
                                if let Some((p, stamp)) = s.queue.pop_back() {
                                    s.queued.remove(&p);
                                    break (p, stamp);
                                }
                                me.cv.wait(&mut s);
                            }
                        };
                        let info = Source::open(&path).ok().map(|s| s.info);
                        me.state.lock().insert(path, stamp, info);
                        repaint();
                    }
                })
                .expect("spawn info worker");
        }
        me
    }

    /// `None` while unknown (and queues a probe); `Some(None)` if unreadable.
    pub fn get(&self, path: &Path, stamp: Stamp) -> Option<Option<Info>> {
        let mut s = self.state.lock();
        if let Some((st, i)) = s.done.get(path)
            && *st == stamp
        {
            return Some(i.clone());
        }
        if !s.queued.contains(path) {
            s.queued.insert(path.to_path_buf());
            s.queue.push_back((path.to_path_buf(), stamp));
            while s.queue.len() > 256 {
                if let Some((old, _)) = s.queue.pop_front() {
                    s.queued.remove(&old);
                }
            }
            self.cv.notify_one();
        }
        None
    }

    /// Record info the player already has, unless this file is known.
    pub fn put(&self, path: &Path, stamp: Stamp, info: &Info) {
        let mut s = self.state.lock();
        if !matches!(s.done.get(path), Some((st, Some(_))) if *st == stamp) {
            s.insert(path.to_path_buf(), stamp, Some(info.clone()));
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
    cache: Mutex<VecDeque<(PathBuf, Stamp, Arc<Wave>)>>,
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
                // Drop an unfinished wave from the cache, so asking for its
                // file again starts it afresh.
                let forget = |wave: &Arc<Wave>| {
                    if let Some(me) = weak.upgrade() {
                        me.cache.lock().retain(|(_, _, w)| !Arc::ptr_eq(w, wave));
                    }
                };
                while let Ok(mut job) = rx.recv() {
                    // Latest request wins; the skipped ones are forgotten
                    // (B33: they stayed cached, empty, never computed).
                    while let Ok(j) = rx.try_recv() {
                        forget(&job.1);
                        job = j;
                    }
                    let (path, wave) = job;
                    if !compute(&path, &wave, &rx, &repaint) {
                        forget(&wave); // cancelled midway
                    }
                    repaint();
                }
            })
            .expect("spawn waveform worker");
        me
    }

    pub fn get(&self, path: &Path, stamp: Stamp) -> Arc<Wave> {
        let mut c = self.cache.lock();
        if let Some(i) = c.iter().position(|(p, _, _)| p == path) {
            let item = c.remove(i).unwrap();
            if item.1 == stamp {
                let w = item.2.clone();
                c.push_back(item);
                return w;
            }
        }
        let w = Arc::new(Wave {
            peaks: RwLock::new(Vec::new()),
            block: RwLock::new(256),
            expected_blocks: RwLock::new(None),
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        c.push_back((path.to_path_buf(), stamp, w.clone()));
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

/// (min, max) of `x`, and of 0: a block's range always includes silence.
/// Written as 16 lanes of compare-and-select so it vectorises; NaN is
/// ignored, as `f32::min` / `f32::max` do.
fn min_max(x: &[f32]) -> (f32, f32) {
    let (chunks, tail) = x.as_chunks::<16>();
    let (mut lo, mut hi) = ([0f32; 16], [0f32; 16]);
    for c in chunks {
        for j in 0..16 {
            lo[j] = if c[j] < lo[j] { c[j] } else { lo[j] };
            hi[j] = if c[j] > hi[j] { c[j] } else { hi[j] };
        }
    }
    let (mut l, mut h) = (0f32, 0f32);
    for &v in lo.iter().chain(tail) {
        l = l.min(v);
    }
    for &v in hi.iter().chain(tail) {
        h = h.max(v);
    }
    (l, h)
}

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
        let mut rest = &buf[..buf.len() / ch * ch];
        while !rest.is_empty() {
            let take = (block - n).min(rest.len() / ch);
            let (seg, tail) = rest.split_at(take * ch);
            rest = tail;
            let (a, b) = min_max(seg);
            lo = lo.min(a);
            hi = hi.max(b);
            n += take;
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
        if packets.is_multiple_of(32) {
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
            c.put(&p(i), (1, 1), &Info::default());
        }
        // Re-putting a known file neither duplicates nor reorders it.
        c.put(&p(INFO_CAP + 9), (1, 1), &Info::default());
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

    /// Waveform cost vs plain decoding: `TOTEMS_WAVE=file cargo test
    /// --release bench_waveform -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_waveform() {
        let p = PathBuf::from(std::env::var("TOTEMS_WAVE").expect("set TOTEMS_WAVE"));
        let _ = std::fs::read(&p);
        let t = std::time::Instant::now();
        let mut s = Source::open(&p).unwrap();
        let mut buf = Vec::new();
        let mut n = 0;
        while s.next(&mut buf).is_some() {
            n += buf.len();
        }
        let decode = t.elapsed();
        let wave = Wave {
            peaks: RwLock::new(Vec::new()),
            block: RwLock::new(256),
            expected_blocks: RwLock::new(None),
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        let (_tx, rx) = crossbeam_channel::unbounded();
        let repaint: Repaint = Arc::new(|| {});
        let t = std::time::Instant::now();
        assert!(compute(&p, &wave, &rx, &repaint));
        println!("decode only {decode:?} ({n} samples); waveform {:?}, {} peaks", t.elapsed(), wave.peaks.read().len());
    }

    #[test]
    fn min_max_matches_naive() {
        let x: Vec<f32> = (0..1000).map(|i| ((i * 7919) % 2001) as f32 / 1000.0 - 1.0).collect();
        for n in [0, 1, 15, 16, 17, 999, 1000] {
            let s = &x[..n];
            let naive = s.iter().fold((0f32, 0f32), |(l, h), &v| (l.min(v), h.max(v)));
            assert_eq!(min_max(s), naive, "{n}");
        }
        assert_eq!(min_max(&[f32::NAN, 0.5, -0.25]), (-0.25, 0.5));
    }

    fn saw_wav(path: &Path, secs: f64) {
        let n = (48000.0 * secs) as usize;
        let data = (n * 2) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&48000u32.to_le_bytes());
        b.extend_from_slice(&96000u32.to_le_bytes());
        for v in [2u16, 16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            b.extend_from_slice(&(((i % 100) as i16 - 50) * 300).to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    fn wait_done(w: &Wave, secs: u64) -> bool {
        let t = std::time::Instant::now();
        while !w.done.load(Ordering::Acquire) && t.elapsed() < std::time::Duration::from_secs(secs) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        w.done.load(Ordering::Acquire)
    }

    fn fresh() -> Arc<Wave> {
        Arc::new(Wave {
            peaks: RwLock::new(Vec::new()),
            block: RwLock::new(256),
            expected_blocks: RwLock::new(None),
            done: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        })
    }

    /// B33: a waveform request skipped by "latest wins" stayed cached,
    /// empty and never computed: going back to that file showed nothing.
    #[test]
    fn skipped_waveform_requests_are_computed_later() {
        let d = std::env::temp_dir().join(format!("totems-waves-skip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b, c) = (d.join("a.wav"), d.join("b.wav"), d.join("c.wav"));
        saw_wav(&a, 600.0);
        saw_wav(&b, 1.0);
        saw_wav(&c, 1.0);
        let waves = Waves::new(Arc::new(|| {}));
        let wa = waves.get(&a, (1, 1));
        while wa.expected_blocks.read().is_none() {
            std::thread::yield_now();
        }
        let wc = fresh();
        {
            // What get(B) then get(C) do; holding the lock pins the worker
            // inside A's cancel check, so both land in one drain.
            let mut cache = waves.cache.lock();
            let wb = fresh();
            cache.push_back((b.clone(), (1, 1), wb.clone()));
            waves.tx.send((b.clone(), wb)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            cache.push_back((c.clone(), (1, 1), wc.clone()));
            waves.tx.send((c.clone(), wc.clone())).unwrap();
        }
        assert!(wait_done(&wc, 5));
        let t = std::time::Instant::now();
        let mut done = false;
        while !done && t.elapsed() < std::time::Duration::from_secs(3) {
            done = waves.get(&b, (1, 1)).done.load(Ordering::Acquire);
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let _ = std::fs::remove_dir_all(d);
        assert!(done, "B's waveform never computed");
    }

    /// B34: a file rewritten in place (new size/mtime in the index) kept its
    /// old waveform and Length.
    #[test]
    fn rewritten_file_gets_new_waveform_and_length() {
        let d = std::env::temp_dir().join(format!("totems-stale-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("render.wav");
        saw_wav(&a, 1.0);
        let waves = Waves::new(Arc::new(|| {}));
        let w1 = waves.get(&a, (96044, 1));
        assert!(wait_done(&w1, 3));
        let infos = InfoCache::new(1, Arc::new(|| {}));
        let probe = |stamp: Stamp| {
            let t = std::time::Instant::now();
            loop {
                if let Some(i) = infos.get(&a, stamp) {
                    return i.and_then(|i| i.duration);
                }
                assert!(t.elapsed() < std::time::Duration::from_secs(3));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        assert_eq!(probe((96044, 1)), Some(1.0));
        saw_wav(&a, 4.0);
        let w2 = waves.get(&a, (384044, 2));
        assert!(wait_done(&w2, 3));
        let (n1, n2) = (w1.peaks.read().len(), w2.peaks.read().len());
        let after = probe((384044, 2));
        let _ = std::fs::remove_dir_all(d);
        assert_ne!(n2, n1, "old waveform served for a rewritten file");
        assert_eq!(after, Some(4.0), "Length column keeps the old duration");
    }
}
