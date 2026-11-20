//! Keeps the index current: loads the on-disk cache instantly at startup,
//! rescans in the background, then follows file system changes live.

use crate::index::{Index, ScanSink, audio_ext_id, norm_lower};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::Watcher;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::time::{Duration, Instant};

type Repaint = Arc<dyn Fn() + Send + Sync>;

enum Cmd {
    Rescan,
    Roots(Vec<PathBuf>),
    /// Changed paths; `true` marks structural events (create / rename /
    /// remove) that may require rescanning a folder.
    Changed(Vec<(PathBuf, bool)>),
}

pub struct Indexer {
    current: RwLock<Option<Arc<Index>>>,
    pub version: AtomicU64,
    pub scanning: AtomicBool,
    pub scanned: AtomicUsize,
    restart: AtomicBool,
    tx: Sender<Cmd>,
}

pub fn cache_path() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("totems").join("index.bin")
}

impl Indexer {
    pub fn start(roots: Vec<PathBuf>, repaint: Repaint) -> Arc<Indexer> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let me = Arc::new(Indexer {
            current: RwLock::new(None),
            version: AtomicU64::new(0),
            scanning: AtomicBool::new(true),
            scanned: AtomicUsize::new(0),
            restart: AtomicBool::new(false),
            tx: tx.clone(),
        });
        let m = me.clone();
        std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || m.run(roots, rx, tx, repaint))
            .expect("spawn indexer");
        me
    }

    pub fn index(&self) -> Option<Arc<Index>> {
        self.current.read().clone()
    }

    pub fn rescan(&self) {
        self.restart.store(true, SeqCst);
        let _ = self.tx.send(Cmd::Rescan);
    }

    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        self.restart.store(true, SeqCst);
        let _ = self.tx.send(Cmd::Roots(roots));
    }

    fn publish(&self, ix: Index, repaint: &Repaint) {
        *self.current.write() = Some(Arc::new(ix));
        self.version.fetch_add(1, SeqCst);
        repaint();
    }

    fn run(self: Arc<Self>, mut roots: Vec<PathBuf>, rx: Receiver<Cmd>, tx: Sender<Cmd>, repaint: Repaint) {
        let cache = cache_path();
        let mut have_index = false;
        if let Some(ix) = Index::load(&cache, &roots) {
            self.publish(ix, &repaint);
            have_index = true;
        }
        let mut need_scan = true;
        let mut _watcher: Option<notify::RecommendedWatcher> = None;
        let mut changes: HashMap<PathBuf, bool> = HashMap::new();
        let mut last_change = Instant::now();
        let mut last_save = Instant::now();
        let mut dirty = false;

        loop {
            if need_scan {
                need_scan = false;
                self.restart.store(false, SeqCst);
                self.scanning.store(true, SeqCst);
                self.scanned.store(0, SeqCst);
                repaint();
                _watcher = None;
                let mut sink = ScanSink::default();
                let mut last_pub = Instant::now();
                let mut cancelled = false;
                for root in &roots {
                    let mut progress = |s: &ScanSink| {
                        self.scanned.store(s.entries.len(), Relaxed);
                        // First run: make results searchable while still scanning.
                        if !have_index && last_pub.elapsed() > Duration::from_millis(1500) {
                            last_pub = Instant::now();
                            self.publish(Index::build(s.dirs.clone(), s.entries.clone()), &repaint);
                        } else {
                            repaint();
                        }
                        !self.restart.load(Relaxed)
                    };
                    if !sink.scan(root, &mut progress) {
                        cancelled = true;
                        break;
                    }
                }
                if !cancelled {
                    let ix = Index::build(sink.dirs, sink.entries);
                    let _ = ix.save(&cache, &roots);
                    last_save = Instant::now();
                    self.publish(ix, &repaint);
                    have_index = true;
                    _watcher = start_watcher(&roots, tx.clone());
                }
                self.scanning.store(false, SeqCst);
                repaint();
            }

            let cmd = if changes.is_empty() && !dirty {
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                rx.recv_timeout(Duration::from_millis(250))
            };
            match cmd {
                Ok(Cmd::Rescan) => need_scan = true,
                Ok(Cmd::Roots(r)) => {
                    roots = r;
                    changes.clear();
                    have_index = false;
                    need_scan = true;
                }
                Ok(Cmd::Changed(paths)) => {
                    for (p, structural) in paths {
                        *changes.entry(p).or_default() |= structural;
                    }
                    last_change = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if need_scan {
                continue;
            }
            if !changes.is_empty() && last_change.elapsed() >= Duration::from_millis(200) {
                let batch: Vec<(PathBuf, bool)> = changes.drain().collect();
                if let Some(cur) = self.index()
                    && let Some(ix) = apply_changes(&cur, &batch, &roots)
                {
                    self.publish(ix, &repaint);
                    dirty = true;
                }
            }
            if dirty && changes.is_empty() && last_save.elapsed() > Duration::from_secs(10) {
                if let Some(cur) = self.index() {
                    let _ = cur.save(&cache, &roots);
                }
                last_save = Instant::now();
                dirty = false;
            }
        }
    }
}

fn start_watcher(roots: &[PathBuf], tx: Sender<Cmd>) -> Option<notify::RecommendedWatcher> {
    let cache_dir = cache_path().parent().and_then(|p| p.to_str()).map(norm_lower).unwrap_or_default();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        use notify::EventKind as K;
        use notify::event::ModifyKind;
        let structural = match ev.kind {
            K::Access(_) => return,
            K::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_)) => false,
            // A folder reported as merely "modified" means its contents
            // changed; the children get their own events.
            K::Modify(ModifyKind::Any | ModifyKind::Other) => false,
            _ => true,
        };
        let paths: Vec<(PathBuf, bool)> = ev
            .paths
            .into_iter()
            .filter(|p| p.to_str().is_some_and(|s| !norm_lower(s).starts_with(&cache_dir)))
            .map(|p| (p, structural))
            .collect();
        if !paths.is_empty() {
            let _ = tx.send(Cmd::Changed(paths));
        }
    })
    .ok()?;
    for r in roots {
        let _ = w.watch(r, notify::RecursiveMode::Recursive);
    }
    Some(w)
}

