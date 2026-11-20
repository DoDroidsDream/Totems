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

/// Lower case, one character at a time. `str::to_lowercase` depends on
/// context (Greek Σ becomes ς at the end of a word, σ elsewhere), so a
/// query `Σ` would never match the name `ΟΔΟΣ`.
/// Final ς folds to σ, as Unicode case folding does, so `οδος` matches too.
pub fn lower(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    s.chars().flat_map(char::to_lowercase).map(|c| if c == 'ς' { 'σ' } else { c }).collect()
}

/// Lower-cased path with all separators normalised to '/'.
pub fn norm_lower(s: &str) -> String {
    let mut l = lower(s);
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
    /// `dirs_lower`, sorted; built on first use (live updates), then kept.
    sorted_dirs: std::sync::OnceLock<Vec<u32>>,
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

    /// Folder ids in `dirs_lower` order, so the folders under a path are
    /// one contiguous run (binary search).
    pub fn sorted_dirs(&self) -> &[u32] {
        self.sorted_dirs.get_or_init(|| {
            let mut v: Vec<u32> = (0..self.dirs_lower.len() as u32).collect();
            v.par_sort_unstable_by(|&a, &b| self.dirs_lower[a as usize].cmp(&self.dirs_lower[b as usize]));
            v
        })
    }

    /// Entry id for a path, if indexed. Names compare case-insensitively
    /// (Windows paths), but an exact-case match wins: case-sensitive folders
    /// can hold `kick.wav` and `Kick.wav` side by side.
    pub fn find(&self, path: &Path) -> Option<usize> {
        let name = path.file_name()?.to_str()?;
        let (mut exact_name, mut first) = (None, None);
        for i in self.find_all(path) {
            if self.name(i) == name {
                if self.is_exactly(i, path) {
                    return Some(i);
                }
                exact_name.get_or_insert(i);
            }
            first.get_or_insert(i);
        }
        exact_name.or(first)
    }

    /// The entry whose folder and name match `path` exactly, case included.
    pub fn find_exact(&self, path: &Path) -> Option<usize> {
        self.find_all(path).find(|&i| self.is_exactly(i, path))
    }

    fn is_exactly(&self, i: usize, path: &Path) -> bool {
        path.file_name().and_then(|n| n.to_str()) == Some(self.name(i)) && path.parent() == Some(Path::new(self.dir_str(i)))
    }

    /// Every entry whose path equals `path` ignoring case.
    pub fn find_all(&self, path: &Path) -> impl Iterator<Item = usize> + '_ {
        let key = path.file_name().and_then(|n| n.to_str()).zip(path.parent().and_then(|p| p.to_str()));
        let valid = key.is_some();
        let (lname, dir) = key.map(|(n, d)| (lower(n), norm_lower(d))).unwrap_or_default();
        // Binary search the name-sorted entries.
        let (mut lo, mut hi) = (0usize, if valid { self.len() } else { 0 });
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.name_lower(mid) < lname.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let end = if valid { self.len() } else { lo };
        (lo..end)
            .take_while(move |&i| self.name_lower(i) == lname.as_bytes())
            .filter(move |&i| *self.dirs_lower[self.dir[i] as usize] == *dir)
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
        // Lower-cased names in one buffer, not a String each (1M allocations).
        let (lbuf, loff) = {
            let parts: Vec<(Vec<u8>, Vec<u32>)> = entries
                .par_chunks(16 * 1024)
                .map(|c| {
                    let mut b = Vec::with_capacity(c.iter().map(|e| e.name.len()).sum::<usize>() + 64);
                    let mut o = Vec::with_capacity(c.len());
                    for e in c {
                        o.push(b.len() as u32);
                        if e.name.is_ascii() {
                            b.extend(e.name.bytes().map(|x| x.to_ascii_lowercase()));
                        } else {
                            b.extend_from_slice(lower(&e.name).as_bytes());
                        }
                    }
                    (b, o)
                })
                .collect();
            let total: usize = parts.iter().map(|p| p.0.len()).sum();
            let mut lbuf = Vec::with_capacity(total);
            let mut loff = Vec::with_capacity(entries.len() + 1);
            for (b, o) in parts {
                let base = lbuf.len() as u32;
                loff.extend(o.iter().map(|x| x + base));
                lbuf.extend_from_slice(&b);
            }
            loff.push(lbuf.len() as u32);
            (lbuf, loff)
        };
        let lowers = |i: usize| &lbuf[loff[i] as usize..loff[i + 1] as usize];
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            lowers(a)
                .cmp(lowers(b))
                .then_with(|| dirs_lower[entries[a].dir as usize].cmp(&dirs_lower[entries[b].dir as usize]))
                .then_with(|| entries[a].name.cmp(&entries[b].name))
        });
        // Drop exact duplicates (same dir + name).
        order.dedup_by(|b, a| {
            let (a, b) = (*a as usize, *b as usize);
            entries[a].dir == entries[b].dir && entries[a].name == entries[b].name
        });

        let n = order.len();
        // Exact sizes, so the buffers never grow (doubling) past them.
        let name_bytes: usize = order.par_iter().map(|&o| entries[o as usize].name.len() + 1).sum();
        let lower_bytes: usize = order.par_iter().map(|&o| lowers(o as usize).len() + 1).sum();
        let mut ix = Index {
            dirs: new_dirs,
            dirs_lower,
            names: Vec::with_capacity(name_bytes),
            name_off: Vec::with_capacity(n + 1),
            lower: Vec::with_capacity(lower_bytes),
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
            ix.lower.extend_from_slice(lowers(o as usize));
            ix.lower.push(0);
            ix.dir.push(e.dir);
            ix.size.push(e.size);
            ix.mtime.push(e.mtime);
            ix.ext.push(audio_ext_id(&e.name).unwrap_or(255));
        }
        ix.name_off.push(ix.names.len() as u32);
        ix.lower_off.push(ix.lower.len() as u32);

        // The inputs are dead: free them before the rank temporaries.
        drop(order);
        drop(lbuf);
        drop(loff);
        drop(entries);
        ix.compute_ranks();
        ix
    }

    /// Permutation ranks for the Path, Size and Modified sorts. Integer keys
    /// only: folders are ranked once (equal lower-case folders share a rank).
    fn compute_ranks(&mut self) {
        let n = self.len();
        let mut order: Vec<u32> = (0..self.dirs_lower.len() as u32).collect();
        order.par_sort_unstable_by(|&a, &b| self.dirs_lower[a as usize].cmp(&self.dirs_lower[b as usize]));
        let mut dir_rank = vec![0u32; order.len()];
        let mut r = 0u32;
        for (k, &d) in order.iter().enumerate() {
            if k > 0 && self.dirs_lower[d as usize] != self.dirs_lower[order[k - 1] as usize] {
                r = k as u32;
            }
            dir_rank[d as usize] = r;
        }
        let rank_of = |perm: Vec<u32>| {
            let mut rank = vec![0u32; perm.len()];
            for (r, &i) in perm.iter().enumerate() {
                rank[i as usize] = r as u32;
            }
            rank
        };
        let ids = || (0..n as u32).into_par_iter();
        let (rp, (rs, rm)) = rayon::join(
            || {
                let mut k: Vec<u64> = ids().map(|i| ((dir_rank[self.dir[i as usize] as usize] as u64) << 32) | i as u64).collect();
                k.par_sort_unstable();
                rank_of(k.into_iter().map(|k| k as u32).collect())
            },
            || {
                rayon::join(
                    || {
                        let mut k: Vec<(u64, u32)> = ids().map(|i| (self.size[i as usize], i)).collect();
                        k.par_sort_unstable();
                        rank_of(k.into_iter().map(|k| k.1).collect())
                    },
                    || {
                        let mut k: Vec<(i64, u32)> = ids().map(|i| (self.mtime[i as usize], i)).collect();
                        k.par_sort_unstable();
                        rank_of(k.into_iter().map(|k| k.1).collect())
                    },
                )
            },
        );
        self.rank_path = rp;
        self.rank_size = rs;
        self.rank_mtime = rm;
    }

    /// A copy without the entries `drop` picks and with `add` merged in:
    /// the same index `build` would make from the remaining entries, but the
    /// kept ones are already in order, so only the new ones are lower-cased
    /// and sorted, and the rest is one merge. `add` holds (folder, entry);
    /// the entry's own `dir` is ignored.
    pub fn with_changes(&self, drop: impl Fn(usize) -> bool + Sync, add: Vec<(String, RawEntry)>) -> Index {
        // Folders: the old ones keep their ids; new ones are appended.
        let mut dirs: Vec<Box<str>> = self.dirs.clone();
        let mut dirs_lower: Vec<Box<str>> = self.dirs_lower.clone();
        let mut added: Vec<(RawEntry, String)> = Vec::with_capacity(add.len());
        if !add.is_empty() {
            let mut ids: std::collections::HashMap<Box<str>, u32> =
                self.dirs.iter().enumerate().map(|(i, d)| (d.clone(), i as u32)).collect();
            for (d, e) in add {
                let dir = *ids.entry(d.as_str().into()).or_insert_with(|| {
                    dirs.push(d.as_str().into());
                    dirs_lower.push(norm_lower(&d).into_boxed_str());
                    (dirs.len() - 1) as u32
                });
                let l = lower(&e.name);
                added.push((RawEntry { dir, ..e }, l));
            }
            added.sort_by(|(a, al), (b, bl)| {
                al.cmp(bl)
                    .then_with(|| dirs_lower[a.dir as usize].cmp(&dirs_lower[b.dir as usize]))
                    .then_with(|| a.name.cmp(&b.name))
            });
            added.dedup_by(|b, a| a.0.dir == b.0.dir && a.0.name == b.0.name);
        }
        let keep: Vec<bool> = (0..self.len()).into_par_iter().map(|i| !drop(i)).collect();

        // Merge the kept entries (in order) with the sorted new ones.
        let n_keep = keep.iter().filter(|k| **k).count();
        let n = n_keep + added.len();
        let mut ix = Index {
            names: Vec::with_capacity(self.names.len() + added.len() * 24),
            name_off: Vec::with_capacity(n + 1),
            lower: Vec::with_capacity(self.lower.len() + added.len() * 24),
            lower_off: Vec::with_capacity(n + 1),
            dir: Vec::with_capacity(n),
            size: Vec::with_capacity(n),
            mtime: Vec::with_capacity(n),
            ext: Vec::with_capacity(n),
            ..Default::default()
        };
        let push = |ix: &mut Index, name: &[u8], low: &[u8], dir: u32, size: u64, mtime: i64, ext: u8| {
            ix.name_off.push(ix.names.len() as u32);
            ix.names.extend_from_slice(name);
            ix.names.push(0);
            ix.lower_off.push(ix.lower.len() as u32);
            ix.lower.extend_from_slice(low);
            ix.lower.push(0);
            ix.dir.push(dir);
            ix.size.push(size);
            ix.mtime.push(mtime);
            ix.ext.push(ext);
        };
        let mut j = 0;
        for i in (0..self.len()).filter(|&i| keep[i]) {
            let (name, low, d) = (self.name(i), self.name_lower(i), self.dir[i]);
            while j < added.len() {
                let (e, l) = &added[j];
                let ord = l
                    .as_bytes()
                    .cmp(low)
                    .then_with(|| dirs_lower[e.dir as usize].cmp(&dirs_lower[d as usize]))
                    .then_with(|| e.name.as_str().cmp(name));
                if ord == std::cmp::Ordering::Greater {
                    break;
                }
                push(&mut ix, e.name.as_bytes(), l.as_bytes(), e.dir, e.size, e.mtime, audio_ext_id(&e.name).unwrap_or(255));
                j += 1;
            }
            // The same file again: the new entry (just pushed) wins.
            let dup = ix.dir.last() == Some(&d) && ix.name_off.last().is_some_and(|&o| {
                &ix.names[o as usize..ix.names.len() - 1] == name.as_bytes()
            });
            if !dup {
                push(&mut ix, name.as_bytes(), low, d, self.size[i], self.mtime[i], self.ext[i]);
            }
        }
        for (e, l) in &added[j..] {
            push(&mut ix, e.name.as_bytes(), l.as_bytes(), e.dir, e.size, e.mtime, audio_ext_id(&e.name).unwrap_or(255));
        }
        ix.name_off.push(ix.names.len() as u32);
        ix.lower_off.push(ix.lower.len() as u32);

        // Drop folders nothing uses any more.
        let mut used = vec![false; dirs.len()];
        for &d in &ix.dir {
            used[d as usize] = true;
        }
        let mut remap = vec![u32::MAX; dirs.len()];
        for (i, (d, l)) in dirs.into_iter().zip(dirs_lower).enumerate() {
            if used[i] {
                remap[i] = ix.dirs.len() as u32;
                ix.dirs.push(d);
                ix.dirs_lower.push(l);
            }
        }
        ix.dir.iter_mut().for_each(|d| *d = remap[*d as usize]);
        ix.compute_ranks();
        ix
    }

    /// Decompose back into raw entries.
    #[cfg(test)]
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
    /// v6: names and folders are lower-cased per character (`lower`).
    const MAGIC: &'static [u8; 8] = b"TOTEMIX6";

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
        // Column by column straight from the file: holding the whole file
        // and the columns at once doubled the startup peak.
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        Self::parse(BufReader::with_capacity(1 << 20, file), len, roots)
    }

    /// The cache format from any reader holding `len` bytes.
    fn parse(r: impl Read, len: u64, roots: &[PathBuf]) -> Option<Index> {
        let mut r = Reader { r, left: len };
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
            (0..n).map(|_| r.str().map(String::into_boxed_str)).collect()
        };
        let dirs = strs()?;
        let dirs_lower = strs()?;
        let names = r.bytes()?;
        let lower = r.bytes()?;
        let mut u32s = || -> Option<Vec<u32>> {
            let n = r.u32()? as usize;
            r.column(n, u32::from_le_bytes)
        };
        let (name_off, lower_off, dir) = (u32s()?, u32s()?, u32s()?);
        let (rank_path, rank_size, rank_mtime) = (u32s()?, u32s()?, u32s()?);
        let n = r.u32()? as usize;
        let size = r.column(n, u64::from_le_bytes)?;
        let mtime = r.column(n, i64::from_le_bytes)?;
        let ext = r.bytes()?;
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
            sorted_dirs: Default::default(),
        };
        // Structural validation so a corrupt cache can never cause a panic.
        let ok = ix.name_off.len() == n + 1
            // Names start right after a ' ' (or at 0), so each one is a
            // whole UTF-8 string: `name()` relies on it.
            && ix.name_off.first() == Some(&0)
            && ix.lower_off.first() == Some(&0)
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
/// Reads the cache file. Every length is checked against the bytes left in
/// the file before anything is allocated, so a corrupt length can't ask for
/// gigabytes.
struct Reader<R> {
    r: R,
    left: u64,
}

