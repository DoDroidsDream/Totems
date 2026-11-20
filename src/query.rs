//! Search syntax and the parallel search engine.
//!
//! Syntax (familiar to anyone who has used a desktop file search tool):
//!   foo bar        AND
//!   foo | bar      OR (binds tighter than AND)
//!   !foo           NOT
//!   < ... >        grouping
//!   "a b"          literal phrase (spaces, | ! < > kept)
//!   *.wav  k?ck    wildcards match the whole name
//!   dir\name       a term with a path separator matches the full path
//!   ext:wav;flac   extension list
//!   size:>1mb      size compare: > >= < <= = and ranges 1mb..5mb, or
//!                  empty|tiny|small|medium|large|huge|gigantic
//!   path: case: nocase: nopath: regex:   per-term modifiers
//!   F:\a b\c.wav   a whole query starting with a drive (or \\server) is one
//!                  literal path, so a pasted path finds its file

use crate::index::{AUDIO_EXTS, Index};
use memchr::memmem;
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Options {
    pub match_case: bool,
    pub match_path: bool,
    pub regex: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Term(Term),
}

enum Term {
    Text { finder: memmem::Finder<'static>, case: bool, path: bool },
    Glob { pat: Vec<u8>, case: bool, path: bool },
    Regex { re: regex::Regex, path: bool },
    Ext([bool; 256]),
    Size(u64, u64), // inclusive range
}

// --------------------------------------------------------------- lexer

#[derive(Debug, PartialEq)]
enum Tok {
    Open,
    Close,
    Or,
    Not,
    Word(String),
}

fn lex(q: &str) -> Vec<Tok> {
    let c: Vec<char> = q.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0i32;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        match ch {
            '|' => {
                out.push(Tok::Or);
                i += 1;
                continue;
            }
            '!' => {
                out.push(Tok::Not);
                i += 1;
                continue;
            }
            '<' => {
                out.push(Tok::Open);
                depth += 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        let mut w = String::new();
        let mut quoted = false;
        let mut in_q = false;
        while i < c.len() {
            let ch = c[i];
            if ch == '"' {
                in_q = !in_q;
                quoted = true;
                i += 1;
                continue;
            }
            if !in_q {
                if ch.is_whitespace() || ch == '|' {
                    break;
                }
                if ch == '>' && depth > 0 {
                    let next = c.get(i + 1);
                    if next.is_none_or(|n| n.is_whitespace() || matches!(n, '|' | '>' | '<' | '!' | '"')) {
                        break;
                    }
                }
            }
            w.push(ch);
            i += 1;
        }
        if !w.is_empty() || quoted {
            out.push(Tok::Word(w));
        }
        while i < c.len() && c[i] == '>' && depth > 0 {
            out.push(Tok::Close);
            depth -= 1;
            i += 1;
        }
    }
    out
}

// -------------------------------------------------------------- parser

/// Deepest `< >` nesting accepted. Parsing, evaluation and dropping a
/// query all recurse per level, on the search thread's 2 MiB stack.
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    opts: &'a Options,
    err: Option<String>,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn and(&mut self) -> Option<Node> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Tok::Close => break,
                Tok::Or => {
                    self.pos += 1; // stray '|'
                }
                _ => {
                    if let Some(n) = self.or() {
                        items.push(n);
                    }
                }
            }
        }
        match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::And(items)),
        }
    }

    fn or(&mut self) -> Option<Node> {
        let mut items: Vec<Node> = self.unary().into_iter().collect();
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            if let Some(n) = self.unary() {
                items.push(n);
            }
        }
        match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::Or(items)),
        }
    }

    fn unary(&mut self) -> Option<Node> {
        let t = self.toks.get(self.pos)?;
        match t {
            Tok::Not => {
                // A run of `!` folds to one NOT or none, without recursing.
                let mut neg = false;
                while self.peek() == Some(&Tok::Not) {
                    neg = !neg;
                    self.pos += 1;
                }
                let n = self.unary();
                if neg { n.map(|n| Node::Not(Box::new(n))) } else { n }
            }
            Tok::Open => {
                self.pos += 1;
                if self.depth >= MAX_DEPTH {
                    self.err.get_or_insert_with(|| format!("Too many nested < > (most is {MAX_DEPTH})"));
                    self.pos = self.toks.len();
                    return None;
                }
                self.depth += 1;
                let n = self.and();
                self.depth -= 1;
                if self.peek() == Some(&Tok::Close) {
                    self.pos += 1;
                }
                n
            }
            Tok::Word(w) => {
                let w = w.clone();
                self.pos += 1;
                match term(&w, self.opts) {
                    Ok(t) => t.map(Node::Term),
                    Err(e) => {
                        self.err.get_or_insert(e);
                        None
                    }
                }
            }
            Tok::Or | Tok::Close => None,
        }
    }
}

