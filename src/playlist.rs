//! Playlists: one `.m3u8` per playlist (a full path per line), in a folder
//! the user picks (`%APPDATA%\Totems\Playlists` by default). REAPER, VLC and
//! foobar2000 open the same files.
//!
//! The UI keeps the playlists in memory and edits them there at once; one
//! background thread does every read and write, in order, so a reload
//! always sees the writes queued before it and the UI never waits on a
//! slow (synced) folder.

use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlist {
    /// The file's name without its extension.
    pub name: String,
    /// The file it was read from (and is written back to).
    pub file: PathBuf,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Playlists {
    pub dir: PathBuf,
    /// Sorted by name, ignoring case.
    pub lists: Vec<Playlist>,
}

/// `%APPDATA%\Totems\Playlists`.
pub fn default_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("Totems").join("Playlists")
}

/// Path key: case and separators folded, as Windows compares paths.
pub fn key(p: &Path) -> String {
    crate::index::norm_lower(&p.to_string_lossy())
}

/// A playlist name made safe as a file name: characters Windows forbids,
/// and `;` (it separates names in `pl:A;B`), become spaces; ends trimmed.
pub fn clean_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if "<>:\"/\\|?*;".contains(c) || c.is_control() { ' ' } else { c }).collect();
    s.trim().trim_end_matches('.').trim().to_string()
}

/// The paths in an M3U / M3U8 file: comments (`#…`) and blank lines
/// skipped, relative paths taken from the playlist's folder.
pub fn parse_m3u(text: &str, base: &Path) -> Vec<PathBuf> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let p = PathBuf::from(l);
            if p.is_absolute() || l.starts_with(['\\', '/']) { p } else { base.join(p) }
        })
        .collect()
}

pub fn to_m3u(paths: &[PathBuf]) -> String {
    let mut s = String::from("#EXTM3U\r\n");
    for p in paths {
        s += &p.to_string_lossy();
        s += "\r\n";
    }
    s
}

impl Playlists {
    /// Every `.m3u8` / `.m3u` in `dir`. A missing folder is no playlists.
    pub fn load(dir: &Path) -> Playlists {
        let mut lists = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let file = e.path();
                let ext = file.extension().and_then(|x| x.to_str()).unwrap_or("").to_ascii_lowercase();
                if ext != "m3u8" && ext != "m3u" {
                    continue;
                }
                let Some(name) = file.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
                let Ok(bytes) = std::fs::read(&file) else { continue };
                let paths = parse_m3u(&String::from_utf8_lossy(&bytes), dir);
                lists.push(Playlist { name, file, paths });
            }
        }
        let mut pls = Playlists { dir: dir.to_path_buf(), lists };
        pls.sort();
        // `a.m3u` and `a.m3u8` side by side: keep the first only, so a name
        // means one playlist.
        pls.lists.dedup_by(|a, b| crate::index::lower(&a.name) == crate::index::lower(&b.name));
        pls
    }

    fn sort(&mut self) {
        self.lists.sort_by_cached_key(|l| (crate::index::lower(&l.name), l.file.clone()));
    }

    /// The playlist called `name`, ignoring case.
    pub fn position(&self, name: &str) -> Option<usize> {
        let n = crate::index::lower(name.trim());
        self.lists.iter().position(|l| crate::index::lower(&l.name) == n)
    }

    pub fn get(&self, name: &str) -> Option<&Playlist> {
        self.position(name).map(|i| &self.lists[i])
    }

    /// The playlist called `name`, made (empty) if there's none yet.
    pub fn get_or_create(&mut self, name: &str) -> &mut Playlist {
        let i = match self.position(name) {
            Some(i) => i,
            None => {
                let file = self.dir.join(format!("{name}.m3u8"));
                self.lists.push(Playlist { name: name.to_string(), file, paths: Vec::new() });
                self.sort();
                self.position(name).expect("just added")
            }
        };
        &mut self.lists[i]
    }

    /// Every path in every playlist (for the indexer: files outside the
    /// indexed folders).
    pub fn all_paths(&self) -> Vec<PathBuf> {
        self.lists.iter().flat_map(|l| l.paths.iter().cloned()).collect()
    }

    /// Names starting with `prefix` (ignoring case), in order.
    pub fn names_starting(&self, prefix: &str) -> Vec<&str> {
        let p = crate::index::lower(prefix);
        self.lists.iter().map(|l| l.name.as_str()).filter(|n| crate::index::lower(n).starts_with(&p)).collect()
    }
}