/// Apply a batch of changed paths. Returns None if nothing relevant changed.
fn apply_changes(ix: &Index, paths: &[(PathBuf, bool)], roots: &[PathBuf]) -> Option<Index> {
    let root_l: Vec<String> = roots.iter().filter_map(|r| r.to_str()).map(norm_lower).collect();
    let under_root = |p: &str| root_l.iter().any(|r| p.starts_with(r.trim_end_matches('/')));

    // Does any indexed folder equal or live under `p`? Binary search over
    // the sorted folder list: folders under p/ are contiguous.
    let mut sorted: Vec<&str> = ix.dirs_lower.iter().map(|d| &**d).collect();
    sorted.sort_unstable();
    let has_indexed_under = |p: &str| {
        let p = p.trim_end_matches('/');
        let i = sorted.partition_point(|d| *d < p);
        sorted[i..].iter().take_while(|d| d.starts_with(p)).any(|d| d.len() == p.len() || d.as_bytes()[p.len()] == b'/')
    };
    let in_skipped = |p: &Path| {
        p.components().any(|c| c.as_os_str().to_str().is_some_and(|n| crate::index::skip_dir(n, Path::new(""))))
    };

    let mut remove_files: HashSet<String> = HashSet::new(); // lower full paths
    let mut remove_trees: Vec<String> = Vec::new(); // lower dir paths
    let mut add_files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut add_trees: Vec<PathBuf> = Vec::new();

    for (p, structural) in paths {
        let Some(ps) = p.to_str() else { continue };
        let pl = norm_lower(ps);
        if !under_root(&pl) {
            continue;
        }
        let is_audio = p.file_name().and_then(|n| n.to_str()).and_then(audio_ext_id).is_some();
        if !is_audio && !structural {
            continue; // content change of a non-audio file or a folder
        }
        if in_skipped(p) {
            continue;
        }
        match std::fs::symlink_metadata(p) {
            Ok(md) if md.is_file() => {
                if is_audio && !unchanged(ix, p, &md) {
                    remove_files.insert(pl);
                    add_files.push((p.clone(), md));
                }
            }
            Ok(md) if md.is_dir() => {
                // Could be a folder moved in: (re)scan it.
                if has_indexed_under(&pl) {
                    remove_trees.push(pl);
                }
                add_trees.push(p.clone());
            }
            Ok(_) => {}
            Err(_) => {
                if is_audio {
                    remove_files.insert(pl.clone());
                }
                if has_indexed_under(&pl) {
                    remove_trees.push(pl);
                }
            }
        }
    }

    // New folders that contain no audio don't change anything.
    let mut extra = ScanSink::default();
    for t in &add_trees {
        let mut keep_going = |_: &ScanSink| true;
        extra.scan(t, &mut keep_going);
    }
    if remove_files.is_empty() && remove_trees.is_empty() && add_files.is_empty() && extra.entries.is_empty() {
        return None;
    }

    let (dirs, entries) = ix.to_raw();
    let dir_removed: Vec<bool> = ix
        .dirs_lower
        .iter()
        .map(|d| {
            remove_trees.iter().any(|t| {
                d.as_ref() == t.as_str() || (d.starts_with(t.as_str()) && d.as_bytes().get(t.len()) == Some(&b'/'))
            })
        })
        .collect();
    let before = entries.len();
    let mut sink = ScanSink::from_raw(dirs, Vec::with_capacity(before));
    for e in entries {
        if dir_removed[e.dir as usize] {
            continue;
        }
        if !remove_files.is_empty() {
            let d = &ix.dirs_lower[e.dir as usize];
            let full = format!("{}{}{}", d, if d.ends_with('/') { "" } else { "/" }, e.name.to_lowercase());
            if remove_files.contains(&full) {
                continue;
            }
        }
        sink.entries.push(e);
    }
    let removed = before - sink.entries.len();
    for (p, md) in &add_files {
        sink.add_file(p, md);
    }
    for e in extra.entries {
        let dir = sink.dir_id(Path::new(&extra.dirs[e.dir as usize]));
        sink.entries.push(crate::index::RawEntry { dir, ..e });
    }
    if removed == 0 && sink.entries.len() == before {
        return None;
    }
    Some(Index::build(sink.dirs, sink.entries))
}