/// A size in bytes; may be fractional (1.1kb is 1126.4 bytes).
fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(n) = s.strip_suffix("kb").or_else(|| s.strip_suffix('k')) {
        (n, 1u64 << 10)
    } else if let Some(n) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
        (n, 1 << 20)
    } else if let Some(n) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
        (n, 1 << 30)
    } else if let Some(n) = s.strip_suffix('b') {
        (n, 1)
    } else {
        (s.as_str(), 1)
    };
    let v: f64 = num.trim().parse().ok()?;
    let bytes = v * mul as f64;
    (bytes.is_finite() && bytes >= 0.0).then_some(bytes)
}

fn size_term(v: &str) -> Result<Term, String> {
    const K: u64 = 1024;
    let named = match v.to_ascii_lowercase().as_str() {
        "empty" => Some((0, 0)),
        "tiny" => Some((0, 10 * K)),
        "small" => Some((10 * K + 1, 100 * K)),
        "medium" => Some((100 * K + 1, K * K)),
        "large" => Some((K * K + 1, 16 * K * K)),
        "huge" => Some((16 * K * K + 1, 128 * K * K)),
        "gigantic" => Some((128 * K * K + 1, u64::MAX)),
        _ => None,
    };
    if let Some((a, b)) = named {
        return Ok(Term::Size(a, b));
    }
    let bad = || format!("Invalid size: {v}");
    // Whole bytes at or above / at or below a possibly fractional size.
    let ceil = |x: f64| if x >= u64::MAX as f64 { u64::MAX } else { x.ceil() as u64 };
    let floor = |x: f64| if x >= u64::MAX as f64 { u64::MAX } else { x.floor() as u64 };
    if let Some((a, b)) = v.split_once("..") {
        let (a, b) = (parse_size(a).ok_or_else(bad)?, parse_size(b).ok_or_else(bad)?);
        return Ok(Term::Size(ceil(a), floor(b)));
    }
    let (op, rest) = ["<=", ">=", "<", ">", "="]
        .iter()
        .find_map(|op| v.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("=", v));
    let n = parse_size(rest).ok_or_else(bad)?;
    // An empty range is (1, 0): nothing is >= 1 and <= 0.
    Ok(match op {
        "<=" => Term::Size(0, floor(n)),
        ">=" => Term::Size(ceil(n), u64::MAX),
        "<" if ceil(n) == 0 => Term::Size(1, 0),
        "<" => Term::Size(0, ceil(n) - 1),
        ">" => Term::Size(floor(n).saturating_add(1), u64::MAX),
        _ => Term::Size(ceil(n), floor(n)),
    })
}

