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
    /// Every file named by a playlist: those outside the roots are kept in
    /// the index too (stat'ed, never watched).
    Extras(Vec<PathBuf>),
}

pub struct Indexer {
    current: RwLock<Option<Arc<Index>>>,
    pub version: AtomicU64,
    pub scanning: AtomicBool,
    pub scanned: AtomicUsize,
    /// Folders listed by the scan under way (moves even where there's no audio).
    pub scanned_dirs: AtomicUsize,
    restart: AtomicBool,
    tx: Sender<Cmd>,
}

pub fn cache_path() -> PathBuf {
    if cfg!(test) {
        #[cfg(test)]
        sweep_test_temp();
        // Tests must never overwrite the user's real index (B11).
        return std::env::temp_dir().join(format!("totems-test-cache-{}", std::process::id())).join("index.bin");
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("totems").join("index.bin")
}

/// Test runs leave `%TEMP%\totems-*` files and folders behind (a test can't
/// tidy a folder that parallel tests still use). Once per test process,
/// remove ones older than an hour. Totems itself keeps nothing in the temp
/// folder.
#[cfg(test)]
pub fn sweep_test_temp() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else { return };
        for e in rd.flatten() {
            let old = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(3600));
            if old && e.file_name().to_string_lossy().starts_with("totems-") {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    });
}

/// A folder as a root: no trailing separator (`F:\Samples\` lists its files
/// under `F:\Samples\`, which lookups of `F:\Samples\x.wav` never match,
/// B38), and `D:` (drive-relative) means the drive.
pub fn clean_root(p: &Path) -> PathBuf {
    let mut c: PathBuf = p.components().collect();
    if cfg!(windows) && c.to_str().is_some_and(|s| s.len() == 2 && s.ends_with(':')) {
        c.push("\\");
    }
    c
}

fn lower_key(p: &Path) -> String {
    // Lossy, never empty: an empty key would count as above every path.
    norm_lower(&p.to_string_lossy()).trim_end_matches('/').to_string()
}

/// `p` (lower-cased, '/') equals `base` or lies below it.
fn under(p: &str, base: &str) -> bool {
    p == base || (p.starts_with(base) && p.as_bytes().get(base.len()) == Some(&b'/'))
}

/// The roots actually scanned: cleaned, and each folder once whatever its
/// spelling (B43: `lib` and `LIB` indexed the same files twice). A root
/// inside another is kept: the outer walk may never reach it (a junction,
/// or inside a skipped folder like `Installer`, B48), so instead the outer
/// walk skips it (`nested_in`) and it is scanned on its own.
pub fn normalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots.iter().map(|r| clean_root(r)) {
        let l = lower_key(&r);
        if !out.iter().any(|o| lower_key(o) == l) {
            out.push(r);
        }
    }
    out
}

/// The roots strictly inside `dir`, as (lower-cased last name, lower-cased
/// path): a walk of `dir` leaves them to their own scans.
pub fn nested_in(dir: &Path, roots: &[PathBuf]) -> Vec<(String, String)> {
    let d = lower_key(dir);
    roots
        .iter()
        .map(|r| lower_key(r))
        .filter(|k| *k != d && under(k, &d))
        .map(|k| (k.rsplit('/').next().unwrap_or_default().to_string(), k))
        .collect()
}

/// Is `p` one of `nested` (from `nested_in`)? Cheap when there are none.
pub fn is_nested_root(p: &Path, name: &str, nested: &[(String, String)]) -> bool {
    if nested.is_empty() {
        return false;
    }
    let n = crate::index::lower(name);
    nested.iter().any(|(last, key)| *last == n && lower_key(p) == *key)
}

/// Totems' own folders (index cache, region exports): never indexed, by
/// full scans or live updates alike (B40).
pub fn own_dirs() -> &'static [String] {
    static OWN: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    OWN.get_or_init(|| {
        [cache_path().parent().map(Path::to_path_buf), crate::region::regions_dir().parent().map(Path::to_path_buf)]
            .into_iter()
            .flatten()
            .map(|p| lower_key(&p))
            .collect()
    })
}

/// Is `p` one of Totems' own folders or inside one?
pub fn is_own(p: &Path) -> bool {
    let l = lower_key(p);
    own_dirs().iter().any(|o| under(&l, o))
}

