//! The in-memory audio file database.
//!
//! Everything is stored column-wise in flat buffers so that searching a
//! million names is a linear SIMD scan over contiguous memory. Entries are
//! pre-sorted by name, and rank tables for the other sort keys are built once,
//! so sorting results is a cheap integer sort.

use rayon::prelude::*;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Every extension we index. Playback support depends on the decoder, but
/// search covers them all.
pub const AUDIO_EXTS: &[&str] = &[
    "wav", "wave", "bwf", "rf64", "w64", "aif", "aiff", "aifc", "flac", "mp3", "mp2", "mp1", "ogg",
    "oga", "opus", "m4a", "m4b", "aac", "alac", "caf", "wma", "ape", "wv", "mka", "webm", "amr",
    "au", "snd", "dsf", "dff", "mpc", "tta", "ac3", "dts", "voc", "sd2", "3gp",
];

/// The extension id of an audio file name, or None if it isn't one. This is
/// the single gate for what gets indexed (full scans, folder rescans and live
/// updates all ask it), so anything rejected here never reaches search.
///
/// macOS AppleDouble companions (`._Kick.wav`: Finder metadata a Mac writes
/// next to each file on non-Mac drives) are rejected: they're 4 KB of
/// metadata with an audio extension, never audio.
pub fn audio_ext_id(name: &str) -> Option<u8> {
    if name.starts_with("._") {
        return None;
    }
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    if ext.len() > 5 {
        return None;
    }
    AUDIO_EXTS.iter().position(|e| e.eq_ignore_ascii_case(ext)).map(|i| i as u8)
}

/// Lower-cased path with all separators normalised to '/'.
pub fn norm_lower(s: &str) -> String {
    let mut l = s.to_lowercase();
    if std::path::MAIN_SEPARATOR == '\\' {
        l = l.replace('\\', "/");
    }
    l
}

#[derive(Clone, Debug)]
pub struct RawEntry {
    pub dir: u32,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub struct Index {
    pub dirs: Vec<Box<str>>,
    pub dirs_lower: Vec<Box<str>>,
    /// Original-case names, concatenated, separated by '\0'.
    pub names: Vec<u8>,
    pub name_off: Vec<u32>,
    /// Lower-case names, concatenated, separated by '\0'.
    pub lower: Vec<u8>,
    pub lower_off: Vec<u32>,
    pub dir: Vec<u32>,
    pub size: Vec<u64>,
    pub mtime: Vec<i64>,
    pub ext: Vec<u8>,
    pub rank_path: Vec<u32>,
    pub rank_size: Vec<u32>,
    pub rank_mtime: Vec<u32>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.dir.len()
    }

    #[inline]
    pub fn name(&self, i: usize) -> &str {
        let a = self.name_off[i] as usize;
        let b = self.name_off[i + 1] as usize - 1;
        // SAFETY: built from &str slices joined by '\0'.
        unsafe { std::str::from_utf8_unchecked(&self.names[a..b]) }
    }

    #[inline]
    pub fn name_lower(&self, i: usize) -> &[u8] {
        let a = self.lower_off[i] as usize;
        let b = self.lower_off[i + 1] as usize - 1;
        &self.lower[a..b]
    }

    #[inline]
    pub fn dir_str(&self, i: usize) -> &str {
        &self.dirs[self.dir[i] as usize]
    }

    pub fn full_path(&self, i: usize) -> PathBuf {
        Path::new(self.dir_str(i)).join(self.name(i))
    }