fn text_term(text: &str, case: bool, path: bool, regex: bool) -> Result<Option<Term>, String> {
    if regex {
        let re = regex::RegexBuilder::new(text)
            .case_insensitive(!case)
            .size_limit(1 << 22)
            .build()
            .map_err(|e| e.to_string().lines().last().unwrap_or("Invalid regex").to_string())?;
        return Ok(Some(Term::Regex { re, path }));
    }
    if text.is_empty() {
        return Ok(None);
    }
    let path = path || text.contains('/') || text.contains('\\');
    let mut t = if case { text.to_string() } else { crate::index::lower(text) };
    if path {
        t = t.replace('\\', "/");
    }
    if t.contains('*') || t.contains('?') {
        if t.bytes().all(|b| b == b'*') {
            return Ok(None); // matches everything
        }
        return Ok(Some(Term::Glob { pat: t.into_bytes(), case, path }));
    }
    let finder = memmem::Finder::new(t.as_bytes()).into_owned();
    Ok(Some(Term::Text { finder, case, path }))
}

fn term(word: &str, opts: &Options) -> Result<Option<Term>, String> {
    let (mut case, mut path, mut regex) = (opts.match_case, opts.match_path, false);
    let mut w = word;
    loop {
        let Some((m, rest)) = w.split_once(':') else { break };
        match m.to_ascii_lowercase().as_str() {
            "case" => case = true,
            "nocase" => case = false,
            "path" => path = true,
            "nopath" => path = false,
            "regex" => regex = true,
            "ext" => {
                let mut set = [false; 256];
                for e in rest.split([';', ',']) {
                    let e = e.trim().trim_start_matches('.');
                    if let Some(i) = AUDIO_EXTS.iter().position(|x| x.eq_ignore_ascii_case(e)) {
                        set[i] = true;
                    }
                }
                return Ok(Some(Term::Ext(set)));
            }
            "size" => return size_term(rest).map(Some),
            _ => break,
        }
        w = rest;
    }
    text_term(w, case, path, regex)
}

/// A compiled query.
pub struct Query {
    root: Option<Node>,
    pub highlights: Vec<String>,
}

/// The query as one literal Windows path (`F:\x y\z.wav`, `\\server\x`,
/// optionally in quotes, as Explorer's "Copy as path" gives it), if it is
/// one. Paths have spaces and `!`, which would otherwise split the query
/// into name-only words that never match.
fn pasted_path(q: &str) -> Option<&str> {
    let q = q.trim();
    let q = q.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(q);
    let b = q.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    let unc = q.starts_with(r"\\") && q.len() > 2;
    // '"' can't appear in a Windows path: anything quoted inside is syntax.
    ((drive || unc) && !q.contains('"')).then_some(q)
}

impl Query {
    pub fn parse(q: &str, opts: &Options) -> Result<Query, String> {
        if !opts.regex
            && let Some(p) = pasted_path(q)
        {
            let t = text_term(p, opts.match_case, true, false)?;
            return Ok(Query { root: t.map(Node::Term), highlights: vec![] });
        }
        if opts.regex {
            let t = text_term(q, opts.match_case, opts.match_path, !q.is_empty())?;
            return Ok(Query { root: t.map(Node::Term), highlights: vec![] });
        }
        let mut p = Parser { toks: lex(q), pos: 0, opts, err: None, depth: 0 };
        let mut items = Vec::new();
        while p.pos < p.toks.len() {
            if let Some(n) = p.and() {
                items.push(n);
            }
            if p.peek() == Some(&Tok::Close) {
                p.pos += 1; // unbalanced '>'
            }
        }
        if let Some(e) = p.err {
            return Err(e);
        }
        let root = match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::And(items)),
        };
        let mut highlights = Vec::new();
        if let Some(r) = &root {
            collect_highlights(r, &mut highlights);
        }
        Ok(Query { root, highlights })
    }
}

fn collect_highlights(n: &Node, out: &mut Vec<String>) {
    match n {
        Node::And(v) | Node::Or(v) => v.iter().for_each(|n| collect_highlights(n, out)),
        Node::Not(_) => {}
        Node::Term(Term::Text { finder, path: false, .. }) => {
            if let Ok(s) = std::str::from_utf8(finder.needle()) {
                out.push(crate::index::lower(s));
            }
        }
        Node::Term(_) => {}
    }
}

