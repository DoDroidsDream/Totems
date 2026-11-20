//! Keeps the index current: loads the on-disk cache instantly at startup,
//! rescans in the background, then follows file system changes live.

use crate::index::{Index, ScanSink, audio_ext_id, norm_lower};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::Watcher;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
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
    if cfg!(test) {
        // Tests must never overwrite the user's real index (B11).
        return std::env::temp_dir().join(format!("totems-test-cache-{}", std::process::id())).join("index.bin");
    }
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
        // When the oldest unapplied change arrived: a long copy keeps
        // resetting `last_change`, but the index still updates every 2 s.
        let mut first_change: Option<Instant> = None;
        // Roots that don't exist (deleted, drive unplugged): polled, and
        // rescanned and watched again when they come back.
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut last_save = Instant::now();
        let mut dirty = false;

        loop {
            if need_scan {
                need_scan = false;
                self.restart.store(false, SeqCst);
                self.scanning.store(true, SeqCst);
                self.scanned.store(0, SeqCst);
                repaint();
                // Watch before scanning: changes made during a long first
                // scan queue up and are applied once it's done.
                _watcher = start_watcher(&roots, tx.clone());
                missing = roots.iter().filter(|r| !r.exists()).cloned().collect();
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
                }
                self.scanning.store(false, SeqCst);
                repaint();
            }

            let cmd = if !changes.is_empty() || dirty {
                rx.recv_timeout(Duration::from_millis(250))
            } else if !missing.is_empty() {
                rx.recv_timeout(Duration::from_secs(2))
            } else {
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };
            match cmd {
                Ok(Cmd::Rescan) => need_scan = true,
                Ok(Cmd::Roots(r)) => {
                    roots = r;
                    changes.clear();
                    first_change = None;
                    have_index = false;
                    need_scan = true;
                }
                Ok(Cmd::Changed(paths)) => {
                    for (p, structural) in paths {
                        *changes.entry(p).or_default() |= structural;
                    }
                    last_change = Instant::now();
                    first_change.get_or_insert(last_change);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if need_scan {
                continue;
            }
            let back: Vec<PathBuf> = missing.iter().filter(|r| r.exists()).cloned().collect();
            if !back.is_empty() {
                missing.retain(|r| !back.contains(r));
                _watcher = start_watcher(&roots, tx.clone());
                first_change = Some(Instant::now() - Duration::from_secs(2)); // apply now
                changes.extend(back.into_iter().map(|r| (r, true)));
            }
            let quiet = last_change.elapsed() >= Duration::from_millis(200);
            let overdue = first_change.is_some_and(|t| t.elapsed() >= Duration::from_secs(2));
            if !changes.is_empty() && (quiet || overdue) {
                first_change = None;
                let batch: Vec<(PathBuf, bool)> = changes.drain().collect();
                if batch.iter().any(|(_, structural)| *structural) {
                    // A root deleted (or its drive unplugged) is polled until
                    // it comes back; its watch is dead.
                    for r in &roots {
                        if !r.exists() && !missing.contains(r) {
                            missing.push(r.clone());
                        }
                    }
                }
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

/// On-disk spelling of event paths. Windows reports a case-only rename with
/// both spellings, and both exist; listing the parent tells which is real.
/// Each parent is listed once per batch.
#[derive(Default)]
struct Spelling {
    dirs: HashMap<PathBuf, Option<(HashSet<OsString>, HashMap<String, Option<OsString>>)>>,
}

impl Spelling {
    fn on_disk(&mut self, p: &Path) -> PathBuf {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return p.to_path_buf() };
        let listing = self.dirs.entry(parent.to_path_buf()).or_insert_with(|| {
            let rd = std::fs::read_dir(parent).ok()?;
            let mut exact = HashSet::new();
            let mut lower: HashMap<String, Option<OsString>> = HashMap::new();
            for e in rd.flatten() {
                let n = e.file_name();
                if let Some(l) = n.to_str().map(str::to_lowercase) {
                    // Two names differing only by case: ambiguous.
                    lower.entry(l).and_modify(|v| *v = None).or_insert_with(|| Some(n.clone()));
                }
                exact.insert(n);
            }
            Some((exact, lower))
        });
        let Some((exact, lower)) = listing else { return p.to_path_buf() };
        if exact.contains(name) {
            return p.to_path_buf();
        }
        match name.to_str().and_then(|n| lower.get(&n.to_lowercase())) {
            Some(Some(real)) => parent.join(real),
            _ => p.to_path_buf(),
        }
    }
}

/// Apply a batch of changed paths. Returns None if nothing relevant changed.
fn apply_changes(ix: &Index, paths: &[(PathBuf, bool)], roots: &[PathBuf]) -> Option<Index> {
    let root_l: Vec<String> =
        roots.iter().map(|r| r.to_str().map(norm_lower).unwrap_or_default().trim_end_matches('/').to_string()).collect();
    // The root `p` (lower-cased) lies in: equal to it, or below it at a '/'.
    let root_of = |p: &str| {
        root_l
            .iter()
            .enumerate()
            .filter(|(_, r)| p == r.as_str() || (p.starts_with(r.as_str()) && p.as_bytes().get(r.len()) == Some(&b'/')))
            .max_by_key(|(_, r)| r.len())
            .map(|(i, _)| i)
    };

    // Does any indexed folder equal or live under `p`? Binary search over
    // the sorted folder list: folders under p/ are contiguous.
    let mut sorted: Vec<&str> = ix.dirs_lower.iter().map(|d| &**d).collect();
    sorted.sort_unstable();
    let has_indexed_under = |p: &str| {
        let p = p.trim_end_matches('/');
        let i = sorted.partition_point(|d| *d < p);
        sorted[i..].iter().take_while(|d| d.starts_with(p)).any(|d| d.len() == p.len() || d.as_bytes()[p.len()] == b'/')
    };
    // Only folders below the root count: a root may itself sit inside a
    // folder named like a skipped one (D:\Installer\Samples).
    let in_skipped = |p: &Path, root: &Path| {
        let mut parent = root.to_path_buf();
        for c in p.components().skip(root.components().count()) {
            if c.as_os_str().to_str().is_some_and(|n| crate::index::skip_dir(n, &parent)) {
                return true;
            }
            parent.push(c);
        }
        false
    };

    let mut remove_ids: HashSet<usize> = HashSet::new();
    let mut remove_trees: Vec<String> = Vec::new(); // lower dir paths
    let mut add_files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut add_trees: Vec<PathBuf> = Vec::new();
    let mut spelling = Spelling::default();

    for (p, structural) in paths {
        let Some(ps) = p.to_str() else { continue };
        let pl = norm_lower(ps);
        let Some(ri) = root_of(&pl) else { continue };
        let name = p.file_name().and_then(|n| n.to_str());
        let is_audio = name.and_then(audio_ext_id).is_some();
        if !is_audio && !structural {
            continue; // content change of a non-audio file or a folder
        }
        if in_skipped(p, &roots[ri]) {
            continue;
        }
        match std::fs::symlink_metadata(p) {
            Ok(md) if md.is_file() => {
                if !is_audio {
                    continue;
                }
                let p = spelling.on_disk(p);
                let real = p.file_name().and_then(|n| n.to_str());
                let found = ix.find(&p);
                let same = found.is_some_and(|i| {
                    Some(ix.name(i)) == real && ix.size[i] == md.len() && ix.mtime[i] == crate::index::mtime_of(&md)
                });
                if !same {
                    remove_ids.extend(found);
                    add_files.push((p, md));
                }
            }
            Ok(md) if md.is_dir() => {
                // Could be a folder moved in: (re)scan it.
                if has_indexed_under(&pl) {
                    remove_trees.push(pl);
                }
                add_trees.push(spelling.on_disk(p));
            }
            Ok(_) => {}
            Err(_) => {
                if is_audio {
                    // Gone. Drop the exact entry; one differing only by case
                    // only if it's gone too (a case-sensitive folder can hold
                    // both).
                    let ids: Vec<usize> = ix.find_all(p).collect();
                    match ids.iter().find(|&&i| Some(ix.name(i)) == name) {
                        Some(&i) => {
                            remove_ids.insert(i);
                        }
                        None => remove_ids
                            .extend(ids.into_iter().filter(|&i| std::fs::symlink_metadata(ix.full_path(i)).is_err())),
                    }
                }
                if has_indexed_under(&pl) {
                    remove_trees.push(pl);
                }
            }
        }
    }
    add_trees.sort();
    add_trees.dedup();

    // New folders that contain no audio don't change anything.
    let mut extra = ScanSink::default();
    for t in &add_trees {
        let mut keep_going = |_: &ScanSink| true;
        extra.scan(t, &mut keep_going);
    }
    if remove_ids.is_empty() && remove_trees.is_empty() && add_files.is_empty() && extra.entries.is_empty() {
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
    for (i, e) in entries.into_iter().enumerate() {
        if !dir_removed[e.dir as usize] && !remove_ids.contains(&i) {
            sink.entries.push(e);
        }
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


    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("totems-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn put(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![0u8; n]).unwrap();
    }
    fn scan(roots: &[PathBuf]) -> Index {
        let mut s = ScanSink::default();
        for r in roots {
            s.scan(r, &mut |_| true);
        }
        Index::build(s.dirs, s.entries)
    }
    /// Applying `events` to `before` must give exactly what a fresh full scan gives.
    fn assert_incremental(roots: &[PathBuf], before: &Index, events: &[(PathBuf, bool)]) {
        let inc = apply_changes(before, events, roots);
        let inc = inc.as_ref().unwrap_or(before);
        let paths = |ix: &Index| (0..ix.len()).map(|i| ix.full_path(i)).collect::<Vec<_>>();
        assert_eq!(paths(inc), paths(&scan(roots)));
    }

    /// B10: a case-only folder rename indexed its whole subtree twice.
    #[test]
    fn case_only_folder_rename_does_not_duplicate() {
        let b = tmpdir("caseren");
        let root = b.join("lib");
        put(&root.join("Drums").join("Kick.wav"), 1);
        let ix = scan(std::slice::from_ref(&root));
        std::fs::rename(root.join("Drums"), root.join("drums")).unwrap();
        // Exactly what notify reports on Windows (checked with a real watcher).
        let ev = [(root.join("Drums"), true), (root.join("drums"), true)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    /// B13: a case-only file rename kept the old spelling.
    #[test]
    fn case_only_file_rename_updates_name() {
        let b = tmpdir("casefile");
        let root = b.join("lib");
        put(&root.join("Kick.wav"), 1);
        let ix = scan(std::slice::from_ref(&root));
        std::fs::rename(root.join("Kick.wav"), root.join("kick.wav")).unwrap();
        let ev = [(root.join("Kick.wav"), true), (root.join("kick.wav"), true)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    /// B14: a sibling folder sharing the root's prefix (lib2 vs lib) was indexed.
    #[test]
    fn change_in_sibling_sharing_root_prefix_is_ignored() {
        let b = tmpdir("prefix");
        let root = b.join("lib");
        put(&root.join("a.wav"), 1);
        let ix = scan(std::slice::from_ref(&root));
        put(&b.join("lib2").join("x.wav"), 1);
        let ev = [(b.join("lib2").join("x.wav"), true)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    /// B15: a root inside a folder named like a skipped one got no live adds.
    #[test]
    fn live_add_under_root_inside_skip_named_folder() {
        let b = tmpdir("skipanc");
        let root = b.join("Installer").join("lib");
        put(&root.join("a.wav"), 1);
        let ix = scan(std::slice::from_ref(&root));
        put(&root.join("new.wav"), 1);
        let ev = [(root.join("new.wav"), true)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    /// B16: in a case-sensitive folder, changing Kick.wav dropped kick.wav.
    #[test]
    fn case_sensitive_folder_keeps_case_siblings() {
        let b = tmpdir("cssib");
        let root = b.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        let cs = std::process::Command::new("fsutil")
            .args(["file", "setCaseSensitiveInfo"])
            .arg(&root)
            .arg("enable")
            .output()
            .is_ok_and(|o| o.status.success());
        if !cs {
            return; // needs Windows 10+ case-sensitive folders
        }
        put(&root.join("kick.wav"), 1);
        put(&root.join("Kick.wav"), 2);
        let ix = scan(std::slice::from_ref(&root));
        put(&root.join("Kick.wav"), 3);
        let ev = [(root.join("Kick.wav"), false)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    /// B11: running the tests overwrote the user's real index cache.
    #[test]
    fn tests_never_write_the_real_cache() {
        assert!(cache_path().starts_with(std::env::temp_dir()), "{}", cache_path().display());
    }

    /// B12: after a root was deleted and re-created, live updates were dead
    /// for it until the next launch.
    #[test]
    fn root_deleted_and_recreated_is_indexed_again() {
        let b = tmpdir("rootback");
        let root = b.join("lib");
        put(&root.join("a.wav"), 1);
        let me = Indexer::start(vec![root.clone()], Arc::new(|| {}));
        assert!(wait_for(&me, |ix| ix.len() == 1));
        while me.scanning.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(wait_for(&me, |ix| ix.len() == 0), "root deleted");
        put(&root.join("back.wav"), 1);
        let ok = wait_for(&me, |ix| ix.len() == 1 && ix.name(0) == "back.wav");
        let _ = std::fs::remove_dir_all(b);
        assert!(ok, "re-created root not indexed: {:?}", me.index().map(|ix| names(&ix)));
    }

    /// Live update cost on a 1M-file index (one file deleted):
    /// `cargo test --release bench_apply_changes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_apply_changes() {
        let base = std::env::temp_dir().join("totems-bench-apply");
        let dirs: Vec<String> = (0..20_000).map(|d| base.join(format!("d{d}")).to_string_lossy().into_owned()).collect();
        let entries: Vec<crate::index::RawEntry> = (0..1_000_000u32)
            .map(|i| crate::index::RawEntry { dir: i % 20_000, name: format!("Sound {i:07}.wav"), size: i as u64, mtime: 0 })
            .collect();
        let ix = Index::build(dirs, entries);
        let gone = base.join("d7").join("Sound 0000007.wav");
        assert!(ix.find(&gone).is_some());
        let t = std::time::Instant::now();
        let new = apply_changes(&ix, &[(gone, true)], &[base]).unwrap();
        println!("apply_changes, 1M entries, 1 delete: {:?}", t.elapsed());
        assert_eq!(new.len(), 999_999);
    }
}