fn unchanged(ix: &Index, p: &Path, md: &std::fs::Metadata) -> bool {
    ix.find(p).is_some_and(|i| ix.size[i] == md.len() && ix.mtime[i] == crate::index::mtime_of(md))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ix: &Index) -> Vec<String> {
        let mut v: Vec<String> = (0..ix.len()).map(|i| ix.full_path(i).to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn wait_for(me: &Indexer, pred: impl Fn(&Index) -> bool) -> bool {
        for _ in 0..100 {
            if let Some(ix) = me.index()
                && pred(&ix)
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn live_updates() {
        let base = std::env::temp_dir().join(format!("totems-live-{}", std::process::id()));
        let root = base.join("lib");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(outside.join("pack")).unwrap();
        // SAFETY: test-only; set before any thread reads it.
        unsafe { std::env::set_var("XDG_CACHE_HOME", base.join("cache")) };
        std::fs::write(root.join("a/one.wav"), b"x").unwrap();
        std::fs::write(root.join("a/notes.txt"), b"x").unwrap();
        std::fs::write(root.join("a/._one.wav"), b"x").unwrap(); // macOS metadata
        std::fs::create_dir_all(outside.join("pack/__MACOSX")).unwrap();
        std::fs::write(outside.join("pack/two.flac"), b"x").unwrap();
        std::fs::write(outside.join("pack/._two.flac"), b"x").unwrap();
        std::fs::write(outside.join("pack/__MACOSX/._two.flac"), b"x").unwrap();

        let me = Indexer::start(vec![root.clone()], Arc::new(|| {}));
        assert!(wait_for(&me, |ix| ix.len() == 1));
        while me.scanning.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200)); // watcher armed

        std::fs::write(root.join("a/._three.mp3"), b"x").unwrap();
        std::fs::write(root.join("a/three.mp3"), b"x").unwrap();
        assert!(wait_for(&me, |ix| ix.len() == 2), "new file");

        std::fs::rename(outside.join("pack"), root.join("pack")).unwrap();
        assert!(wait_for(&me, |ix| ix.len() == 3), "folder moved in");

        std::fs::rename(root.join("a"), root.join("b")).unwrap();
        assert!(
            wait_for(&me, |ix| names(ix).iter().all(|n| !n.contains("/a/")) && ix.len() == 3),
            "folder renamed: {:?}",
            me.index().map(|ix| names(&ix))
        );

        std::fs::remove_file(root.join("b/one.wav")).unwrap();
        assert!(wait_for(&me, |ix| ix.len() == 2), "file deleted");
        let all = names(&me.index().unwrap());
        assert!(
            all.iter().all(|n| Path::new(n)
                .components()
                .all(|c| !c.as_os_str().to_string_lossy().starts_with("._") && c.as_os_str() != "__MACOSX")),
            "macOS metadata indexed: {all:?}"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