// ----------------------------------------------------------- evaluation

#[derive(Default)]
struct Bufs {
    lower_for: usize,
    lower: Vec<u8>,
    case_for: usize,
    case: Vec<u8>,
}

impl Bufs {
    fn new() -> Self {
        Self { lower_for: usize::MAX, case_for: usize::MAX, ..Default::default() }
    }

    fn path<'a>(&'a mut self, ix: &Index, i: usize, case: bool) -> &'a [u8] {
        let (buf, slot) =
            if case { (&mut self.case, &mut self.case_for) } else { (&mut self.lower, &mut self.lower_for) };
        if *slot != i {
            *slot = i;
            buf.clear();
            let d = ix.dir[i] as usize;
            if case {
                buf.extend(ix.dirs[d].bytes().map(|b| if b == b'\\' { b'/' } else { b }));
            } else {
                buf.extend_from_slice(ix.dirs_lower[d].as_bytes());
            }
            if buf.last() != Some(&b'/') {
                buf.push(b'/');
            }
            if case {
                buf.extend_from_slice(ix.name(i).as_bytes());
            } else {
                buf.extend_from_slice(ix.name_lower(i));
            }
        }
        buf
    }
}

/// Length of the UTF-8 sequence starting with byte `b`.
fn char_len(b: u8) -> usize {
    match b {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

/// Wildcard match over UTF-8 bytes; `?` is one character, `*` any run.
fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'?' {
            pi += 1;
            si = (si + char_len(s[si])).min(s.len());
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark = (mark + char_len(s[mark])).min(s.len());
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn eval(n: &Node, ix: &Index, i: usize, b: &mut Bufs) -> bool {
    match n {
        Node::And(v) => v.iter().all(|n| eval(n, ix, i, b)),
        Node::Or(v) => v.iter().any(|n| eval(n, ix, i, b)),
        Node::Not(n) => !eval(n, ix, i, b),
        Node::Term(t) => match t {
            Term::Text { finder, case, path } => {
                let hay = if *path {
                    b.path(ix, i, *case)
                } else if *case {
                    ix.name(i).as_bytes()
                } else {
                    ix.name_lower(i)
                };
                finder.find(hay).is_some()
            }
            Term::Glob { pat, case, path } => {
                let hay = if *path {
                    b.path(ix, i, *case)
                } else if *case {
                    ix.name(i).as_bytes()
                } else {
                    ix.name_lower(i)
                };
                glob(pat, hay)
            }
            Term::Regex { re, path } => {
                if *path {
                    // The same '/'-separated path every other path term sees.
                    std::str::from_utf8(b.path(ix, i, true)).is_ok_and(|p| re.is_match(p))
                } else {
                    re.is_match(ix.name(i))
                }
            }
            Term::Ext(set) => set[ix.ext[i] as usize],
            Term::Size(a, z) => (*a..=*z).contains(&ix.size[i]),
        },
    }
}

/// Pick a required, case-insensitive name substring to pre-filter with a
/// single SIMD pass over the packed name buffer.
fn prefilter(root: &Node) -> Option<&memmem::Finder<'static>> {
    fn pick(n: &Node) -> Option<&memmem::Finder<'static>> {
        match n {
            Node::Term(Term::Text { finder, case: false, path: false }) => Some(finder),
            _ => None,
        }
    }
    match root {
        Node::And(v) => v.iter().filter_map(pick).max_by_key(|f| f.needle().len()),
        n => pick(n),
    }
}

const CHUNK: usize = 1 << 14;

pub fn search(ix: &Index, q: &Query, sort: SortKey, ascending: bool) -> Vec<u32> {
    let n = ix.len();
    let mut ids: Vec<u32> = match &q.root {
        None => (0..n as u32).collect(),
        Some(root) => {
            if let Some(f) = prefilter(root) {
                // Parallel scan of the packed lower-case name buffer.
                let chunks = n.div_ceil(CHUNK);
                let parts: Vec<Vec<u32>> = (0..chunks)
                    .into_par_iter()
                    .map(|c| {
                        let (s, e) = (c * CHUNK, ((c + 1) * CHUNK).min(n));
                        let base = ix.lower_off[s] as usize;
                        let hay = &ix.lower[base..ix.lower_off[e] as usize];
                        let mut out = Vec::new();
                        let mut cur = s;
                        let mut last = usize::MAX;
                        let mut b = Bufs::new();
                        for pos in f.find_iter(hay) {
                            let abs = (base + pos) as u32;
                            while ix.lower_off[cur + 1] <= abs {
                                cur += 1;
                            }
                            if cur == last {
                                continue;
                            }
                            last = cur;
                            if eval(root, ix, cur, &mut b) {
                                out.push(cur as u32);
                            }
                        }
                        out
                    })
                    .collect();
                parts.concat()
            } else {
                let chunks = n.div_ceil(CHUNK);
                let parts: Vec<Vec<u32>> = (0..chunks)
                    .into_par_iter()
                    .map(|c| {
                        let mut b = Bufs::new();
                        (c * CHUNK..((c + 1) * CHUNK).min(n))
                            .filter(|&i| eval(root, ix, i, &mut b))
                            .map(|i| i as u32)
                            .collect()
                    })
                    .collect();
                parts.concat()
            }
        }
    };
    let rank = match sort {
        SortKey::Name => None,
        SortKey::Path => Some(&ix.rank_path),
        SortKey::Size => Some(&ix.rank_size),
        SortKey::Modified => Some(&ix.rank_mtime),
    };
    if let Some(r) = rank {
        ids.par_sort_unstable_by_key(|&i| r[i as usize]);
    }
    if !ascending {
        ids.reverse();
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RawEntry;

    fn ix() -> Index {
        let dirs = vec!["/lib/Foley".to_string(), "/lib/Ambience".to_string(), "/music".to_string()];
        let e = |dir, name: &str, size| RawEntry { dir, name: name.into(), size, mtime: 0 };
        Index::build(
            dirs,
            vec![
                e(0, "Door Slam 01.wav", 2_000_000),
                e(0, "Door Creak.flac", 50_000),
                e(1, "Forest Birds.wav", 30_000_000),
                e(1, "City Rain.mp3", 5_000),
                e(2, "Rain Song.mp3", 4_000_000),
                e(0, "Footsteps Gravel.aiff", 900_000),
            ],
        )
    }

    fn names(q: &str) -> Vec<String> {
        names_opts(q, Options::default())
    }

    fn names_opts(q: &str, o: Options) -> Vec<String> {
        let ix = ix();
        let q = Query::parse(q, &o).unwrap();
        search(&ix, &q, SortKey::Name, true).iter().map(|&i| ix.name(i as usize).to_string()).collect()
    }

    #[test]
    fn pasted_path_is_one_literal_term() {
        assert_eq!(pasted_path(r"F:\Big Fish\Vol. 1\(03).wav"), Some(r"F:\Big Fish\Vol. 1\(03).wav"));
        assert_eq!(pasted_path(r#"  "F:\a b\!!x.wav"  "#), Some(r"F:\a b\!!x.wav"));
        assert_eq!(pasted_path("c:/x y"), Some("c:/x y"));
        assert_eq!(pasted_path(r"\\nas\samples\kick 1.wav"), Some(r"\\nas\samples\kick 1.wav"));
        for q in ["door slam", r"foley\door", "ext:wav", "f:", r#""F:\a" kick"#, "size:>1mb"] {
            assert_eq!(pasted_path(q), None, "{q}");
        }
    }

    /// Ctrl+C a row, paste it into search: that file is found. A pasted path
    /// used to split at its spaces into name-only words that matched nothing.
    #[cfg(windows)]
    #[test]
    fn pasting_a_copied_path_finds_the_file() {
        let dirs = vec![r"F:\Samples\Big Fish Audio e-Lab\Vol. 1".to_string(), r"F:\Other".to_string()];
        let e = |dir, name: &str| RawEntry { dir, name: name.into(), size: 1, mtime: 0 };
        let ix = Index::build(dirs, vec![e(0, "(03).wav"), e(0, "(04).wav"), e(1, "(03).wav"), e(0, "!!x y.wav")]);
        let hits = |q: &str| {
            let q = Query::parse(q, &Options::default()).unwrap();
            search(&ix, &q, SortKey::Name, true).iter().map(|&i| ix.full_path(i as usize)).collect::<Vec<_>>()
        };
        let p = std::path::PathBuf::from;
        assert_eq!(
            hits(r"F:\Samples\Big Fish Audio e-Lab\Vol. 1\(03).wav"),
            [p(r"F:\Samples\Big Fish Audio e-Lab\Vol. 1\(03).wav")]
        );
        assert_eq!(hits(r#""f:\samples\big fish audio e-lab\vol. 1\!!x y.wav""#).len(), 1, "quoted, any case, with !");
        assert_eq!(hits(r"F:\Samples\Big Fish Audio e-Lab\Vol. 1").len(), 3, "a folder lists its files");
    }

    #[test]
    fn syntax() {
        assert_eq!(names("").len(), 6);
        assert_eq!(names("door"), ["Door Creak.flac", "Door Slam 01.wav"]);
        assert_eq!(names("DOOR slam"), ["Door Slam 01.wav"]);
        assert_eq!(names("door !slam"), ["Door Creak.flac"]);
        assert_eq!(names("creak|birds"), ["Door Creak.flac", "Forest Birds.wav"]);
        assert_eq!(names("rain <city|song> !mp4"), ["City Rain.mp3", "Rain Song.mp3"]);
        assert_eq!(names("\"door s\""), ["Door Slam 01.wav"]);
        assert_eq!(names("*.wav"), ["Door Slam 01.wav", "Forest Birds.wav"]);
        assert_eq!(names("d??r*"), ["Door Creak.flac", "Door Slam 01.wav"]);
        assert_eq!(names("ext:mp3;aiff"), ["City Rain.mp3", "Footsteps Gravel.aiff", "Rain Song.mp3"]);
        assert_eq!(names("size:>1mb"), ["Door Slam 01.wav", "Forest Birds.wav", "Rain Song.mp3"]);
        assert_eq!(names("size:1kb..100kb"), ["City Rain.mp3", "Door Creak.flac"]);
        assert_eq!(names("size:huge"), ["Forest Birds.wav"]);
        assert_eq!(names("ambience\\"), ["City Rain.mp3", "Forest Birds.wav"]);
        assert_eq!(names("path:foley door"), ["Door Creak.flac", "Door Slam 01.wav"]);
        assert_eq!(names("/lib/*/f*"), ["Footsteps Gravel.aiff", "Forest Birds.wav"]);
        assert_eq!(names("case:door"), Vec::<String>::new());
        assert_eq!(names("regex:\"^(door|city)\""), ["City Rain.mp3", "Door Creak.flac", "Door Slam 01.wav"]);
        assert!(Query::parse("regex:(", &Options::default()).is_err());
        let re = Options { regex: true, ..Default::default() };
        assert_eq!(names_opts("^f.*\\.(wav|aiff)$", re), ["Footsteps Gravel.aiff", "Forest Birds.wav"]);
    }

    #[test]
    fn sorting() {
        let ix = ix();
        let q = Query::parse("", &Options::default()).unwrap();
        let by_size = search(&ix, &q, SortKey::Size, false);
        assert_eq!(ix.name(by_size[0] as usize), "Forest Birds.wav");
        let by_path = search(&ix, &q, SortKey::Path, true);
        assert_eq!(ix.dir_str(by_path[0] as usize), "/lib/Ambience");
    }

    fn ix_of(dir: &str, names: &[(&str, u64)]) -> Index {
        Index::build(
            vec![dir.to_string()],
            names.iter().map(|&(n, size)| RawEntry { dir: 0, name: n.into(), size, mtime: 0 }).collect(),
        )
    }

    fn hits(ix: &Index, q: &str) -> Vec<String> {
        let q = Query::parse(q, &Options::default()).unwrap();
        search(ix, &q, SortKey::Name, true).iter().map(|&i| ix.name(i as usize).to_string()).collect()
    }

    /// B24: 1 KB of nested `<` (or a few KB of `!`) overflowed the search
    /// thread's stack and aborted the app.
    #[test]
    fn deep_nesting_does_not_overflow() {
        for q in ["<".repeat(10_000) + "x", "!".repeat(10_000) + "x", "!<".repeat(5_000) + "x"] {
            let ok = std::thread::Builder::new()
                .stack_size(2 << 20) // the app's search thread
                .spawn(move || {
                    let ix = ix();
                    if let Ok(p) = Query::parse(&q, &Options::default()) {
                        search(&ix, &p, SortKey::Name, true);
                    }
                })
                .unwrap()
                .join()
                .is_ok();
            assert!(ok);
        }
        // An even number of `!` cancels out; an odd number negates.
        let ix = ix_of(r"C:\sfx", &[("door.wav", 1), ("rain.wav", 1)]);
        assert_eq!(hits(&ix, "!!door"), ["door.wav"]);
        assert_eq!(hits(&ix, "!!!door"), ["rain.wav"]);
    }

    /// B25: `>` didn't close a group when `<` or `!` followed it directly.
    #[test]
    fn group_close_before_open_or_not() {
        let ix = ix_of(r"C:\sfx", &[("door slam.wav", 1), ("door creak.wav", 1), ("city rain.wav", 1), ("rain song.wav", 1)]);
        assert_eq!(hits(&ix, "<door|city><slam|rain>"), ["city rain.wav", "door slam.wav"]);
        assert_eq!(hits(&ix, "<door|creak>!slam"), ["door creak.wav"]);
    }

    /// B26: glob `?` matched one byte, not one character.
    #[test]
    fn glob_question_mark_is_one_char() {
        let ix = ix_of(r"C:\sfx", &[("café.wav", 1), ("cafe.wav", 1), ("caféé.wav", 1)]);
        assert_eq!(hits(&ix, "caf?.wav"), ["cafe.wav", "café.wav"]);
        assert_eq!(hits(&ix, "caf*é.wav"), ["café.wav", "caféé.wav"]);
    }

    /// B27: lowercasing depends on context for Greek sigma (Σ -> ς at the
    /// end of a word, σ elsewhere), so `Σ` never matched `ΟΔΟΣ`.
    #[test]
    fn final_sigma_case_insensitive() {
        let ix = ix_of(r"C:\sfx", &[("ΟΔΟΣ 01.wav", 1)]);
        assert_eq!(hits(&ix, "Σ"), ["ΟΔΟΣ 01.wav"]);
        assert_eq!(hits(&ix, "οδος"), ["ΟΔΟΣ 01.wav"]);
        let ix = ix_of(r"C:\ΑΣ", &[("x.wav", 1)]);
        assert_eq!(hits(&ix, r"Σ\x"), ["x.wav"]);
        assert_eq!(ix.find(std::path::Path::new(r"C:\ΑΣ\X.WAV")), Some(0));
    }

    /// B28: `size:<0` matched empty files, fractions were truncated, and
    /// negative or NaN sizes were accepted.
    #[test]
    fn size_edges() {
        let ix = ix_of(r"C:\sfx", &[("empty.wav", 0), ("one.wav", 1)]);
        assert!(hits(&ix, "size:<0").is_empty());
        assert_eq!(hits(&ix, "size:<=0"), ["empty.wav"]);
        let ix = ix_of(r"C:\sfx", &[("a.wav", 1126), ("b.wav", 1127)]); // 1.1 KB = 1126.4 bytes
        assert_eq!(hits(&ix, "size:<1.1kb"), ["a.wav"]);
        assert_eq!(hits(&ix, "size:<=1.1kb"), ["a.wav"]);
        assert_eq!(hits(&ix, "size:>=1.1kb"), ["b.wav"]);
        assert_eq!(hits(&ix, "size:>1.1kb"), ["b.wav"]);
        assert!(hits(&ix, "size:1.1kb").is_empty());
        assert_eq!(hits(&ix, "size:1kb..1.1kb"), ["a.wav"]);
        for bad in ["size:-5", "size:nan", "size:inf", "size:<-1"] {
            assert!(Query::parse(bad, &Options::default()).is_err(), "{bad}");
        }
    }

    /// B29: regex path terms matched the native `C:\` path while every other
    /// path term uses `/`.
    #[test]
    fn regex_path_uses_forward_slash() {
        let ix = ix_of(r"C:\SFX\Foley", &[("Door.wav", 1)]);
        assert_eq!(hits(&ix, "path:regex:foley/door"), ["Door.wav"]);
        assert_eq!(hits(&ix, "path:door"), ["Door.wav"]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::index::RawEntry;

    /// cargo test --release bench_1m -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_1m() {
        let words = ["door", "slam", "whoosh", "impact", "metal", "wood", "rain", "wind", "forest", "city",
            "footstep", "gravel", "glass", "break", "swish", "ui", "click", "beep", "riser", "drone"];
        let exts = ["wav", "flac", "mp3", "aif", "ogg"];
        let mut x = 0x2545F4914F6CDD1Du64;
        let mut rnd = || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        let dirs: Vec<String> = (0..20000).map(|i| format!("/Libraries/Vendor{}/{}/{}", i % 50, words[i % 20], i)).collect();
        let n = 1_000_000;
        let t = std::time::Instant::now();
        let entries: Vec<RawEntry> = (0..n).map(|i| {
            let name = format!("{}_{}_{}_{:03}.{}", words[(rnd() % 20) as usize], words[(rnd() % 20) as usize],
                rnd() % 10000, i % 1000, exts[(rnd() % 5) as usize]);
            RawEntry { dir: (rnd() % 20000) as u32, name, size: rnd() % 50_000_000, mtime: (rnd() % 1_700_000_000) as i64 }
        }).collect();
        let ix = Index::build(dirs, entries);
        println!("build 1M: {:?}", t.elapsed());
        let cp = std::env::temp_dir().join("totems-bench-ix.bin");
        let t = std::time::Instant::now();
        ix.save(&cp, &[]).unwrap();
        println!("cache save: {:?} ({} MB)", t.elapsed(), std::fs::metadata(&cp).unwrap().len() >> 20);
        let t = std::time::Instant::now();
        let back = Index::load(&cp, &[]).unwrap();
        println!("cache load: {:?}", t.elapsed());
        assert_eq!(back.len(), ix.len());
        let _ = std::fs::remove_file(cp);
        for (q, sort) in [
            ("", SortKey::Name), ("", SortKey::Size), ("door", SortKey::Name), ("door slam", SortKey::Name),
            ("whoosh !metal ext:wav", SortKey::Modified), ("imp|swi", SortKey::Name), ("*_123.wav", SortKey::Name),
            ("vendor7\\ rain", SortKey::Path), ("size:>40mb", SortKey::Size), ("zzzz", SortKey::Name),
            ("regex:^door_s.*7\\.flac$", SortKey::Name),
        ] {
            let qq = Query::parse(q, &Options::default()).unwrap();
            search(&ix, &qq, sort, true); // warm
            let t = std::time::Instant::now();
            let reps = 5;
            let mut cnt = 0;
            for _ in 0..reps { cnt = search(&ix, &qq, sort, true).len(); }
            println!("{:<28} {:>8} hits  {:>8.2?}", format!("{q:?} {sort:?}"), cnt, t.elapsed() / reps);
        }
    }

}