impl Indexer {
    pub fn start(roots: Vec<PathBuf>, repaint: Repaint) -> Arc<Indexer> {
        let roots = normalize_roots(&roots);
        let (tx, rx) = crossbeam_channel::unbounded();
        let me = Arc::new(Indexer {
            current: RwLock::new(None),
            version: AtomicU64::new(0),
            scanning: AtomicBool::new(true),
            scanned: AtomicUsize::new(0),
            scanned_dirs: AtomicUsize::new(0),
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
        let _ = self.tx.send(Cmd::Roots(normalize_roots(&roots)));
    }

    /// The playlists' files (all of them; the indexer keeps the ones
    /// outside the roots).
    pub fn set_extras(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(Cmd::Extras(paths));
    }

    /// Publish `ix`, its outside-the-roots folders marked.
    fn publish(&self, mut ix: Index, roots: &[PathBuf], repaint: &Repaint) {
        ix.outside = outside_dirs(&ix, roots);
        *self.current.write() = Some(Arc::new(ix));
        self.version.fetch_add(1, SeqCst);
        repaint();
    }

    fn run(self: Arc<Self>, mut roots: Vec<PathBuf>, rx: Receiver<Cmd>, tx: Sender<Cmd>, repaint: Repaint) {
        let cache = cache_path();
        let mut have_index = false;
        // The playlists' files; until the UI sends them, none.
        let mut extras: Vec<PathBuf> = Vec::new();
        if let Some(ix) = Index::load(&cache, &roots) {
            self.publish(ix, &roots, &repaint);
            have_index = true;
        }
        let mut need_scan = true;
        let mut _watcher: Option<notify::RecommendedWatcher> = None;
        let mut changes: HashMap<PathBuf, bool> = HashMap::new();
        let mut last_change = Instant::now();
        // When the oldest unapplied change arrived: a long copy keeps
        // resetting `last_change`, but the index still updates every 10 s.
        let mut first_change: Option<Instant> = None;
        // Roots that don't exist (deleted, drive unplugged): polled, and
        // rescanned and watched again when they come back.
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut last_save = Instant::now();
        let mut dirty = false;

        loop {
            if need_scan {
                need_scan = false;
                // Commands that queued up meanwhile: only the latest roots
                // matter, so a superseded list is never scanned (B39).
                for cmd in rx.try_iter() {
                    match cmd {
                        Cmd::Roots(r) => {
                            roots = r;
                            changes.clear();
                            first_change = None;
                            have_index = false;
                        }
                        Cmd::Rescan => {}
                        Cmd::Extras(e) => extras = e,
                        Cmd::Changed(paths) => {
                            for (p, structural) in paths {
                                *changes.entry(p).or_default() |= structural;
                            }
                        }
                    }
                }
                self.restart.store(false, SeqCst);
                self.scanning.store(true, SeqCst);
                self.scanned.store(0, SeqCst);
                self.scanned_dirs.store(0, SeqCst);
                repaint();
                // Watch before scanning: changes made during a long first
                // scan queue up and are applied once it's done.
                _watcher = start_watcher(&roots, tx.clone());
                missing = roots.iter().filter(|r| !r.exists()).cloned().collect();
                let mut sink = ScanSink::default();
                let mut last_pub = Instant::now();
                // Files in the last partial index published (None: none yet).
                let mut pub_len: Option<usize> = None;
                let mut cancelled = false;
                for root in &roots {
                    let mut progress = |s: &ScanSink| {
                        self.scanned.store(s.entries.len(), Relaxed);
                        self.scanned_dirs.store(s.folders.load(Relaxed), Relaxed);
                        // First run: make results searchable while still
                        // scanning. The first one goes out even if empty, so
                        // the UI shows a count instead of a blank "Indexing…".
                        if !have_index && last_pub.elapsed() > Duration::from_millis(1500) && pub_len != Some(s.entries.len()) {
                            last_pub = Instant::now();
                            pub_len = Some(s.entries.len());
                            self.publish(Index::build(s.dirs.clone(), s.entries.clone()), &roots, &repaint);
                        } else {
                            repaint();
                        }
                        !self.restart.load(Relaxed)
                    };
                    sink.skip = nested_in(root, &roots);
                    if !sink.scan(root, &mut progress) {
                        cancelled = true;
                        // Start over with whatever asked for the restart
                        // (it may already have been drained above).
                        need_scan = true;
                        break;
                    }
                }
                if !cancelled {
                    let ix = Index::build(sink.dirs, sink.entries);
                    let _ = ix.save(&cache, &roots);
                    last_save = Instant::now();
                    let ix = with_extras(&ix, &roots, &extras).unwrap_or(ix);
                    self.publish(ix, &roots, &repaint);
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
                Ok(Cmd::Extras(e)) => {
                    extras = e;
                    if let Some(cur) = self.index()
                        && let Some(ix) = with_extras(&cur, &roots, &extras)
                    {
                        self.publish(ix, &roots, &repaint);
                    }
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
                first_change = Some(Instant::now().checked_sub(Duration::from_secs(10)).unwrap_or_else(Instant::now)); // apply now
                changes.extend(back.into_iter().map(|r| (r, true)));
            }
            let quiet = last_change.elapsed() >= Duration::from_millis(200);
            let overdue = first_change.is_some_and(|t| t.elapsed() >= Duration::from_secs(10));
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
                    self.publish(ix, &roots, &repaint);
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
            .filter(|p| !is_own(p))
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
    dirs: HashMap<PathBuf, Option<Listing>>,
}

/// A folder's entries by exact name, and lower-cased name to the name (None
/// when two names differ only by case).
type Listing = (HashMap<OsString, std::fs::DirEntry>, HashMap<String, Option<OsString>>);

/// Each changed path is looked up in its folder's listing, read once per
/// batch: that gives the on-disk spelling (Windows reports a case-only
/// rename with both spellings, and both exist), and on Windows the size and
/// mtime come with the listing, so a copy of thousands of files costs no
/// stat per file (it was 36 us each).
impl Spelling {
    /// Is `p`'s exact name in its folder's listing?
    fn listed(&mut self, p: &Path) -> bool {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return p.exists() };
        if !self.dirs.contains_key(parent) {
            let _ = self.metadata(&parent.join(name));
        }
        self.dirs.get(parent).and_then(|l| l.as_ref()).is_some_and(|(exact, _)| exact.contains_key(name))
    }

    /// The path as spelled on disk and its metadata (links not followed);
    /// NotFound if it doesn't exist.
    fn metadata(&mut self, p: &Path) -> std::io::Result<(PathBuf, std::fs::Metadata)> {
        let stat = |p: &Path| std::fs::symlink_metadata(p).map(|md| (p.to_path_buf(), md));
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return stat(p) };
        let listing = self.dirs.entry(parent.to_path_buf()).or_insert_with(|| {
            let rd = std::fs::read_dir(parent).ok()?;
            let mut exact = HashMap::new();
            let mut lower: HashMap<String, Option<OsString>> = HashMap::new();
            for e in rd.flatten() {
                let n = e.file_name();
                if let Some(l) = n.to_str().map(crate::index::lower) {
                    // Two names differing only by case: ambiguous.
                    lower.entry(l).and_modify(|v| *v = None).or_insert_with(|| Some(n.clone()));
                }
                exact.insert(n, e);
            }
            Some((exact, lower))
        });
        // The folder can't be listed (gone, or no access): ask directly.
        let Some((exact, lower)) = listing else { return stat(p) };
        let real = if exact.contains_key(name) {
            Some(name.to_os_string())
        } else if std::fs::symlink_metadata(p).is_ok() {
            // It exists under another case: the folder is case-insensitive.
            // (In a case-sensitive one, a deleted Kick.wav must not resolve
            // to a surviving kick.wav, B54.)
            name.to_str().and_then(|n| lower.get(&crate::index::lower(n))).cloned().flatten()
        } else {
            None
        };
        match real.and_then(|r| exact.get(&r).map(|e| (r, e))) {
            Some((r, e)) => match e.metadata() {
                Ok(md) => Ok((parent.join(r), md)),
                Err(_) => stat(&parent.join(r)),
            },
            None => Err(std::io::ErrorKind::NotFound.into()),
        }
    }
}

/// Apply a batch of changed paths. Returns None if nothing relevant changed.
/// Per folder of `ix`: lies outside every root.
fn outside_dirs(ix: &Index, roots: &[PathBuf]) -> Vec<bool> {
    let keys: Vec<String> = roots.iter().map(|r| lower_key(r)).collect();
    let v: Vec<bool> =
        ix.dirs_lower.iter().map(|d| !keys.iter().any(|k| under(d.trim_end_matches('/'), k))).collect();
    if v.iter().any(|&o| o) { v } else { Vec::new() }
}

/// `ix` with the playlists' files outside the roots (`extras`) in it, and
/// no other outside files: None if it already is. New ones are stat'ed (a
/// file that isn't there is left out: the playlist shows it as missing).
/// Only full scans, cache loads and playlist changes call this; live
/// updates never touch outside files, so they keep them.
fn with_extras(ix: &Index, roots: &[PathBuf], extras: &[PathBuf]) -> Option<Index> {
    let keys: Vec<String> = roots.iter().map(|r| lower_key(r)).collect();
    let inside = |dir: &str| keys.iter().any(|k| under(dir, k));
    // Wanted: path key → path.
    let mut want: HashMap<String, &PathBuf> = HashMap::new();
    for p in extras {
        let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) else { continue };
        let d = lower_key(dir);
        if d.is_empty() || inside(&d) || audio_ext_id(name).is_none() || is_own(p) {
            continue;
        }
        want.entry(format!("{d}/{}", crate::index::lower(name))).or_insert(p);
    }
    let outside = outside_dirs(ix, roots);
    let mut have: HashSet<String> = HashSet::new();
    let mut drop: Vec<bool> = Vec::new();
    if !outside.is_empty() {
        drop = vec![false; ix.len()];
        for (i, gone) in drop.iter_mut().enumerate() {
            if !outside[ix.dir[i] as usize] {
                continue;
            }
            let d = ix.dirs_lower[ix.dir[i] as usize].trim_end_matches('/');
            let k = format!("{d}/{}", String::from_utf8_lossy(ix.name_lower(i)));
            if want.contains_key(&k) {
                have.insert(k);
            } else {
                *gone = true;
            }
        }
    }
    let mut add = Vec::new();
    for (k, p) in &want {
        if have.contains(k) {
            continue;
        }
        let Ok(md) = std::fs::metadata(p) else { continue };
        if !md.is_file() {
            continue;
        }
        let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) else { continue };
        let entry = crate::index::RawEntry { dir: 0, name: name.to_string(), size: md.len(), mtime: crate::index::mtime_of(&md) };
        add.push((dir.to_string_lossy().into_owned(), entry));
    }
    if add.is_empty() && !drop.contains(&true) {
        return None;
    }
    Some(ix.with_changes(|i| drop.get(i).copied().unwrap_or(false), add))
}

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
    let sorted = ix.sorted_dirs();
    let dl = |k: u32| &*ix.dirs_lower[k as usize];
    let has_indexed_under = |p: &str| {
        let p = p.trim_end_matches('/');
        let i = sorted.partition_point(|&d| dl(d) < p);
        sorted[i..].iter().map(|&d| dl(d)).take_while(|d| d.starts_with(p)).any(|d| d.len() == p.len() || d.as_bytes()[p.len()] == b'/')
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
    // Folders to drop, as (lower-cased key without a trailing '/', the path
    // as spelled by the event or on disk).
    let mut remove_trees: Vec<(String, PathBuf)> = Vec::new();
    let mut add_files: Vec<(PathBuf, std::fs::Metadata)> = Vec::new();
    let mut add_trees: Vec<PathBuf> = Vec::new();
    let mut spelling = Spelling::default();

    for (p, structural) in paths {
        let Some(ps) = p.to_str() else { continue };
        let pl = norm_lower(ps);
        let Some(ri) = root_of(&pl) else { continue };
        // Spelled from its root: a nested root typed in another case has a
        // watch of its own reporting its spelling, and the same file must
        // not be added twice (B53).
        let rest: PathBuf = p.components().skip(roots[ri].components().count()).collect();
        // The event is for a root itself: keep the root's own spelling (its
        // files are spelled from it, B55).
        let is_root = rest.as_os_str().is_empty();
        let respelled = if is_root { roots[ri].clone() } else { roots[ri].join(rest) };
        let key = pl.trim_end_matches('/').to_string(); // `q:/` -> `q:` (B57)
        let p = &respelled;
        let name = p.file_name().and_then(|n| n.to_str());
        let is_audio = name.and_then(audio_ext_id).is_some();
        if !is_audio && !structural {
            continue; // content change of a non-audio file or a folder
        }
        if in_skipped(p, &roots[ri]) || is_own(p) {
            continue;
        }
        match spelling.metadata(p) {
            Ok((p, md)) if md.is_file() => {
                if !is_audio {
                    continue;
                }
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
            Ok((real, md)) if md.is_dir() => {
                // Could be a folder moved in: (re)scan it.
                let real = if is_root { p.clone() } else { real };
                if has_indexed_under(&pl) {
                    remove_trees.push((key, real.clone()));
                }
                add_trees.push(real);
            }
            Ok(_) => {}
            Err(_) => {
                if is_audio {
                    // Gone. Drop the exact entry; one differing only by case
                    // only if it's gone too (a case-sensitive folder can hold
                    // both).
                    match ix.find_exact(p) {
                        Some(i) => {
                            remove_ids.insert(i);
                        }
                        None => remove_ids
                            .extend(ix.find_all(p).filter(|&i| std::fs::symlink_metadata(ix.full_path(i)).is_err())),
                    }
                }
                if has_indexed_under(&pl) {
                    remove_trees.push((key, p.clone()));
                }
            }
        }
    }
    // A rescanned tree may hold roots of its own, which its walk skips: they
    // were removed with it, so rescan them too (B52).
    for t in add_trees.clone() {
        let lt = lower_key(&t);
        add_trees.extend(roots.iter().filter(|r| {
            let lr = lower_key(r);
            lr != lt && under(&lr, &lt)
        }).cloned());
    }
    add_trees.sort();
    add_trees.dedup();
    // An event inside a rescanned tree may carry a stale parent spelling
    // (a case-only rename of the parent in the same batch): the tree's
    // walk covers it. Roots stay: the outer walk skips them.
    let tree_keys: Vec<String> = add_trees.iter().map(|t| lower_key(t)).collect();
    let root_keys: Vec<String> = roots.iter().map(|r| lower_key(r)).collect();
    let in_other_tree = |l: &str| tree_keys.iter().any(|k| *k != l && under(l, k));
    add_trees.retain(|t| {
        let l = lower_key(t);
        root_keys.contains(&l) || !in_other_tree(&l)
    });
    add_files.retain(|(p, _)| !in_other_tree(&lower_key(p)));

    // New folders that contain no audio don't change anything.
    let mut extra = ScanSink::default();
    for t in &add_trees {
        let mut keep_going = |_: &ScanSink| true;
        extra.skip = nested_in(t, roots);
        extra.scan(t, &mut keep_going);
    }
    if remove_ids.is_empty() && remove_trees.is_empty() && add_files.is_empty() && extra.entries.is_empty() {
        return None;
    }

    // A folder goes with a removed tree if it's spelled like it, or if it
    // differs in case and its own spelling is no longer listed (a case-only
    // rename). In a case-sensitive folder, `sub` stays when `Sub` goes (B56).
    // Deleting a big folder reports every subfolder too: a tree inside
    // another with the same spelling adds nothing, and checking every
    // folder against thousands of trees took seconds.
    remove_trees.sort_by(|a, b| a.0.cmp(&b.0));
    let mut kept: Vec<(String, PathBuf)> = Vec::with_capacity(remove_trees.len());
    for t in remove_trees {
        if !kept.iter().rev().take(64).any(|k| under(&t.0, &k.0) && t.1.starts_with(&k.1)) {
            kept.push(t);
        }
    }
    let remove_trees = kept;
    let dir_removed: Vec<bool> = (0..ix.dirs.len())
        .map(|i| {
            let d = &*ix.dirs_lower[i];
            remove_trees.iter().any(|(key, exact)| {
                if !under(d, key) {
                    return false;
                }
                let ds = &*ix.dirs[i];
                if let Some(es) = exact.to_str()
                    && ds.starts_with(es)
                    && (ds.len() == es.len() || es.ends_with(['\\', '/']) || matches!(ds.as_bytes()[es.len()], b'\\' | b'/'))
                {
                    return true;
                }
                // Differs in case somewhere: a distinct folder only if the
                // index's spelling is still listed at the level it differs.
                let mut prefix = PathBuf::new();
                for (a, b) in Path::new(ds).components().zip(exact.components()) {
                    prefix.push(a);
                    if a != b && spelling.listed(&prefix) {
                        return false;
                    }
                }
                true
            })
        })
        .collect();
    let mut add: Vec<(String, crate::index::RawEntry)> = Vec::with_capacity(add_files.len() + extra.entries.len());
    for (p, md) in &add_files {
        if let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) {
            let e = crate::index::RawEntry { dir: 0, name: name.to_string(), size: md.len(), mtime: crate::index::mtime_of(md) };
            add.push((dir.to_string_lossy().into_owned(), e));
        }
    }
    for e in extra.entries {
        add.push((extra.dirs[e.dir as usize].clone(), e));
    }
    let gone = |i: usize| dir_removed[ix.dir[i] as usize] || remove_ids.contains(&i);
    if add.is_empty() && !(0..ix.len()).any(gone) {
        return None;
    }
    Some(ix.with_changes(gone, add))
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

    /// A playlist's file outside the roots is indexed (marked outside),
    /// kept through live updates, and dropped when no playlist names it.
    /// Files inside the roots, missing files and non-audio are left alone.
    #[test]
    fn playlist_files_outside_roots_are_indexed() {
        let base = std::env::temp_dir().join(format!("totems-extras-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, outside) = (base.join("lib"), base.join("away"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("in.wav"), b"x").unwrap();
        std::fs::write(outside.join("far.wav"), b"xyz").unwrap();
        std::fs::write(outside.join("far.txt"), b"x").unwrap();

        let me = Indexer::start(vec![root.clone()], Arc::new(|| {}));
        assert!(wait_for(&me, |ix| ix.len() == 1));
        me.set_extras(vec![root.join("in.wav"), outside.join("far.wav"), outside.join("far.txt"), outside.join("gone.wav")]);
        assert!(wait_for(&me, |ix| ix.len() == 2), "the outside file joins the index");
        let ix = me.index().unwrap();
        let far = ix.find(&outside.join("far.wav")).expect("indexed");
        assert!(ix.is_outside(far));
        assert_eq!(ix.size[far], 3);
        assert!(!ix.is_outside(ix.find(&root.join("in.wav")).unwrap()));

        // A live update inside the root keeps it.
        std::fs::write(root.join("new.wav"), b"x").unwrap();
        assert!(wait_for(&me, |ix| ix.len() == 3), "live update");
        let ix = me.index().unwrap();
        assert!(ix.find(&outside.join("far.wav")).is_some_and(|i| ix.is_outside(i)));

        // No playlist names it any more: gone.
        me.set_extras(Vec::new());
        assert!(wait_for(&me, |ix| ix.len() == 2 && !ix.has_outside()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A scan counts every folder it lists, audio or not: on a drive with
    /// little audio the file count sat still for minutes and the status bar
    /// looked stuck.
    #[test]
    fn scan_counts_folders_without_audio() {
        let b = tmpdir("foldercount");
        for i in 0..40 {
            std::fs::create_dir_all(b.join(format!("empty{i}"))).unwrap();
        }
        put(&b.join("sfx").join("a.wav"), 1);
        let mut s = ScanSink::default();
        let mut calls = 0;
        assert!(s.scan(&b, &mut |_| {
            calls += 1;
            true
        }));
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.folders.load(Relaxed), 42, "the root, 40 empty folders and sfx");
        assert!(calls >= 1);
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

    /// B38: a root typed with a trailing separator listed its own files
    /// under `lib\`, which lookups of `lib\a.wav` never match: a modified
    /// file was indexed twice and a deleted one stayed.
    #[test]
    fn root_with_trailing_separator_live_modify_and_delete() {
        let b = tmpdir("trail");
        let root = b.join("lib");
        put(&root.join("a.wav"), 1);
        let mut s = root.as_os_str().to_owned();
        s.push(std::path::MAIN_SEPARATOR_STR);
        let roots = normalize_roots(&[PathBuf::from(s)]);
        assert_eq!(roots, vec![root.clone()]);
        let ix = scan(&roots);
        put(&root.join("a.wav"), 5);
        let n = apply_changes(&ix, &[(root.join("a.wav"), false)], &roots).map_or(ix.len(), |i| i.len());
        std::fs::remove_file(root.join("a.wav")).unwrap();
        let res = std::panic::catch_unwind(|| assert_incremental(&roots, &ix, &[(root.join("a.wav"), true)]));
        let _ = std::fs::remove_dir_all(b);
        assert_eq!(n, 1, "modify duplicated the entry");
        res.unwrap();
    }

    /// B43: the same folder under two spellings, or a folder inside another
    /// root, was indexed twice.
    #[test]
    fn roots_are_normalised() {
        let p = |s: &str| PathBuf::from(s);
        if cfg!(windows) {
            assert_eq!(normalize_roots(&[p(r"C:\Lib\"), p(r"c:\lib")]), vec![p(r"C:\Lib")]);
            // Nested roots stay (B48); the outer walk skips them instead.
            assert_eq!(normalize_roots(&[p(r"c:\samples"), p(r"C:\")]), vec![p(r"c:\samples"), p(r"C:\")]);
            assert_eq!(normalize_roots(&[p(r"F:\lib"), p(r"F:\lib2")]), vec![p(r"F:\lib"), p(r"F:\lib2")]);
            assert_eq!(clean_root(&p("D:")), p(r"D:\"));
            assert_eq!(nested_in(&p(r"C:\"), &[p(r"C:\"), p(r"c:\Samples"), p(r"D:\x")]), vec![("samples".into(), "c:/samples".into())]);
        }
        let b = tmpdir("casedup");
        put(&b.join("lib/a.wav"), 1);
        let n = scan(&normalize_roots(&[b.join("lib"), b.join("LIB")])).len();
        let _ = std::fs::remove_dir_all(b);
        assert_eq!(n, 1);
    }

    /// B48: a root inside another root was dropped as covered, but the
    /// outer walk never reaches a junction or a folder under a skipped name
    /// (`Installer`): such roots were silently never indexed. Plain nested
    /// roots must still not index anything twice.
    #[test]
    fn nested_roots_are_scanned_once_each() {
        let b = tmpdir("nested");
        let lib = b.join("lib");
        put(&lib.join("top.wav"), 1);
        put(&lib.join("Installer").join("Sounds").join("inst.wav"), 1);
        put(&lib.join("sub").join("sub.wav"), 1);
        put(&b.join("elsewhere").join("linked.wav"), 1);
        let junction = cfg!(windows)
            && std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(lib.join("Linked"))
                .arg(b.join("elsewhere"))
                .output()
                .is_ok_and(|o| o.status.success());
        let mut roots = vec![lib.clone(), lib.join("Installer").join("Sounds"), lib.join("SUB")];
        if junction {
            roots.push(lib.join("Linked"));
        }
        let me = Indexer::start(roots, Arc::new(|| {}));
        let want = if junction { 4 } else { 3 };
        let ok = wait_for(&me, |ix| ix.len() == want);
        while me.scanning.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let got = me.index().map(|ix| names(&ix));
        let _ = std::fs::remove_dir(lib.join("Linked"));
        let _ = std::fs::remove_dir_all(&b);
        assert!(ok, "{got:?}");
        assert_eq!(got.map(|v| v.len()), Some(want), "each file once");
    }

    /// B39: set_roots calls that queued up during a scan were each scanned
    /// in full, including superseded lists.
    #[test]
    fn queued_set_roots_skip_superseded_roots() {
        let b = tmpdir("coalesce");
        let (r0, a, bb) = (b.join("r0"), b.join("a"), b.join("b"));
        for i in 0..3000 {
            put(&r0.join(format!("d{}/r{i}.wav", i % 30)), 0);
        }
        put(&a.join("a_only.wav"), 1);
        put(&bb.join("b_only.wav"), 1);
        let slot: Arc<std::sync::OnceLock<Arc<Indexer>>> = Default::default();
        let saw_a = Arc::new(AtomicBool::new(false));
        let (s2, sa) = (slot.clone(), saw_a.clone());
        let me = Indexer::start(
            vec![r0],
            Arc::new(move || {
                if let Some(me) = s2.get()
                    && let Some(ix) = me.index()
                    && (0..ix.len()).any(|i| ix.name(i) == "a_only.wav")
                {
                    sa.store(true, SeqCst);
                }
            }),
        );
        let _ = slot.set(me.clone());
        me.set_roots(vec![a]);
        me.set_roots(vec![bb]);
        let ok = wait_for(&me, |ix| ix.len() == 1 && ix.name(0) == "b_only.wav");
        let _ = std::fs::remove_dir_all(&b);
        assert!(ok);
        assert!(!saw_a.load(SeqCst), "superseded roots were scanned and published");
    }

    /// B40: Totems' own folder (index cache, region exports) was indexed by
    /// full scans while live changes there were ignored, so ghosts stayed.
    #[test]
    fn own_folder_never_indexed() {
        let own = cache_path().parent().unwrap().to_path_buf();
        let outer = tmpdir("ownparent");
        put(&outer.join("keep.wav"), 1);
        // A root that contains the app's folder, and one inside it.
        let inside = own.join("lib");
        put(&inside.join("ghost.wav"), 1);
        let n_inside = scan(std::slice::from_ref(&inside)).len();
        assert!(is_own(&inside) && is_own(&own) && !is_own(&outer));
        let mut sibling = own.clone().into_os_string();
        sibling.push("Foo");
        assert!(!is_own(Path::new(&sibling)), "prefix without a separator");
        let _ = std::fs::remove_dir_all(&inside);
        let _ = std::fs::remove_dir_all(&outer);
        assert_eq!(n_inside, 0, "files in the app's own folder were indexed");
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
        let new = apply_changes(&ix, &[(gone, true)], std::slice::from_ref(&base)).unwrap();
        println!("apply_changes, 1M entries, 1 delete: {:?}", t.elapsed());
        assert_eq!(new.len(), 999_999);
        // Every publish marks folders outside the roots (20k folders, 3 roots).
        let roots = [base.join("d1"), base.join("x"), base];
        let t = std::time::Instant::now();
        let out = outside_dirs(&new, &roots);
        println!("outside_dirs, 20k folders, 3 roots: {:?}", t.elapsed());
        assert!(out.is_empty());
    }
    /// A big copy landing: 5,000 new files in 50 folders, applied to a
    /// 250k-entry index: `cargo test --release bench_apply_copy -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_apply_copy() {
        let b = tmpdir("benchcopy");
        let root = b.join("lib");
        for i in 0..5000 {
            put(&root.join(format!("d{}", i % 50)).join(format!("f{i}.wav")), 1);
        }
        let dirs: Vec<String> = (0..2000).map(|d| root.join(format!("old{d}")).to_string_lossy().into_owned()).collect();
        let entries: Vec<crate::index::RawEntry> = (0..250_000u32)
            .map(|i| crate::index::RawEntry { dir: i % 2000, name: format!("o{i}.wav"), size: 1, mtime: 0 })
            .collect();
        let ix = Index::build(dirs, entries);
        let events: Vec<(PathBuf, bool)> =
            (0..5000).map(|i| (root.join(format!("d{}", i % 50)).join(format!("f{i}.wav")), true)).collect();
        let roots = vec![root.clone()];
        let _ = apply_changes(&ix, &events, &roots); // warm the OS cache
        let t = std::time::Instant::now();
        let n = apply_changes(&ix, &events, &roots).map(|ix| ix.len());
        println!("apply_changes, 5k new files on 250k: {:?} -> {n:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(n, Some(255_000));
    }
    /// B52: rescanning a folder above a nested root (here a case-only
    /// rename) dropped the nested root's files until the next full scan.
    #[test]
    fn live_rescan_of_folder_above_nested_root_keeps_its_files() {
        let b = tmpdir("nestlive");
        let lib = b.join("lib");
        put(&lib.join("top.wav"), 1);
        put(&lib.join("Sub").join("mid.wav"), 1);
        put(&lib.join("Sub").join("deep").join("deep.wav"), 1);
        let roots = normalize_roots(&[lib.clone(), lib.join("Sub").join("deep")]);
        let full = |roots: &[PathBuf]| {
            let mut s = ScanSink::default();
            for r in roots {
                s.skip = nested_in(r, roots);
                s.scan(r, &mut |_| true);
            }
            Index::build(s.dirs, s.entries)
        };
        let ix = full(&roots);
        assert_eq!(ix.len(), 3);
        std::fs::rename(lib.join("Sub"), lib.join("sub")).unwrap();
        let ev = [(lib.join("Sub"), true), (lib.join("sub"), true)];
        let got = apply_changes(&ix, &ev, &roots).map(|ix| {
            let mut v: Vec<String> = (0..ix.len()).map(|i| ix.name(i).to_lowercase()).collect();
            v.sort();
            v
        });
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(got, Some(vec!["deep.wav".to_string(), "mid.wav".into(), "top.wav".into()]));
    }
    /// B53: a nested root typed in another case than on disk has its own
    /// watch, which reports paths in its spelling: new files were added twice.
    #[test]
    fn live_add_in_nested_root_typed_in_other_case_indexed_once() {
        if !cfg!(windows) {
            return;
        }
        let b = tmpdir("nestcase");
        let lib = b.join("Lib");
        put(&lib.join("Sub").join("old.wav"), 1);
        let roots = vec![lib.clone(), b.join("lib").join("sub")];
        let me = Indexer::start(roots, Arc::new(|| {}));
        assert!(wait_for(&me, |ix| ix.len() == 1));
        while me.scanning.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300)); // watchers armed
        put(&lib.join("Sub").join("new.wav"), 1);
        let _ = wait_for(&me, |ix| ix.len() >= 2);
        std::thread::sleep(Duration::from_millis(800)); // let every batch land
        let got = me.index().map(|ix| names(&ix));
        drop(me);
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(got.as_ref().map(|v| v.len()), Some(2), "{got:#?}");
    }
    /// B54: in a case-sensitive folder holding Kick.wav and kick.wav, deleting
    /// Kick.wav resolved to the survivor by case-insensitive lookup: the
    /// deleted entry stayed.
    #[test]
    fn delete_in_case_sensitive_folder_removes_entry() {
        let b = tmpdir("csdel");
        let root = b.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        let cs = !cfg!(windows)
            || std::process::Command::new("fsutil")
                .args(["file", "setCaseSensitiveInfo"])
                .arg(&root)
                .arg("enable")
                .output()
                .is_ok_and(|o| o.status.success());
        if !cs {
            return;
        }
        put(&root.join("kick.wav"), 1);
        put(&root.join("Kick.wav"), 2);
        let ix = scan(std::slice::from_ref(&root));
        assert_eq!(ix.len(), 2);
        std::fs::remove_file(root.join("Kick.wav")).unwrap();
        let ev = [(root.join("Kick.wav"), true)];
        let r = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r.unwrap();
    }

    fn full_scan(roots: &[PathBuf]) -> Index {
        let mut s = ScanSink::default();
        for r in roots {
            s.skip = nested_in(r, roots);
            s.scan(r, &mut |_| true);
        }
        Index::build(s.dirs, s.entries)
    }

    /// B55: a nested root typed in another case than on disk, whose folder
    /// and a file in it change in one batch: the folder was rescanned under
    /// the disk's spelling and the file added under the root's, twice.
    #[test]
    fn nested_root_folder_and_file_events_in_one_batch_index_once() {
        let b = tmpdir("nestcopy");
        let lib = b.join("Lib");
        put(&lib.join("top.wav"), 1);
        let roots = normalize_roots(&[lib.clone(), b.join("lib").join("sub")]);
        let ix = full_scan(&roots);
        put(&lib.join("Sub").join("a.wav"), 1);
        let ev = [(lib.join("Sub"), true), (lib.join("Sub").join("a.wav"), true)];
        let got = apply_changes(&ix, &ev, &roots).map(|ix| names(&ix));
        let want = names(&full_scan(&roots));
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(got, Some(want));
    }

    fn case_sensitive(dir: &Path) -> bool {
        !cfg!(windows)
            || std::process::Command::new("fsutil")
                .args(["file", "setCaseSensitiveInfo"])
                .arg(dir)
                .arg("enable")
                .output()
                .is_ok_and(|o| o.status.success())
    }

    /// B56: folder removal ignored case: in a case-sensitive folder holding
    /// `sub` and `Sub`, deleting `Sub` (or moving a new `Sub` in) dropped
    /// `sub`'s files too.
    #[test]
    fn folder_changes_in_case_sensitive_folder_keep_sibling() {
        let b = tmpdir("csdeldir");
        let root = b.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        if !case_sensitive(&root) {
            return;
        }
        put(&root.join("sub").join("keep.wav"), 1);
        put(&root.join("Sub").join("gone.wav"), 2);
        let ix = scan(std::slice::from_ref(&root));
        assert_eq!(ix.len(), 2);
        std::fs::remove_dir_all(root.join("Sub")).unwrap();
        let ev = [(root.join("Sub").join("gone.wav"), true), (root.join("Sub"), true)];
        let r1 = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        // A folder `Sub` moved in next to `sub`.
        let ix = scan(std::slice::from_ref(&root));
        put(&b.join("incoming").join("new.wav"), 3);
        std::fs::rename(b.join("incoming"), root.join("Sub")).unwrap();
        let ev = [(root.join("Sub"), true)];
        let r2 = std::panic::catch_unwind(|| assert_incremental(std::slice::from_ref(&root), &ix, &ev));
        let _ = std::fs::remove_dir_all(b);
        r1.unwrap();
        r2.unwrap();
    }

    /// B57: an event for a drive root itself (a drive coming back) never
    /// removed anything below it: its key kept the trailing '/' (`q:/`).
    #[cfg(windows)]
    #[test]
    fn drive_root_event_rescans_the_drive() {
        let b = tmpdir("substroot");
        put(&b.join("a").join("deep.wav"), 1);
        let Some(letter) = ('P'..='Y').rev().find(|c| !Path::new(&format!("{c}:\\")).exists()) else { return };
        let drive = format!("{letter}:");
        let ok = std::process::Command::new("subst").arg(&drive).arg(&b).output().is_ok_and(|o| o.status.success());
        if !ok {
            let _ = std::fs::remove_dir_all(&b);
            return;
        }
        let root = PathBuf::from(format!("{drive}\\"));
        let roots = normalize_roots(std::slice::from_ref(&root));
        let ix = full_scan(&roots);
        std::fs::rename(root.join("a"), root.join("A")).unwrap();
        std::fs::remove_file(root.join("A").join("deep.wav")).unwrap();
        put(&root.join("A").join("new.wav"), 1);
        let got = apply_changes(&ix, &[(root.clone(), true)], &roots).map(|ix| names(&ix));
        let want = names(&full_scan(&roots));
        let _ = std::process::Command::new("subst").arg(&drive).arg("/d").output();
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(got, Some(want));
    }
}

#[cfg(test)]
mod live_update_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("totems-live-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn put(p: &Path, n: usize) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![0u8; n]).unwrap();
    }
    fn fscan(roots: &[PathBuf]) -> Index {
        let mut s = ScanSink::default();
        for r in roots {
            s.skip = nested_in(r, roots);
            s.scan(r, &mut |_| true);
        }
        Index::build(s.dirs, s.entries)
    }
    fn paths(ix: &Index) -> Vec<PathBuf> {
        (0..ix.len()).map(|i| ix.full_path(i)).collect()
    }
    /// Apply, compare with a fresh scan, clean up, then assert.
    fn check(b: &Path, roots: &[PathBuf], before: &Index, ev: &[(PathBuf, bool)]) {
        let inc = apply_changes(before, ev, roots);
        let got = paths(inc.as_ref().unwrap_or(before));
        let want = paths(&fscan(roots));
        let _ = std::fs::remove_dir_all(b);
        assert_eq!(got, want);
    }
    fn tree(root: &Path) {
        put(&root.join("top.wav"), 1);
        put(&root.join("Drums").join("Kick.wav"), 1);
        put(&root.join("Drums").join("Snares").join("Snare.wav"), 1);
        put(&root.join("Drums").join("Snares").join("Rim").join("Rim.wav"), 1);
        put(&root.join("Keys").join("Piano.wav"), 1);
    }

    #[test]
    fn live_folder_deleted() {
        let b = tmpdir("del");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::remove_dir_all(r.join("Drums")).unwrap();
        check(&b, &roots, &ix, &[(r.join("Drums"), true)]);
    }
    #[test]
    fn live_folder_deleted_bottom_up_events() {
        let b = tmpdir("delnest");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::remove_dir_all(r.join("Drums")).unwrap();
        let d = r.join("Drums");
        let ev = [
            (d.join("Snares").join("Rim").join("Rim.wav"), true),
            (d.join("Snares").join("Rim"), true),
            (d.join("Snares").join("Snare.wav"), true),
            (d.join("Snares"), true),
            (d.join("Kick.wav"), true),
            (d.clone(), true),
        ];
        check(&b, &roots, &ix, &ev);
    }
    #[test]
    fn live_folder_renamed() {
        let b = tmpdir("ren");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::rename(r.join("Drums"), r.join("Percussion")).unwrap();
        check(&b, &roots, &ix, &[(r.join("Drums"), true), (r.join("Percussion"), true)]);
    }
    #[test]
    fn live_case_only_rename_nested() {
        let b = tmpdir("caseonly");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::rename(r.join("Drums"), r.join("DRUMS")).unwrap();
        check(&b, &roots, &ix, &[(r.join("Drums"), true), (r.join("DRUMS"), true)]);
    }
    #[test]
    fn live_case_only_rename_inner_and_outer_same_batch() {
        let b = tmpdir("caseonly2");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::rename(r.join("Drums").join("Snares"), r.join("Drums").join("snares")).unwrap();
        std::fs::rename(r.join("Drums"), r.join("drums")).unwrap();
        let ev = [
            (r.join("Drums").join("Snares"), true),
            (r.join("Drums").join("snares"), true),
            (r.join("Drums"), true),
            (r.join("drums"), true),
        ];
        check(&b, &roots, &ix, &ev);
    }
    #[test]
    fn live_moved_out_and_in() {
        let b = tmpdir("mv");
        let r = b.join("lib");
        tree(&r);
        put(&b.join("outside").join("Pack").join("x.wav"), 1);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::rename(r.join("Keys"), b.join("Keys")).unwrap();
        std::fs::rename(b.join("outside").join("Pack"), r.join("Pack")).unwrap();
        check(&b, &roots, &ix, &[(r.join("Keys"), true), (r.join("Pack"), true)]);
    }
    #[test]
    fn live_folder_replaced_by_same_name() {
        let b = tmpdir("repl");
        let r = b.join("lib");
        tree(&r);
        put(&b.join("new").join("fresh.wav"), 1);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::rename(r.join("Drums"), b.join("old")).unwrap();
        std::fs::rename(b.join("new"), r.join("Drums")).unwrap();
        check(&b, &roots, &ix, &[(r.join("Drums"), true), (r.join("Drums"), true)]);
    }
    #[test]
    fn live_root_deleted() {
        let b = tmpdir("rootdel");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::remove_dir_all(&r).unwrap();
        let inc = apply_changes(&ix, &[(r.clone(), true)], &roots).unwrap();
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(inc.len(), 0);
    }
    #[test]
    fn live_root_deleted_and_recreated_one_batch() {
        let b = tmpdir("rootre");
        let r = b.join("lib");
        tree(&r);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        std::fs::remove_dir_all(&r).unwrap();
        put(&r.join("back.wav"), 1);
        check(&b, &roots, &ix, &[(r.clone(), true), (r.clone(), true)]);
    }
    #[test]
    fn live_root_typed_in_other_case_event() {
        let b = tmpdir("rootcase");
        let r = b.join("Lib");
        tree(&r);
        let typed = b.join("lib");
        let roots = normalize_roots(std::slice::from_ref(&typed));
        let ix = fscan(&roots);
        std::fs::remove_dir_all(r.join("Drums")).unwrap();
        put(&r.join("Keys").join("Organ.wav"), 1);
        // The watch was armed on the typed spelling.
        check(&b, &roots, &ix, &[(typed.join("Drums"), true), (typed.join("Keys"), true), (typed.clone(), true)]);
    }
    #[test]
    fn live_many_folders_deleted_one_batch() {
        let b = tmpdir("many");
        let r = b.join("lib");
        for i in 0..200 {
            put(&r.join(format!("F{i}")).join("s").join("a.wav"), 1);
        }
        put(&r.join("keep").join("k.wav"), 1);
        let roots = normalize_roots(std::slice::from_ref(&r));
        let ix = fscan(&roots);
        let mut ev = Vec::new();
        for i in 0..200 {
            std::fs::remove_dir_all(r.join(format!("F{i}"))).unwrap();
            ev.push((r.join(format!("F{i}")).join("s").join("a.wav"), true));
            ev.push((r.join(format!("F{i}")).join("s"), true));
            ev.push((r.join(format!("F{i}")), true));
        }
        check(&b, &roots, &ix, &ev);
    }
    #[test]
    fn live_nested_root_deleted_with_parent() {
        let b = tmpdir("nestdel");
        let lib = b.join("lib");
        tree(&lib);
        put(&lib.join("Drums").join("Snares").join("deep.wav"), 1);
        let roots = normalize_roots(&[lib.clone(), lib.join("Drums").join("Snares")]);
        let ix = fscan(&roots);
        std::fs::remove_dir_all(lib.join("Drums")).unwrap();
        check(&b, &roots, &ix, &[(lib.join("Drums"), true)]);
    }

    /// Nested root whose spelling differs from the outer root's in an
    /// intermediate component (here the outer root's own name). A folder
    /// above the nested root is replaced (moved out, another moved in).
    #[test]
    fn live_nested_root_other_case_ancestor_replaced() {
        let b = tmpdir("nestanc");
        let lib = b.join("lib");
        put(&lib.join("sub").join("deep").join("d.wav"), 1);
        put(&lib.join("sub").join("mid.wav"), 1);
        let roots = normalize_roots(&[lib.clone(), b.join("LIB").join("sub").join("deep")]);
        assert_eq!(roots.len(), 2);
        let ix = fscan(&roots);
        // A same-named folder moved in over it.
        std::fs::rename(lib.join("sub"), b.join("old")).unwrap();
        put(&b.join("new").join("deep").join("d2.wav"), 1);
        std::fs::rename(b.join("new"), lib.join("sub")).unwrap();
        check(&b, &roots, &ix, &[(lib.join("sub"), true), (lib.join("sub"), true)]);
    }

    /// Same, but the only difference is the drive letter's case.
    #[cfg(windows)]
    #[test]
    fn live_nested_root_drive_letter_case() {
        let b = tmpdir("nestdrv");
        let s = b.to_string_lossy().into_owned();
        let lower_drive = PathBuf::from(format!("{}{}", s[..1].to_ascii_lowercase(), &s[1..]));
        let upper_drive = PathBuf::from(format!("{}{}", s[..1].to_ascii_uppercase(), &s[1..]));
        let lib = upper_drive.join("lib");
        put(&lib.join("sub").join("deep").join("d.wav"), 1);
        let roots = normalize_roots(&[lib.clone(), lower_drive.join("lib").join("sub").join("deep")]);
        let ix = fscan(&roots);
        std::fs::rename(lib.join("sub"), b.join("old")).unwrap();
        put(&b.join("new").join("deep").join("d.wav"), 1);
        std::fs::rename(b.join("new"), lib.join("sub")).unwrap();
        check(&b, &roots, &ix, &[(lib.join("sub"), true)]);
    }
}

#[cfg(test)]
mod live_update_bench {
    use super::*;
    fn synth(base: &Path, sub: &str) -> Index {
        let dirs: Vec<String> =
            (0..20_000).map(|d| base.join(sub).join(format!("d{d}")).to_string_lossy().into_owned()).collect();
        let entries: Vec<crate::index::RawEntry> = (0..1_000_000u32)
            .map(|i| crate::index::RawEntry { dir: i % 20_000, name: format!("Sound {i:07}.wav"), size: i as u64, mtime: 0 })
            .collect();
        Index::build(dirs, entries)
    }
    fn time(label: &str, ix: &Index, ev: &[(PathBuf, bool)], roots: &[PathBuf]) {
        let _ = apply_changes(ix, ev, roots);
        let mut best = std::time::Duration::MAX;
        let mut n = None;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            n = apply_changes(ix, ev, roots).map(|ix| ix.len());
            best = best.min(t.elapsed());
        }
        println!("live update {label}: {best:?} -> {n:?}");
    }
    #[test]
    #[ignore]
    fn bench_live_delete_20k_subfolders() {
        let base = std::env::temp_dir().join(format!("totems-livebench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let roots = vec![base.clone()];
        let ix = synth(&base, "Big");
        time("delete top only", &ix, &[(base.join("Big"), true)], &roots);
        let mut ev: Vec<(PathBuf, bool)> = (0..20_000).map(|d| (base.join("Big").join(format!("d{d}")), true)).collect();
        ev.push((base.join("Big"), true));
        time("delete, 20k folder events", &ix, &ev, &roots);
        std::fs::create_dir_all(base.join("big")).unwrap();
        time("case-only rename", &ix, &[(base.join("Big"), true), (base.join("big"), true)], &roots);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod review_tests2 {
    use super::*;
    /// A file added inside a folder, then the folder case-renamed, in one
    /// batch: the file's event carries the old parent spelling.
    #[test]
    fn live_file_added_then_parent_case_renamed() {
        let b = std::env::temp_dir().join(format!("totems-live-stalefile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        let r = b.join("lib");
        std::fs::create_dir_all(r.join("Drums")).unwrap();
        std::fs::write(r.join("Drums").join("Kick.wav"), b"x").unwrap();
        let roots = normalize_roots(std::slice::from_ref(&r));
        let full = |roots: &[PathBuf]| {
            let mut s = ScanSink::default();
            for x in roots {
                s.skip = nested_in(x, roots);
                s.scan(x, &mut |_| true);
            }
            Index::build(s.dirs, s.entries)
        };
        let ix = full(&roots);
        std::fs::write(r.join("Drums").join("New.wav"), b"x").unwrap();
        std::fs::rename(r.join("Drums"), r.join("drums")).unwrap();
        let ev = [(r.join("Drums").join("New.wav"), true), (r.join("Drums"), true), (r.join("drums"), true)];
        let inc = apply_changes(&ix, &ev, &roots).unwrap();
        let p = |ix: &Index| (0..ix.len()).map(|i| ix.full_path(i)).collect::<Vec<_>>();
        let (got, want) = (p(&inc), p(&full(&roots)));
        let _ = std::fs::remove_dir_all(&b);
        assert_eq!(got, want);
    }
}