impl Playlist {
    /// Append paths not already in the list. Returns how many were added.
    pub fn add(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let mut have: std::collections::HashSet<String> = self.paths.iter().map(|p| key(p)).collect();
        let before = self.paths.len();
        for p in paths {
            if have.insert(key(&p)) {
                self.paths.push(p);
            }
        }
        self.paths.len() - before
    }

    /// Remove `path` (any spelling). Returns where it was.
    pub fn remove(&mut self, path: &Path) -> Option<usize> {
        let k = key(path);
        let i = self.paths.iter().position(|p| key(p) == k)?;
        self.paths.remove(i);
        Some(i)
    }

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        let k = key(path);
        self.paths.iter().position(|p| key(p) == k)
    }
}

// ------------------------------------------------------------------ I/O

enum Job {
    /// Read the folder; the tag comes back with the result.
    Load(PathBuf, u64),
    Save(PathBuf, String),
}

pub enum Done {
    Loaded(u64, Arc<Playlists>),
    Failed(String),
}

/// The playlist reader/writer thread.
pub struct Io {
    tx: Sender<Job>,
    pub rx: Receiver<Done>,
}

impl Io {
    pub fn start(repaint: Arc<dyn Fn() + Send + Sync>) -> Io {
        let (tx, jobs) = crossbeam_channel::unbounded::<Job>();
        let (done, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("playlists".into())
            .spawn(move || {
                for job in jobs {
                    let out = match job {
                        Job::Load(dir, tag) => Done::Loaded(tag, Arc::new(Playlists::load(&dir))),
                        Job::Save(file, text) => match write_atomic(&file, &text) {
                            Ok(()) => continue,
                            Err(e) => Done::Failed(format!("Couldn't save {}: {e}", file.display())),
                        },
                    };
                    if done.send(out).is_err() {
                        return;
                    }
                    repaint();
                }
            })
            .expect("spawn playlists");
        Io { tx, rx }
    }

    pub fn load(&self, dir: PathBuf, tag: u64) {
        let _ = self.tx.send(Job::Load(dir, tag));
    }

    pub fn save(&self, list: &Playlist) {
        let _ = self.tx.send(Job::Save(list.file.clone(), to_m3u(&list.paths)));
    }
}

/// Write via a temp file and a rename, so a crash or a full disk never
/// leaves half a playlist.
fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, file)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("totems-pl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn m3u_round_trips_and_skips_comments() {
        let base = Path::new(if cfg!(windows) { r"C:\pl" } else { "/pl" });
        let abs = if cfg!(windows) { r"F:\sfx\Door Slam.wav" } else { "/sfx/Door Slam.wav" };
        let text = format!("\u{feff}#EXTM3U\r\n#EXTINF:1,Door\r\n{abs}\r\n\r\nsub/kick.wav\n");
        let got = parse_m3u(&text, base);
        assert_eq!(got, [PathBuf::from(abs), base.join("sub/kick.wav")]);
        assert_eq!(parse_m3u(&to_m3u(&got), base), got);
    }

    #[test]
    fn names_are_safe_file_names() {
        assert_eq!(clean_name("  Impacts / Big;Ones?  "), "Impacts   Big Ones");
        assert_eq!(clean_name("x."), "x");
        assert_eq!(clean_name("::"), "");
    }

    #[test]
    fn add_skips_duplicates_any_spelling() {
        let mut l = Playlist::default();
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(l.add([p("/a/x.wav"), p("/a/y.wav"), p("/a/x.wav")]), 2);
        if cfg!(windows) {
            assert_eq!(l.add([p("/A/X.WAV")]), 0);
        }
        assert_eq!(l.remove(&p("/a/x.wav")), Some(0));
        assert_eq!(l.paths, [p("/a/y.wav")]);
    }

    #[test]
    fn load_reads_the_folder_and_saves_write_back() {
        let d = tmp_dir("load");
        let mut pls = Playlists::load(&d);
        assert!(pls.lists.is_empty());
        let a = d.join("a.wav");
        pls.get_or_create("Whooshes").add([a.clone()]);
        pls.get_or_create("impacts").add([a.clone()]);
        assert_eq!(pls.lists.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["impacts", "Whooshes"]);
        assert_eq!(pls.get("WHOOSHES").map(|l| l.paths.len()), Some(1));
        for l in &pls.lists {
            write_atomic(&l.file, &to_m3u(&l.paths)).unwrap();
        }
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        let back = Playlists::load(&d);
        assert_eq!(back.lists.len(), 2);
        assert_eq!(back.get("whooshes").unwrap().paths, [a]);
        assert_eq!(back.names_starting("WH"), ["Whooshes"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