    /// Entry id for an exact path, if indexed.
    pub fn find(&self, path: &Path) -> Option<usize> {
        let name = path.file_name()?.to_str()?;
        let dir = norm_lower(path.parent()?.to_str()?);
        let lname = name.to_lowercase();
        let lb = lname.as_bytes();
        // Binary search the name-sorted entries.
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.name_lower(mid) < lb {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut i = lo;
        while i < self.len() && self.name_lower(i) == lb {
            if *self.dirs_lower[self.dir[i] as usize] == *dir {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Build from raw entries. Unused directories are compacted away.
    pub fn build(dirs: Vec<String>, mut entries: Vec<RawEntry>) -> Index {
        // Compact directories.
        let mut used = vec![false; dirs.len()];
        for e in &entries {
            used[e.dir as usize] = true;
        }
        let mut remap = vec![u32::MAX; dirs.len()];
        let mut new_dirs: Vec<Box<str>> = Vec::new();
        for (i, d) in dirs.into_iter().enumerate() {
            if used[i] {
                remap[i] = new_dirs.len() as u32;
                new_dirs.push(d.into_boxed_str());
            }
        }
        for e in &mut entries {
            e.dir = remap[e.dir as usize];
        }
        let dirs_lower: Vec<Box<str>> =
            new_dirs.par_iter().map(|d| norm_lower(d).into_boxed_str()).collect();

        // Sort by (lower name, lower dir, name).
        let lowers: Vec<String> = entries.par_iter().map(|e| e.name.to_lowercase()).collect();
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            lowers[a]
                .cmp(&lowers[b])
                .then_with(|| dirs_lower[entries[a].dir as usize].cmp(&dirs_lower[entries[b].dir as usize]))
                .then_with(|| entries[a].name.cmp(&entries[b].name))
        });
        // Drop exact duplicates (same dir + name).
        order.dedup_by(|b, a| {
            let (a, b) = (*a as usize, *b as usize);
            entries[a].dir == entries[b].dir && entries[a].name == entries[b].name
        });

        let n = order.len();
        let mut ix = Index {
            dirs: new_dirs,
            dirs_lower,
            names: Vec::with_capacity(n * 24),
            name_off: Vec::with_capacity(n + 1),
            lower: Vec::with_capacity(n * 24),
            lower_off: Vec::with_capacity(n + 1),
            dir: Vec::with_capacity(n),
            size: Vec::with_capacity(n),
            mtime: Vec::with_capacity(n),
            ext: Vec::with_capacity(n),
            ..Default::default()
        };
        for &o in &order {
            let e = &entries[o as usize];
            ix.name_off.push(ix.names.len() as u32);
            ix.names.extend_from_slice(e.name.as_bytes());
            ix.names.push(0);
            ix.lower_off.push(ix.lower.len() as u32);
            ix.lower.extend_from_slice(lowers[o as usize].as_bytes());
            ix.lower.push(0);
            ix.dir.push(e.dir);
            ix.size.push(e.size);
            ix.mtime.push(e.mtime);
            ix.ext.push(audio_ext_id(&e.name).unwrap_or(255));
        }
        ix.name_off.push(ix.names.len() as u32);
        ix.lower_off.push(ix.lower.len() as u32);

        let rank_of = |mut perm: Vec<u32>| {
            let mut rank = vec![0u32; perm.len()];
            for (r, &i) in perm.iter().enumerate() {
                rank[i as usize] = r as u32;
            }
            perm.clear();
            rank
        };
        let ids: Vec<u32> = (0..n as u32).collect();
        let (rp, (rs, rm)) = rayon::join(
            || {
                let mut p = ids.clone();
                p.par_sort_by(|&a, &b| {
                    ix.dirs_lower[ix.dir[a as usize] as usize]
                        .cmp(&ix.dirs_lower[ix.dir[b as usize] as usize])
                        .then(a.cmp(&b))
                });
                rank_of(p)
            },
            || {
                rayon::join(
                    || {
                        let mut p = ids.clone();
                        p.par_sort_by_key(|&i| (ix.size[i as usize], i));
                        rank_of(p)
                    },
                    || {
                        let mut p = ids.clone();
                        p.par_sort_by_key(|&i| (ix.mtime[i as usize], i));
                        rank_of(p)
                    },
                )
            },
        );
        ix.rank_path = rp;
        ix.rank_size = rs;
        ix.rank_mtime = rm;
        ix
    }

    /// Decompose back into raw entries (for incremental rebuilds).
    pub fn to_raw(&self) -> (Vec<String>, Vec<RawEntry>) {
        let dirs = self.dirs.iter().map(|d| d.to_string()).collect();
        let entries = (0..self.len())
            .into_par_iter()
            .map(|i| RawEntry {
                dir: self.dir[i],
                name: self.name(i).to_string(),
                size: self.size[i],
                mtime: self.mtime[i],
            })
            .collect();
        (dirs, entries)
    }

    // ---------------------------------------------------------------- cache

    /// v5: AppleDouble `._` files and `__MACOSX` folders are no longer
    /// indexed; bumping drops v4 caches that still contain them.
    const MAGIC: &'static [u8; 8] = b"TOTEMIX5";

    /// Persist the finished index column-by-column so loading is a memcpy,
    /// not a rebuild.
    pub fn save(&self, path: &Path, roots: &[PathBuf]) -> io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
            w.write_all(Self::MAGIC)?;
            write_u32(&mut w, roots.len() as u32)?;
            for r in roots {
                write_str(&mut w, &r.to_string_lossy())?;
            }
            for list in [&self.dirs, &self.dirs_lower] {
                write_u32(&mut w, list.len() as u32)?;
                for d in list.iter() {
                    write_str(&mut w, d)?;
                }
            }
            write_bytes(&mut w, &self.names)?;
            write_bytes(&mut w, &self.lower)?;
            for v in [&self.name_off, &self.lower_off, &self.dir, &self.rank_path, &self.rank_size, &self.rank_mtime] {
                write_u32(&mut w, v.len() as u32)?;
                for x in v.iter() {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
            write_u32(&mut w, self.size.len() as u32)?;
            for x in &self.size {
                w.write_all(&x.to_le_bytes())?;
            }
            for x in &self.mtime {
                w.write_all(&x.to_le_bytes())?;
            }
            write_bytes(&mut w, &self.ext)?;
            w.flush()?;
        }
        std::fs::rename(tmp, path)
    }

    /// Load a cache file; None if missing, corrupt, or built for other roots.
    pub fn load(path: &Path, roots: &[PathBuf]) -> Option<Index> {
        let mut data = Vec::new();
        BufReader::new(std::fs::File::open(path).ok()?).read_to_end(&mut data).ok()?;
        let mut r = Cursor { d: &data, p: 0 };
        if r.take(8)? != Self::MAGIC {
            return None;
        }
        let nroots = r.u32()? as usize;
        let mut cached_roots = Vec::new();
        for _ in 0..nroots {
            cached_roots.push(PathBuf::from(r.str()?));
        }
        if cached_roots != roots {
            return None;
        }
        let mut strs = || -> Option<Vec<Box<str>>> {
            let n = r.u32()? as usize;
            (0..n).map(|_| r.str().map(Box::from)).collect()
        };
        let dirs = strs()?;
        let dirs_lower = strs()?;
        let names = r.bytes()?.to_vec();
        let lower = r.bytes()?.to_vec();
        let mut u32s = || -> Option<Vec<u32>> {
            let n = r.u32()? as usize;
            Some(r.take(n * 4)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect())
        };
        let (name_off, lower_off, dir) = (u32s()?, u32s()?, u32s()?);
        let (rank_path, rank_size, rank_mtime) = (u32s()?, u32s()?, u32s()?);
        let n = r.u32()? as usize;
        let size = r.take(n * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        let mtime = r.take(n * 8)?.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect();
        let ext = r.bytes()?.to_vec();
        let ix = Index {
            dirs,
            dirs_lower,
            names,
            name_off,
            lower,
            lower_off,
            dir,
            size,
            mtime,
            ext,
            rank_path,
            rank_size,
            rank_mtime,
        };
        // Structural validation so a corrupt cache can never cause a panic.
        let ok = ix.name_off.len() == n + 1
            && ix.lower_off.len() == n + 1
            && [ix.dir.len(), ix.ext.len(), ix.rank_path.len(), ix.rank_size.len(), ix.rank_mtime.len()]
                .iter()
                .all(|&l| l == n)
            && ix.dirs.len() == ix.dirs_lower.len()
            && ix.dir.iter().all(|&d| (d as usize) < ix.dirs.len())
            && ix.name_off.windows(2).all(|w| w[0] < w[1])
            && ix.lower_off.windows(2).all(|w| w[0] < w[1])
            && ix.name_off.last().is_some_and(|&e| e as usize == ix.names.len())
            && ix.lower_off.last().is_some_and(|&e| e as usize == ix.lower.len())
            && std::str::from_utf8(&ix.names).is_ok()
            && [&ix.rank_path, &ix.rank_size, &ix.rank_mtime].iter().all(|v| v.iter().all(|&x| (x as usize) < n))
            && (0..n).all(|i| ix.names[ix.name_off[i + 1] as usize - 1] == 0 && ix.lower[ix.lower_off[i + 1] as usize - 1] == 0);
        ok.then_some(ix)
    }
}

fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    write_bytes(w, s.as_bytes())
}
fn write_bytes(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    write_u32(w, b.len() as u32)?;
    w.write_all(b)
}
struct Cursor<'a> {
    d: &'a [u8],
    p: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes()?).ok()
    }
}