impl<R: Read> Reader<R> {
    fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        if n as u64 > self.left {
            return None;
        }
        let mut v = vec![0u8; n];
        self.r.read_exact(&mut v).ok()?;
        self.left -= n as u64;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?).ok()
    }
    /// `n` little-endian values of `W` bytes, through a 64 KB buffer.
    fn column<T, const W: usize>(&mut self, n: usize, f: fn([u8; W]) -> T) -> Option<Vec<T>> {
        let total = n.checked_mul(W)?;
        if total as u64 > self.left {
            return None;
        }
        let mut out = Vec::with_capacity(n);
        let mut buf = vec![0u8; (64 * 1024 / W) * W];
        let mut rem = total;
        while rem > 0 {
            let k = rem.min(buf.len());
            self.r.read_exact(&mut buf[..k]).ok()?;
            out.extend(buf[..k].chunks_exact(W).map(|c| f(c.try_into().unwrap())));
            rem -= k;
        }
        self.left -= total as u64;
        Some(out)
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
    pub fn dir_id(&mut self, dir: &Path) -> u32 {
        if let Some(&id) = self.dir_ids.get(dir) {
            return id;
        }
        let id = self.dirs.len() as u32;
        self.dirs.push(dir.to_string_lossy().into_owned());
        self.dir_ids.insert(dir.to_path_buf(), id);
        id
    }

    /// Recursively scan `root`. `progress` is called periodically and
    /// returns false to cancel; returns false if cancelled.
    /// Recursively scan `root`. `progress` is called periodically and
    /// returns false to cancel; returns false if cancelled.
    ///
    /// Folders are listed in parallel (rayon), each exactly once: on
    /// Windows a listing carries every file's size and mtime
    /// (FindNextFile), so `DirEntry::metadata` costs no syscall. (jwalk
    /// dropped that, so every folder with audio was listed twice.)
    pub fn scan(&mut self, root: &Path, progress: &mut dyn FnMut(&ScanSink) -> bool) -> bool {
        let (tx, rx) = crossbeam_channel::unbounded::<(PathBuf, Vec<RawEntry>)>();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut cancelled = false;
        std::thread::scope(|s| {
            let stop = &stop;
            s.spawn(move || rayon::scope(|sc| walk_dir(sc, root.to_path_buf(), tx, stop)));
            let mut n = 0usize;
            // Ends when every walker has finished (all senders dropped).
            for (dir, files) in rx.iter() {
                if cancelled {
                    continue;
                }
                let id = self.dir_id(&dir);
                for e in files {
                    self.entries.push(RawEntry { dir: id, ..e });
                    n += 1;
                    if n.is_multiple_of(2048) && !progress(self) {
                        cancelled = true;
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        !cancelled && progress(self)
    }
}

/// Every file under `dir` (benchmarks and tools).
#[cfg(test)]
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => out.push(e.path()),
                _ => {}
            }
        }
    }
    out
}

/// List `dir`, send its audio files, and walk its subfolders in parallel.
/// Links (symlinks, junctions) are never followed.
fn walk_dir<'s>(
    sc: &rayon::Scope<'s>,
    dir: PathBuf,
    tx: crossbeam_channel::Sender<(PathBuf, Vec<RawEntry>)>,
    stop: &'s std::sync::atomic::AtomicBool,
) {
    if stop.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut files = Vec::new();
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let name = e.file_name();
        if t.is_dir() {
            if !skip_dir(&name.to_string_lossy(), &dir) {
                let tx = tx.clone();
                let sub = dir.join(&name);
                sc.spawn(move |sc| walk_dir(sc, sub, tx, stop));
            }
        } else if t.is_file()
            && let Some(n) = name.to_str()
            && audio_ext_id(n).is_some()
            && let Ok(md) = e.metadata()
        {
            files.push(RawEntry { dir: 0, name: n.to_string(), size: md.len(), mtime: mtime_of(&md) });
        }
    }
    if !files.is_empty() {
        let _ = tx.send((dir, files));
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

    /// `with_changes` (the live-update merge) gives exactly what a full
    /// `build` of the same entries gives: every column, order and rank.
    #[test]
    fn with_changes_matches_full_build() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let words = ["Kick", "kick", "SNARE", "ΟΔΟΣ", "café", "Door Slam", "a", "Zz", "ß", "İx", "door slam"];
        let dirs: Vec<String> = ["/s/Drums", "/s/drums2", "/s/Foley/Doors", "/x", "/ü/Ö", "/s/Drums/Sub"].iter().map(|s| s.to_string()).collect();
        let entry = |rnd: &mut dyn FnMut(u64) -> u64| RawEntry {
            dir: rnd(dirs.len() as u64) as u32,
            name: format!("{} {}.{}", words[rnd(words.len() as u64) as usize], rnd(40), ["wav", "WAV", "flac", "mp3"][rnd(4) as usize]),
            size: rnd(5),
            mtime: rnd(5) as i64 - 2,
        };
        for round in 0..300 {
            let base: Vec<RawEntry> = (0..rnd(60)).map(|_| entry(&mut rnd)).collect();
            let ix = Index::build(dirs.clone(), base);
            let drop: Vec<bool> = (0..ix.len()).map(|_| rnd(4) == 0).collect();
            let new_dirs = ["/s/Drums", "/new/Dir", "/x"];
            // Exact duplicates (same folder and name) are left out: `build`
            // keeps an arbitrary one of them; the merge keeps the new one
            // (checked below).
            let mut seen: std::collections::HashSet<(String, String)> =
                (0..ix.len()).filter(|&i| !drop[i]).map(|i| (ix.dir_str(i).to_string(), ix.name(i).to_string())).collect();
            let add: Vec<(String, RawEntry)> = (0..rnd(8))
                .map(|_| (new_dirs[rnd(3) as usize].to_string(), entry(&mut rnd)))
                .filter(|(d, e)| seen.insert((d.clone(), e.name.clone())))
                .collect();
            let got = ix.with_changes(|i| drop[i], add.clone());
            // The same through a full build.
            let (mut d, e) = ix.to_raw();
            let mut all: Vec<RawEntry> = e.into_iter().enumerate().filter(|(i, _)| !drop[*i]).map(|(_, e)| e).collect();
            for (dir, mut e) in add {
                e.dir = d.iter().position(|x| *x == dir).unwrap_or_else(|| {
                    d.push(dir.clone());
                    d.len() - 1
                }) as u32;
                all.push(e);
            }
            let want = Index::build(d, all);
            let path = |ix: &Index, i: usize| (ix.dir_str(i).to_string(), ix.name(i).to_string(), ix.size[i], ix.mtime[i]);
            assert_eq!(got.len(), want.len(), "round {round}");
            for i in 0..got.len() {
                assert_eq!(path(&got, i), path(&want, i), "round {round} entry {i}");
                assert_eq!(got.name_lower(i), want.name_lower(i));
                assert_eq!(got.ext[i], want.ext[i]);
            }
            // Ranks order the same entries the same way.
            for (a, b) in [(&got.rank_path, &want.rank_path), (&got.rank_size, &want.rank_size), (&got.rank_mtime, &want.rank_mtime)] {
                assert_eq!(a, b, "round {round}: ranks");
            }
            let mut gd: Vec<_> = got.dirs.iter().zip(&got.dirs_lower).collect();
            let mut wd: Vec<_> = want.dirs.iter().zip(&want.dirs_lower).collect();
            gd.sort();
            wd.sort();
            assert_eq!(gd, wd, "round {round}: folders");
        }
        // A file that's already indexed, added again: the new size wins.
        let ix = Index::build(vec!["/d".into()], vec![RawEntry { dir: 0, name: "a.wav".into(), size: 1, mtime: 0 }]);
        let got = ix.with_changes(|_| false, vec![("/d".into(), RawEntry { dir: 9, name: "a.wav".into(), size: 2, mtime: 0 })]);
        assert_eq!((got.len(), got.size[0]), (1, 2));
    }

    /// The walker never follows links: a junction back to the root neither
    /// loops nor duplicates entries.
    #[cfg(windows)]
    #[test]
    fn scan_skips_junction_loops() {
        let d = std::env::temp_dir().join(format!("totems-jloop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a").join("x.wav"), b"x").unwrap();
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(d.join("a").join("loop"))
            .arg(&d)
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "mklink /J failed");
        let mut s = ScanSink::default();
        assert!(s.scan(&d, &mut |_| true));
        let _ = std::fs::remove_dir(d.join("a").join("loop"));
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(s.entries.len(), 1);
    }

    /// B46: the exact-case preference checked only the name, so with
    /// case-sensitive sibling folders `Kicks` and `kicks`, a change to one
    /// file could remove the other's.
    #[test]
    fn find_prefers_exact_case_folder_too() {
        let dirs = vec!["/s/Kicks".to_string(), "/s/kicks".to_string()];
        let e = |dir| RawEntry { dir, name: "a.wav".into(), size: 1, mtime: 1 };
        let ix = Index::build(dirs, vec![e(0), e(1)]);
        for want in ["/s/Kicks", "/s/kicks"] {
            let p = Path::new(want).join("a.wav");
            assert_eq!(ix.find(&p).map(|i| ix.dir_str(i).to_string()).as_deref(), Some(want), "find({p:?})");
            assert_eq!(ix.find_exact(&p).map(|i| ix.dir_str(i).to_string()).as_deref(), Some(want));
        }
        assert_eq!(ix.find_exact(Path::new("/s/KICKS/a.wav")), None);
        assert!(ix.find(Path::new("/s/KICKS/a.wav")).is_some(), "case-insensitive fallback");
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

    /// B9: a crafted cache passed validation with its first name offset
    /// inside a multi-byte character; `name()` (from_utf8_unchecked) then
    /// returned invalid UTF-8.
    #[test]
    fn cache_name_offsets_must_start_at_zero() {
        // A cache whose first name offset is 1, not 0, pointing into the middle
        // of "é" (C3 A9). Every other check passes.
        let mut f = Vec::new();
        let u32le = |f: &mut Vec<u8>, v: u32| f.extend_from_slice(&v.to_le_bytes());
        let bytes = |f: &mut Vec<u8>, b: &[u8]| {
            f.extend_from_slice(&(b.len() as u32).to_le_bytes());
            f.extend_from_slice(b);
        };
        f.extend_from_slice(super::Index::MAGIC);
        u32le(&mut f, 1);
        bytes(&mut f, b"/a"); // roots
        u32le(&mut f, 1);
        bytes(&mut f, b"/a"); // dirs
        u32le(&mut f, 1);
        bytes(&mut f, b"/a"); // dirs_lower
        bytes(&mut f, "é.wav\0".as_bytes()); // names
        bytes(&mut f, "é.wav\0".as_bytes()); // lower
        for v in [&[1u32, 7][..], &[1, 7], &[0], &[0], &[0], &[0]] {
            u32le(&mut f, v.len() as u32);
            for &x in v {
                u32le(&mut f, x);
            }
        }
        u32le(&mut f, 1);
        f.extend_from_slice(&1u64.to_le_bytes()); // size
        f.extend_from_slice(&1i64.to_le_bytes()); // mtime
        bytes(&mut f, &[0]); // ext
        let d = std::env::temp_dir().join(format!("totems-midchar-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("ix.bin");
        std::fs::write(&p, &f).unwrap();
        let ix = super::Index::load(&p, &[std::path::PathBuf::from("/a")]);
        let _ = std::fs::remove_dir_all(d);
        assert!(ix.is_none(), "a cache whose first name starts mid-character was accepted");
    }
}

#[cfg(test)]
#[path = "index_fuzz.rs"]
mod fuzz;