// -------------------------------------------------------------------- scan

/// Directory names never descended into.
pub fn skip_dir(name: &str, parent: &Path) -> bool {
    matches!(
        name,
        ".git" | ".svn" | ".hg" | "node_modules" | "$RECYCLE.BIN" | "$Recycle.Bin"
            | "System Volume Information" | ".Trash" | ".Trashes" | ".cache" | ".npm"
            | ".cargo" | ".rustup" | "__pycache__" | ".Spotlight-V100" | ".fseventsd"
            | "WinSxS" | "Installer" | "__MACOSX"
    ) || (parent == Path::new("/")
        && matches!(name, "proc" | "sys" | "dev" | "run" | "snap" | "tmp" | "var" | "boot" | "lost+found"))
}

pub fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Accumulates scanned files; shared between full and partial scans.
#[derive(Default)]
pub struct ScanSink {
    pub dirs: Vec<String>,
    pub dir_ids: std::collections::HashMap<PathBuf, u32>,
    pub entries: Vec<RawEntry>,
}

impl ScanSink {
    pub fn from_raw(dirs: Vec<String>, entries: Vec<RawEntry>) -> Self {
        let dir_ids = dirs.iter().enumerate().map(|(i, d)| (PathBuf::from(d), i as u32)).collect();
        Self { dirs, dir_ids, entries }
    }

    pub fn dir_id(&mut self, dir: &Path) -> u32 {
        if let Some(&id) = self.dir_ids.get(dir) {
            return id;
        }
        let id = self.dirs.len() as u32;
        self.dirs.push(dir.to_string_lossy().into_owned());
        self.dir_ids.insert(dir.to_path_buf(), id);
        id
    }

    pub fn add_file(&mut self, path: &Path, md: &std::fs::Metadata) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return;
        };
        let dir = self.dir_id(parent);
        self.entries.push(RawEntry { dir, name: name.to_string(), size: md.len(), mtime: mtime_of(md) });
    }

    /// Recursively scan `root`. `progress` is called periodically and
    /// returns false to cancel; returns false if cancelled.
    pub fn scan(&mut self, root: &Path, progress: &mut dyn FnMut(&ScanSink) -> bool) -> bool {
        // Each file's (size, mtime) is attached to its entry inside jwalk's
        // parallel per-directory callback. jwalk's `DirEntry::metadata()` is a
        // separate `symlink_metadata` (one file open per file on Windows, on
        // the consuming thread); re-listing the directory with `std::fs` is
        // far cheaper there, because Windows returns size and mtime with each
        // entry (FindNextFile), so `metadata()` costs no syscall. Measured on
        // 79k files / 3.3k dirs on Windows: 2.78 s → 45 ms.
        let walker = jwalk::WalkDirGeneric::<((), Option<(u64, i64)>)>::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .sort(false)
            .process_read_dir(|_, parent, _, children| {
                children.retain(|c| match c {
                    Ok(e) => {
                        let name = e.file_name.to_string_lossy();
                        if e.file_type.is_dir() {
                            !skip_dir(&name, parent)
                        } else {
                            e.file_type.is_file() && audio_ext_id(&name).is_some()
                        }
                    }
                    Err(_) => false,
                });
                if !children.iter().any(|c| matches!(c, Ok(e) if e.file_type.is_file())) {
                    return;
                }
                let Ok(rd) = std::fs::read_dir(parent) else { return };
                let stats: std::collections::HashMap<_, _> = rd
                    .flatten()
                    .filter_map(|d| {
                        let md = d.metadata().ok()?;
                        md.is_file().then(|| (d.file_name(), (md.len(), mtime_of(&md))))
                    })
                    .collect();
                for e in children.iter_mut().flatten() {
                    if e.file_type.is_file() {
                        e.client_state = stats.get(&e.file_name).copied();
                    }
                }
            });
        let mut n = 0usize;
        for e in walker.into_iter().flatten() {
            if !e.file_type.is_file() {
                continue;
            }
            let Some(name) = e.file_name.to_str() else { continue };
            // None: the file vanished between the two listings.
            let Some((size, mtime)) = e.client_state else { continue };
            let dir = self.dir_id(&e.parent_path);
            self.entries.push(RawEntry { dir, name: name.to_string(), size, mtime });
            n += 1;
            if n % 2048 == 0 && !progress(self) {
                return false;
            }
        }
        progress(self)
    }
}

/// Sensible default roots: every local volume on Windows, home (+ mounted
/// volumes) elsewhere.
pub fn default_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        for c in b'C'..=b'Z' {
            let p = PathBuf::from(format!("{}:\\", c as char));
            if p.exists() {
                v.push(p);
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(h) = dirs::home_dir() {
            v.push(h);
        }
        for extra in ["/Volumes", "/media", "/mnt"] {
            let p = PathBuf::from(extra);
            if p.is_dir() {
                v.push(p);
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let dirs = vec!["/a/sfx".to_string(), "/b/Music".to_string(), "/unused".to_string()];
        let e = |dir, name: &str, size, mtime| RawEntry { dir, name: name.into(), size, mtime };
        Index::build(
            dirs,
            vec![
                e(0, "Whoosh_01.wav", 500, 3),
                e(1, "Song.mp3", 9000, 1),
                e(0, "door_slam.flac", 100, 2),
                e(1, "door_slam.flac", 100, 2),
                e(0, "Whoosh_01.wav", 500, 3),
            ],
        )
    }

    #[test]
    fn build_sorts_dedups_and_compacts() {
        let ix = sample();
        assert_eq!(ix.len(), 4);
        assert_eq!(ix.dirs.len(), 2);
        let names: Vec<_> = (0..ix.len()).map(|i| ix.name(i)).collect();
        assert_eq!(names, ["door_slam.flac", "door_slam.flac", "Song.mp3", "Whoosh_01.wav"]);
        assert_eq!(ix.name_lower(3), b"whoosh_01.wav");
        assert_eq!(ix.find(Path::new("/b/Music/door_slam.flac")), Some(1));
        assert_eq!(ix.find(Path::new("/b/Music/nope.wav")), None);
        // size ranks
        assert_eq!(ix.rank_size[2], 3);
    }

    #[test]
    fn macos_metadata_is_never_indexed() {
        assert_eq!(audio_ext_id("._Back Break.wav"), None);
        assert_eq!(audio_ext_id("._x.flac"), None);
        assert!(audio_ext_id("Back Break.wav").is_some());
        assert!(audio_ext_id("._").is_none());
        // Only the leading "._" marks AppleDouble.
        assert!(audio_ext_id("a._b.wav").is_some());
        assert!(audio_ext_id("_x.wav").is_some());
        assert!(audio_ext_id(".hidden.wav").is_some());
        assert!(skip_dir("__MACOSX", Path::new("/samples")));

        let dir = std::env::temp_dir().join(format!("totems-appledouble-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("__MACOSX")).unwrap();
        for f in ["Kick.wav", "._Kick.wav", "__MACOSX/._Kick.wav", "__MACOSX/Snare.wav"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let mut sink = ScanSink::default();
        assert!(sink.scan(&dir, &mut |_| true));
        let names: Vec<_> = sink.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Kick.wav"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_records_size_and_mtime() {
        let dir = std::env::temp_dir().join(format!("totems-scanmd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.wav"), vec![0u8; 1234]).unwrap();
        std::fs::write(dir.join("sub/b.flac"), vec![0u8; 77]).unwrap();
        std::fs::write(dir.join("sub/notes.txt"), b"x").unwrap();
        let mut sink = ScanSink::default();
        assert!(sink.scan(&dir, &mut |_| true));
        let mut got: Vec<_> = sink.entries.iter().map(|e| (e.name.as_str(), e.size, e.mtime)).collect();
        got.sort();
        let mt = |f: &str| mtime_of(&std::fs::metadata(dir.join(f)).unwrap());
        assert_eq!(got, [("a.wav", 1234, mt("a.wav")), ("b.flac", 77, mt("sub/b.flac"))]);
        assert!(got[0].2 > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_roundtrip() {
        let ix = sample();
        let dir = std::env::temp_dir().join(format!("totems-test-{}", std::process::id()));
        let p = dir.join("ix.bin");
        let roots = vec![PathBuf::from("/a")];
        ix.save(&p, &roots).unwrap();
        let back = Index::load(&p, &roots).unwrap();
        assert_eq!(back.len(), ix.len());
        assert_eq!(back.name(3), "Whoosh_01.wav");
        assert!(Index::load(&p, &[PathBuf::from("/other")]).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod scan_bench {
    /// TOTEMS_SCAN=/ cargo test --release scan_root -- --ignored --nocapture
    #[test]
    #[ignore]
    fn scan_root() {
        let root = std::path::PathBuf::from(std::env::var("TOTEMS_SCAN").unwrap_or("/".into()));
        let t = std::time::Instant::now();
        let mut sink = super::ScanSink::default();
        sink.scan(&root, &mut |_| true);
        let el = t.elapsed();
        let bytes: u64 = sink.entries.iter().map(|e| e.size).sum();
        let mtimes: i64 = sink.entries.iter().map(|e| e.mtime).sum();
        println!("scanned {:?}: {} audio files in {} dirs, {:?} (bytes {bytes}, mtime sum {mtimes})", root, sink.entries.len(), sink.dirs.len(), el);
    }
}
