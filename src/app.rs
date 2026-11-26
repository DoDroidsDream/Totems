//! The window: one search box, one result list, a waveform strip and a
//! status bar.

use crate::decode::Info;
use crate::index::{Index, default_roots};
use crate::indexer::Indexer;
use crate::player::{MAX_DB, MIN_DB, Player, State};
use crate::playlist::{Playlist, Playlists};
use crate::query::{Options, Query, SortKey, mentions_playlist, search_in};
use crate::workers::{InfoCache, Waves};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Event, FontFamily, FontId, Key, Layout, Modifiers, RichText, Sense,
    Stroke, TextFormat, Vec2, text::LayoutJob,
};
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::*;
use std::time::Duration;

pub const APP_NAME: &str = "Totems";
const ROW_H: f32 = 19.0;
/// Path, Length, Size and Date are a touch smaller than Name (13).
const SUB_FONT: f32 = 12.0;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub opts: Options,
    pub sort: SortKey,
    pub ascending: bool,
    pub dark: bool,
    pub volume_db: f32,
    pub soft_clip: bool,
    /// Play a file as soon as it's selected (arrow keys, click, Tab).
    pub autoplay: bool,
    /// When a sound plays to its end, play the next row.
    pub autoplay_next: bool,
    /// Loop the sound (or the selection being auditioned).
    pub looping: bool,
    /// Crossfade loop joins (off: sample-exact).
    pub loop_xfade: bool,
    /// Bookmarked folders: Ctrl+number fills the search box with one.
    pub bookmarks: Vec<Bookmark>,
    /// Where the playlists live; None: `%APPDATA%\Totems\Playlists`.
    pub playlist_dir: Option<PathBuf>,
    /// The output device to play through; None: the system default, followed
    /// when it changes.
    pub output_device: Option<crate::player::OutDevice>,
    /// Dark theme: the colours from before 2026-11-26 (View › Classic Dark).
    pub classic_dark: bool,
}

/// A bookmarked folder and its key: 1–9, or 10 for Ctrl+0.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub path: PathBuf,
    pub key: Option<u8>,
}

impl Config {
    fn without_roots() -> Self {
        Self {
            roots: Vec::new(),
            opts: Options::default(),
            sort: SortKey::Name,
            ascending: true,
            dark: false,
            volume_db: 0.0,
            soft_clip: true,
            autoplay: true,
            autoplay_next: false,
            looping: false,
            loop_xfade: false,
            bookmarks: Vec::new(),
            playlist_dir: None,
            output_device: None,
            classic_dark: false,
        }
    }
}

/// Settings as saved (RON). Read one field at a time when the whole fails:
/// a value this version can't read (another version's sort key, a wrong
/// type) falls back to its default alone, instead of losing the whole
/// config, folders included, which the next save made permanent (B41).
fn load_config(text: &str) -> Config {
    let mut c = ron::from_str::<Config>(text).unwrap_or_else(|_| {
        let fields = ron_fields(text);
        fn field<T: serde::de::DeserializeOwned>(fields: &[(&str, &str)], key: &str) -> Option<T> {
            fields.iter().find(|(k, _)| *k == key).and_then(|(_, v)| ron::from_str(v).ok())
        }
        let base = Config::without_roots();
        Config {
            roots: field(&fields, "roots").unwrap_or_else(default_roots),
            opts: field(&fields, "opts").unwrap_or(base.opts),
            sort: field(&fields, "sort").unwrap_or(base.sort),
            ascending: field(&fields, "ascending").unwrap_or(base.ascending),
            dark: field(&fields, "dark").unwrap_or(base.dark),
            volume_db: field(&fields, "volume_db").unwrap_or(base.volume_db),
            soft_clip: field(&fields, "soft_clip").unwrap_or(base.soft_clip),
            autoplay: field(&fields, "autoplay").unwrap_or(base.autoplay),
            autoplay_next: field(&fields, "autoplay_next").unwrap_or(base.autoplay_next),
            looping: field(&fields, "looping").unwrap_or(base.looping),
            loop_xfade: field(&fields, "loop_xfade").unwrap_or(base.loop_xfade),
            bookmarks: field(&fields, "bookmarks").unwrap_or_default(),
            playlist_dir: field(&fields, "playlist_dir").unwrap_or_default(),
            output_device: field(&fields, "output_device").unwrap_or_default(),
            classic_dark: field(&fields, "classic_dark").unwrap_or_default(),
        }
    });
    // B42: a NaN volume stuck in the playback glide; a huge one clipped.
    c.volume_db = if c.volume_db.is_finite() { c.volume_db.clamp(MIN_DB, MAX_DB) } else { 0.0 };
    c
}

/// Add a folder to the list, cleaned (no trailing separator), unless it's
/// already there under any spelling. Returns whether it was added.
fn add_root(roots: &mut Vec<PathBuf>, dir: &std::path::Path) -> bool {
    let dir = crate::indexer::clean_root(dir);
    let key = |p: &std::path::Path| p.to_str().map(crate::index::norm_lower).unwrap_or_default();
    if roots.iter().any(|r| key(&crate::indexer::clean_root(r)) == key(&dir)) {
        return false;
    }
    roots.push(dir);
    true
}

/// Bookmark a folder (cleaned, once under any spelling), on the lowest free
/// key 1–10; none left: no key. Returns whether it was added.
fn add_bookmark(marks: &mut Vec<Bookmark>, dir: &std::path::Path) -> bool {
    let dir = crate::indexer::clean_root(dir);
    let key = |p: &std::path::Path| p.to_str().map(crate::index::norm_lower).unwrap_or_default();
    if marks.iter().any(|m| key(&crate::indexer::clean_root(&m.path)) == key(&dir)) {
        return false;
    }
    let free = (1..=10).find(|k| !marks.iter().any(|m| m.key == Some(*k)));
    marks.push(Bookmark { path: dir, key: free });
    true
}

/// Give bookmark `i` key `k` (or none); whichever bookmark had it loses it.
fn assign_key(marks: &mut [Bookmark], i: usize, k: Option<u8>) {
    if k.is_some() {
        for m in marks.iter_mut() {
            if m.key == k {
                m.key = None;
            }
        }
    }
    if let Some(m) = marks.get_mut(i) {
        m.key = k;
    }
}

/// The key's label: 1–9, and 0 for 10.
fn key_label(k: u8) -> String {
    (k % 10).to_string()
}

/// Bookmark keys: Ctrl+1 … Ctrl+9 are 1–9, Ctrl+0 is 10.
const BOOKMARK_KEYS: [(u8, Key); 10] = [
    (1, Key::Num1),
    (2, Key::Num2),
    (3, Key::Num3),
    (4, Key::Num4),
    (5, Key::Num5),
    (6, Key::Num6),
    (7, Key::Num7),
    (8, Key::Num8),
    (9, Key::Num9),
    (10, Key::Num0),
];

/// What a bookmark puts in the search box: the folder, quoted, with a
/// trailing separator (so `Foley` doesn't also match `Foley 2`) and a
/// space to type more words after. Alone it's a pasted path, one literal
/// term; with words after it, a quoted path term ANDed with them.
fn bookmark_query(dir: &std::path::Path) -> String {
    let d = dir.to_string_lossy();
    let sep = if d.ends_with(['\\', '/']) { "" } else { std::path::MAIN_SEPARATOR_STR };
    format!("\"{d}{sep}\" ")
}

/// Put the search box's caret at the end of `text`, with focus.
fn caret_to_end(ctx: &egui::Context, id: egui::Id, text: &str) {
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        let n = text.chars().count();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(n))));
        st.store(ctx, id);
    }
    ctx.memory_mut(|m| m.request_focus(id));
}

/// The top-level `key:value` pairs of a RON struct `( … )`, split at commas
/// outside brackets and strings.
fn ron_fields(text: &str) -> Vec<(&str, &str)> {
    let t = text.trim();
    let t = t.strip_prefix('(').and_then(|t| t.strip_suffix(')')).unwrap_or(t);
    let (mut out, mut depth, mut in_str, mut esc, mut start) = (Vec::new(), 0i32, false, false, 0);
    fn push<'a>(part: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
        if let Some((k, v)) = part.split_once(':') {
            out.push((k.trim(), v.trim()));
        }
    }
    for (i, ch) in t.char_indices() {
        if in_str {
            match ch {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                push(&t[start..i], &mut out);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(&t[start..], &mut out);
    out
}

impl Default for Config {
    fn default() -> Self {
        Self { roots: default_roots(), ..Self::without_roots() }
    }
}

/// A transient status-bar message: errors stay 5 s in red, info 2 s in grey.
struct Notice {
    text: String,
    at: std::time::Instant,
    error: bool,
}

impl Notice {
    fn error(text: String) -> Self {
        Self { text, at: std::time::Instant::now(), error: true }
    }
    fn info(text: &str) -> Self {
        Self { text: text.into(), at: std::time::Instant::now(), error: false }
    }
    /// Seconds left on screen, if any.
    fn remaining(&self) -> Option<f32> {
        let life = if self.error { 5.0 } else { 2.0 };
        let left = life - self.at.elapsed().as_secs_f32();
        (left > 0.0).then_some(left)
    }
}

/// Scroll to the selection only for results the user asked for (query,
/// options or sort changed), never for a background index refresh, so a file
/// changing on disk doesn't yank the list away from where the user is reading.
#[derive(Default)]
struct ScrollIntent {
    want: Option<u64>,
}

impl ScrollIntent {
    fn sent(&mut self, generation: u64, user_change: bool) {
        if user_change {
            self.want = Some(generation);
        }
    }
    /// Whether the result with this generation should scroll to the selection.
    /// Latest-wins coalescing can merge a user request into a later refresh,
    /// so any result at or after the wanted generation counts.
    fn arrived(&mut self, generation: u64) -> bool {
        if self.want.is_some_and(|g| generation >= g) {
            self.want = None;
            true
        } else {
            false
        }
    }
}

/// Remembers whether a row's single click started playback, so the second
/// half of a double-click doesn't restart the sound it just started.
#[derive(Default)]
struct ClickPlay {
    started: Option<usize>,
}

impl ClickPlay {
    fn single(&mut self, row: usize, started: bool) {
        self.started = started.then_some(row);
    }
    /// Whether a double-click on `row` should (re)start playback.
    fn double(&mut self, row: usize) -> bool {
        self.started.take() != Some(row)
    }
}

/// ←/→ seek step in seconds (Shift: 5 s). Shift is consumed first because
/// egui's plain-key match also accepts Shift held down.
fn consume_seek(ctx: &egui::Context) -> Option<f64> {
    let mut step = None;
    for (m, k, d) in [
        (Modifiers::SHIFT, Key::ArrowLeft, -5.0),
        (Modifiers::SHIFT, Key::ArrowRight, 5.0),
        (Modifiers::NONE, Key::ArrowLeft, -1.0),
        (Modifiers::NONE, Key::ArrowRight, 1.0),
    ] {
        if ctx.input_mut(|i| i.consume_key(m, k)) {
            step = Some(step.unwrap_or(0.0) + d);
        }
    }
    step
}

/// Ctrl+↓: the first row after `row` whose key (the lowercase name) differs,
/// so a run of 50 files called "Bb" is skipped in one press. The last row if
/// every later row has the same key.
fn next_different<K: PartialEq>(len: usize, row: usize, key: impl Fn(usize) -> K) -> usize {
    let cur = key(row);
    (row + 1..len).find(|&r| key(r) != cur).unwrap_or(len.saturating_sub(1))
}

/// The list's top row this frame. `offset` is last frame's scroll offset
/// (floored, so a part-row at the top is dropped), `wheel` the rows the wheel
/// asked for, and `show` a row that must end up fully visible (keyboard
/// navigation, or the selection after a new search). `total_rows` includes
/// the blank spacer row.
fn list_top_row(offset: f32, view_h: f32, total_rows: usize, wheel: i32, show: Option<usize>) -> usize {
    let (view_rows, max_top) = list_geom(view_h, total_rows);
    let mut top = offset_rows(offset).floor() as usize;
    top = (top as i64 + wheel as i64).clamp(0, max_top as i64) as usize;
    if let Some(r) = show {
        if r < top {
            top = r;
        } else if r >= top + view_rows {
            top = r + 1 - view_rows;
        }
    }
    top.min(max_top)
}

/// A scroll offset in rows. In f64, and rounded to the nearest row when
/// within a quarter row: the scroll area stores offsets as f32, which past
/// 16.7M points (~880k rows) can't hold `top * ROW_H` exactly.
fn offset_rows(offset: f32) -> f64 {
    let x = offset.max(0.0) as f64 / ROW_H as f64;
    if (x - x.round()).abs() <= 0.25 { x.round() } else { x }
}

/// Whether a scroll offset sits on a row boundary.
fn on_row_grid(offset: f32) -> bool {
    offset_rows(offset).fract() == 0.0
}

/// Whole rows that fit in `view_h`, and the largest top row that still
/// shows the last row in full (`total_rows` includes the spacer row).
fn list_geom(view_h: f32, total_rows: usize) -> (usize, usize) {
    let view_rows = ((view_h / ROW_H).floor() as usize).max(1);
    // Floored, never rounded up: a top past the scroll area's real maximum
    // gets clamped off the row grid and the list would jitter at the end.
    let max_top = ((total_rows as f64 * ROW_H as f64 - view_h as f64).max(0.0) / ROW_H as f64 + 1e-6).floor() as usize;
    (view_rows, max_top)
}

/// The list's own scroll bar: always visible, thin, and moving in whole rows.
/// Handle top y and length for a track starting at `track_top`.
fn bar_handle(track_top: f32, track_h: f32, top: usize, max_top: usize, view_rows: usize, total_rows: usize) -> (f32, f32) {
    let len = (track_h * view_rows as f32 / total_rows.max(1) as f32).clamp(SCROLL_HANDLE_MIN.min(track_h), track_h);
    let y = if max_top == 0 { 0.0 } else { (track_h - len) * top.min(max_top) as f32 / max_top as f32 };
    (track_top + y, len)
}

/// The top row for a handle whose top edge is dragged to `handle_y`.
fn bar_top_at(handle_y: f32, track_top: f32, track_h: f32, len: f32, max_top: usize) -> usize {
    let room = (track_h - len).max(1.0);
    let frac = ((handle_y - track_top) / room).clamp(0.0, 1.0);
    (frac * max_top as f32).round() as usize
}

/// Space either side of the waveform, so its very start is easy to click: a
/// click in the left gap lands on 0:00, in the right one on the end.
const WAVE_PAD: f32 = 12.0;
/// Waveform strip height: default, and the range its top edge can be dragged.
const WAVE_H: f32 = 56.0;
const WAVE_H_MIN: f32 = 32.0;
const WAVE_H_MAX: f32 = 400.0;

/// Whether the app's shortcuts may use the keyboard this frame: not while
/// another text field (the Folders path box) has focus, or Enter, Space,
/// arrows and Home/End would be taken from it.
fn keyboard_is_ours(ctx: &egui::Context, search_id: egui::Id) -> bool {
    ctx.memory(|m| m.focused()).is_none_or(|f| f == search_id)
}

/// Whether a wheel event at `pos` belongs to the result list: inside it and
/// not over a window or menu floating above it (Folders, Help).
fn wheel_over_list(ctx: &egui::Context, list: egui::Rect, pos: egui::Pos2) -> bool {
    list.contains(pos) && ctx.layer_id_at(pos).is_none_or(|l| l.order == egui::Order::Background)
}

/// Points of trackpad scrolling per volume step (egui's native line height).
const WHEEL_POINTS_PER_STEP: f32 = 40.0;

/// The volume label's slot; fits "+24.0 dB" and its box.
const VOL_LABEL_W: f32 = 60.0;
/// The volume label while boosting above 0 dB.
const BOOST: Color32 = Color32::from_rgb(0xD8, 0x7A, 0x00);
/// [Liam] The volume fader's cap, in points (a little bigger than egui's
/// round handle), and the handle shape egui travels it with (half a cap
/// wide), so egui's end positions match the cap we paint.
const FADER_CAP: Vec2 = Vec2::new(11.0, 16.0);
const FADER_ASPECT: f32 = 0.75;

/// Fader law, (position 0–1, dB): more travel round 0 dB, where small
/// changes matter; the bottom fifth covers -60 to -30.
const FADER: [(f32, f32); 5] = [(0.0, MIN_DB), (0.2, -30.0), (0.45, -12.0), (0.7, 0.0), (1.0, MAX_DB)];

/// Volume in dB at fader position `t`.
fn fader_db(t: f32) -> f32 {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    FADER
        .windows(2)
        .find(|w| t <= w[1].0)
        .map_or(MAX_DB, |w| w[0].1 + (w[1].1 - w[0].1) * (t - w[0].0) / (w[1].0 - w[0].0))
}

/// Fader position (0–1) of `db`; the inverse of `fader_db`.
fn fader_pos(db: f32) -> f32 {
    let db = if db.is_nan() { 0.0 } else { db.clamp(MIN_DB, MAX_DB) };
    FADER
        .windows(2)
        .find(|w| db <= w[1].1)
        .map_or(1.0, |w| w[0].0 + (w[1].0 - w[0].0) * (db - w[0].1) / (w[1].1 - w[0].1))
}

/// Whole steps from raw mouse-wheel events: a notch (one line) is
/// `per_line` steps, and `points_per_step` trackpad points make one step.
/// egui's smoothed delta spreads a notch over several frames, which turned
/// one notch into several steps. Trackpad fractions carry over in `acc`.
/// Positive = wheel up.
fn wheel_steps<'a>(events: impl IntoIterator<Item = &'a Event>, acc: &mut f32, per_line: f32, points_per_step: f32) -> i32 {
    for e in events {
        if let Event::MouseWheel { unit, delta, .. } = e {
            *acc += match unit {
                egui::MouseWheelUnit::Line => delta.y * per_line,
                egui::MouseWheelUnit::Point => delta.y / points_per_step,
                egui::MouseWheelUnit::Page => delta.y * per_line * 10.0,
            };
        }
    }
    let steps = acc.trunc();
    *acc -= steps;
    steps as i32
}

struct SearchReq {
    generation: u64,
    query: String,
    opts: Options,
    sort: SortKey,
    ascending: bool,
    index: Arc<Index>,
    playlists: Arc<Playlists>,
}

struct SearchRes {
    generation: u64,
    ids: Arc<Vec<u32>>,
    index: Arc<Index>,
    error: Option<String>,
    highlights: Vec<String>,
    /// Playlist files not in the index; the playlists every row is from.
    missing: usize,
    lists: Vec<String>,
}

fn spawn_searcher(
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> (crossbeam_channel::Sender<SearchReq>, crossbeam_channel::Receiver<SearchRes>) {
    let (qtx, qrx) = crossbeam_channel::unbounded::<SearchReq>();
    let (rtx, rrx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("search".into())
        .spawn(move || {
            while let Ok(mut req) = qrx.recv() {
                while let Ok(r) = qrx.try_recv() {
                    req = r; // only the newest query matters
                }
                let res = match Query::parse(&req.query, &req.opts) {
                    Ok(q) => {
                        let found = search_in(&req.index, &q, req.sort, req.ascending, &req.playlists);
                        SearchRes {
                            generation: req.generation,
                            ids: Arc::new(found.ids),
                            index: req.index,
                            error: None,
                            highlights: q.highlights,
                            missing: found.missing,
                            lists: found.lists,
                        }
                    }
                    Err(e) => SearchRes {
                        generation: req.generation,
                        ids: Arc::new(Vec::new()),
                        index: req.index,
                        error: Some(e),
                        highlights: vec![],
                        missing: 0,
                        lists: vec![],
                    },
                };
                if rtx.send(res).is_err() {
                    return;
                }
                repaint();
            }
        })
        .expect("spawn search");
    (qtx, rrx)
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    /// The result list's background. [Liam] Dark: #2B2B2B, lighter than
    /// the rest of the window.
    list_bg: Color32,
    /// The search box's fill. [Liam] Dark: #2B2B2B, like the list.
    search_bg: Color32,
    text: Color32,
    dim: Color32,
    header_text: Color32,
    grid: Color32,
    hover: Color32,
    sel: Color32,
    sel_inactive: Color32,
    sel_border: Color32,
    status_bg: Color32,
    /// [Liam] The frame round the content: the search panel behind the box,
    /// the Name gutter, the waveform's pads and the status bar (with
    /// `scroll_track`). Dark: #1B1B1B, darker than everything it frames.
    /// `status_bg` is left for the entry boxes in dialogs.
    frame_bg: Color32,
    /// The one blue: the waveform not yet played, the list's ▶ marker,
    /// search matches (without a bold face) and the waveform selection.
    accent: Color32,
    /// The waveform already played (grey).
    wave_played: Color32,
    /// With a waveform selection: everything outside it (greyed out).
    wave_out: Color32,
    /// [Liam] The waveform inside a selection: a lighter blue than `accent`,
    /// and it stays that colour as it plays.
    wave_sel: Color32,
    /// Selection edge crowns: fill and outline (the outline keeps the white
    /// crown visible on the light theme's white background).
    crown: Color32,
    crown_edge: Color32,
    playhead: Color32,
    playhead_crown: Color32,
    /// The gap left of the waveform: darker than the background so it reads
    /// as padding.
    /// Pencil-thin outline around each column heading.
    header_border: Color32,
    /// The list's scroll bar: track, handle, handle under the pointer. The
    /// handle's outline is `header_text` blue.
    scroll_track: Color32,
    scroll_handle: Color32,
    scroll_handle_hover: Color32,
    /// [Liam] The handle's outline while it sits at either end of its
    /// travel (top or bottom of the list): a subtle red, in place of blue.
    scroll_end: Color32,
    /// Volume slider groove (light theme only; dark keeps egui's).
    groove: Color32,
}

impl Palette {
    /// Colour of the waveform column centred at `x`. `play_x` is the
    /// playhead (None when not playing); `sel` the selection's x range.
    /// Not yet played is blue, played is grey. With a selection, the inside
    /// is a lighter blue that never greys as it plays, and everything
    /// outside is greyed out.
    fn wave_color(&self, x: f32, play_x: Option<f32>, sel: Option<(f32, f32)>) -> Color32 {
        let played = play_x.is_some_and(|p| x <= p);
        match sel {
            Some((a, b)) if x < a || x > b => self.wave_out,
            Some(_) => self.wave_sel,
            _ if played => self.wave_played,
            _ => self.accent,
        }
    }
}

const LIGHT: Palette = Palette {
    bg: Color32::WHITE,
    list_bg: Color32::WHITE,
    search_bg: Color32::WHITE,
    text: Color32::from_rgb(0x1E, 0x1E, 0x1E),
    dim: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    header_text: Color32::from_rgb(0x00, 0x5F, 0xB8), // 6.3:1 on white
    grid: Color32::from_rgb(0xE5, 0xE5, 0xE5),
    hover: Color32::from_rgb(0xE5, 0xF3, 0xFF),
    sel: Color32::from_rgb(0xCC, 0xE8, 0xFF),
    sel_inactive: Color32::from_rgb(0xD9, 0xD9, 0xD9),
    sel_border: Color32::from_rgb(0x99, 0xD1, 0xFF),
    status_bg: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    frame_bg: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    accent: Color32::from_rgb(0x00, 0x78, 0xD7),
    wave_played: Color32::from_rgb(0xA8, 0xB4, 0xC2),
    wave_out: Color32::from_rgb(0xD6, 0xDA, 0xDF),
    wave_sel: Color32::from_rgb(0x3D, 0x9B, 0xE9),
    crown: Color32::WHITE,
    crown_edge: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    // White would vanish on the light theme's white strip.
    playhead: Color32::from_rgb(0x1E, 0x1E, 0x1E),
    playhead_crown: Color32::from_rgb(0xF2, 0x7A, 0x00),
    header_border: Color32::from_rgb(0xA0, 0xA0, 0xA0),
    scroll_track: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    scroll_handle: Color32::from_rgb(0xC2, 0xC2, 0xC2),
    scroll_handle_hover: Color32::from_rgb(0xA8, 0xA8, 0xA8),
    scroll_end: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    groove: Color32::from_rgb(0xC2, 0xC2, 0xC2),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1F, 0x1F, 0x1F),
    list_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    search_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    text: Color32::from_rgb(0xE6, 0xE6, 0xE6),
    dim: Color32::from_rgb(0x9A, 0x9A, 0x9A),
    header_text: Color32::from_rgb(0x4C, 0xA6, 0xF2), // 6.3:1 on #1F1F1F
    grid: Color32::from_rgb(0x33, 0x33, 0x33),
    hover: Color32::from_rgb(0x2A, 0x33, 0x3D),
    sel: Color32::from_rgb(0x1F, 0x4E, 0x79),
    sel_inactive: Color32::from_rgb(0x3A, 0x3A, 0x3A),
    sel_border: Color32::from_rgb(0x2F, 0x6E, 0xA8),
    status_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    frame_bg: Color32::from_rgb(0x1B, 0x1B, 0x1B),
    accent: Color32::from_rgb(0x3A, 0x9B, 0xEF),
    wave_played: Color32::from_rgb(0x55, 0x60, 0x6C),
    wave_out: Color32::from_rgb(0x3A, 0x3E, 0x43),
    wave_sel: Color32::from_rgb(0x7C, 0xC0, 0xFF),
    crown: Color32::WHITE,
    crown_edge: Color32::TRANSPARENT,
    playhead: Color32::WHITE,
    playhead_crown: Color32::from_rgb(0xFF, 0x8C, 0x1A),
    header_border: Color32::BLACK,
    scroll_track: Color32::from_rgb(0x1B, 0x1B, 0x1B),
    scroll_handle: Color32::from_rgb(0x9E, 0x9E, 0x9E),
    scroll_handle_hover: Color32::from_rgb(0xBE, 0xBE, 0xBE),
    scroll_end: Color32::from_rgb(0xC2, 0x4A, 0x3E),
    groove: Color32::TRANSPARENT,
};

/// View › Classic Dark: the dark theme as it was before 2026-11-26. One
/// grey (#1F1F1F) for the list, search box and waveform, in a lighter
/// (#2B2B2B) frame.
const DARK_CLASSIC: Palette = Palette {
    list_bg: DARK.bg,
    search_bg: DARK.bg,
    frame_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    scroll_track: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    ..DARK
};

pub struct App {
    cfg: Config,
    indexer: Arc<Indexer>,
    seen_version: u64,
    query: String,
    search_tx: crossbeam_channel::Sender<SearchReq>,
    search_rx: crossbeam_channel::Receiver<SearchRes>,
    sent: Option<(String, Options, SortKey, bool, u64, u64)>,
    sent_gen: u64,
    got_gen: u64,
    results: Arc<Vec<u32>>,
    res_index: Option<Arc<Index>>,
    res_error: Option<String>,
    highlights: Vec<String>,
    /// Selected entry: its row, and its path (stable across index rebuilds).
    sel_row: Option<usize>,
    sel_path: Option<PathBuf>,
    scroll_to: Option<usize>,
    list_focus: bool,
    page_rows: usize,
    player: Player,
    infos: Arc<InfoCache>,
    waves: Arc<Waves>,
    show_folders: bool,
    folder_input: String,
    show_bookmarks: bool,
    bookmark_input: String,
    show_help: bool,
    /// Where the Search Syntax pop-out hangs from (under the Help menu),
    /// and the frame it opened on (that frame's click must not close it).
    help_anchor: egui::Pos2,
    help_opened: u64,
    has_bold: bool,
    last_title: String,
    styled: Option<bool>,
    /// File to hand to the OS drag-and-drop at the end of this frame.
    drag_out: Option<DragOut>,
    /// Waveform selection (file, start s, end s) and the anchor of an
    /// in-progress selection drag.
    wsel: Option<(PathBuf, f64, f64)>,
    wave_drag: Option<f64>,
    /// A Ctrl+left drag on the waveform is scrubbing.
    wave_scrub: bool,
    /// Transient message for the status bar.
    notice: Option<Notice>,
    /// The OS drag loop swallows the mouse-up; replay it to egui.
    release_pointer: bool,
    scroll_intent: ScrollIntent,
    click_play: ClickPlay,
    /// Fractional mouse-wheel steps over the volume slider.
    wheel_acc: f32,
    /// The result list's body as of last frame: screen rect, scroll offset
    /// and the wheel rows taken from it (see `raw_input_hook`).
    list_rect: Option<egui::Rect>,
    list_offset: f32,
    list_wheel_acc: f32,
    list_wheel_rows: i32,
    /// Heading cells of this frame, for their outlines.
    header_rects: Vec<egui::Rect>,
    /// Where the list's scroll handle was grabbed (y within the handle).
    bar_grab: Option<f32>,
    /// Search caret: what it was last frame (cursor ends, text length) and
    /// when that last changed, to restart the blink.
    caret_key: Option<(usize, usize, usize)>,
    caret_at: f64,
    /// Player text last shown in the status bar; held while a file loads.
    status_shown: Option<(Option<Transport>, String, Color32)>,
    /// Autoplay Next: the Load already moved on from, and how many files in
    /// a row failed to play.
    advanced_gen: u64,
    next_fails: u32,
    /// Playlists, as edited (saved in the background by `pl_io`).
    pls: Arc<Playlists>,
    pl_io: crate::playlist::Io,
    /// Bumped by every change to `pls` (searches re-run).
    pl_gen: u64,
    /// Bumped by every edit: a reload asked for before an edit is stale.
    pl_edits: u64,
    /// From the last search: playlist files not found, and the playlists
    /// its rows come from (what Delete and Alt+↑↓ edit).
    res_missing: usize,
    res_lists: Vec<String>,
    /// The query named a playlist last frame (switching to playlist order).
    was_pl: bool,
    /// Removed playlist rows, newest last, for Ctrl+Z.
    pl_undo: Vec<Vec<Removed>>,
    pl_menu: Option<PlMenu>,
    show_playlists: bool,
    /// Playback › Output: the devices listed when the submenu last opened,
    /// and the frame it was last drawn in.
    out_devices: Vec<crate::player::OutDevice>,
    out_devices_frame: u64,
    playlist_input: String,
    /// The selected row's Name cell on screen (where Ctrl+P's menu opens).
    sel_rect: Option<egui::Rect>,
    window_focused: bool,
}

/// A row taken out of a playlist: where it was, to put it back.
struct Removed {
    list: String,
    at: usize,
    path: PathBuf,
}

/// Ctrl+P's menu: the playlists, then "New…". `hot` is the highlighted
/// item (↑ ↓, Enter); `new_name` is the name being typed after "New…".
struct PlMenu {
    all: bool,
    hot: usize,
    new_name: Option<String>,
    at: egui::Pos2,
    opened: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let cfg = cc.storage.and_then(|s| s.get_string("config")).map(|t| load_config(&t)).unwrap_or_default();
        let has_bold = crate::fonts::install(&cc.egui_ctx);
        // Clear selection exports left by the last run (or a crash).
        std::thread::spawn(|| crate::region::cleanup(crate::region::GRACE));
        let indexer = Indexer::start(cfg.roots.clone(), repaint.clone());
        let (search_tx, search_rx) = spawn_searcher(repaint.clone());
        let player = Player::new(repaint.clone(), cfg.output_device.as_ref().map(|d| d.id.clone()));
        player.set_volume_db(cfg.volume_db);
        player.set_soft_clip(cfg.soft_clip);
        player.set_loop(cfg.looping);
        player.set_loop_crossfade(cfg.loop_xfade);
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 4);
        let pl_dir = cfg.playlist_dir.clone().unwrap_or_else(crate::playlist::default_dir);
        let pl_io = crate::playlist::Io::start(repaint.clone());
        pl_io.load(pl_dir.clone(), 0);
        Self {
            indexer,
            seen_version: 0,
            query: String::new(),
            search_tx,
            search_rx,
            sent: None,
            sent_gen: 0,
            got_gen: 0,
            results: Arc::new(Vec::new()),
            res_index: None,
            res_error: None,
            highlights: Vec::new(),
            sel_row: None,
            sel_path: None,
            scroll_to: None,
            list_focus: false,
            page_rows: 20,
            player,
            infos: InfoCache::new(threads, repaint.clone()),
            waves: Waves::new(repaint),
            show_folders: false,
            folder_input: String::new(),
            show_bookmarks: false,
            bookmark_input: String::new(),
            show_help: false,
            help_anchor: egui::Pos2::ZERO,
            help_opened: 0,
            has_bold,
            last_title: String::new(),
            styled: None,
            drag_out: None,
            wsel: None,
            wave_drag: None,
            wave_scrub: false,
            notice: None,
            release_pointer: false,
            scroll_intent: ScrollIntent::default(),
            click_play: ClickPlay::default(),
            wheel_acc: 0.0,
            list_rect: None,
            list_offset: 0.0,
            list_wheel_acc: 0.0,
            list_wheel_rows: 0,
            header_rects: Vec::new(),
            bar_grab: None,
            caret_key: None,
            caret_at: 0.0,
            status_shown: None,
            advanced_gen: 0,
            next_fails: 0,
            pls: Arc::new(Playlists { dir: pl_dir, lists: Vec::new() }),
            pl_io,
            pl_gen: 0,
            pl_edits: 0,
            res_missing: 0,
            res_lists: Vec::new(),
            was_pl: false,
            pl_undo: Vec::new(),
            pl_menu: None,
            show_playlists: false,
            out_devices: Vec::new(),
            out_devices_frame: 0,
            playlist_input: String::new(),
            sel_rect: None,
            window_focused: true,
            cfg,
        }
    }

    fn pal(&self) -> Palette {
        match (self.cfg.dark, self.cfg.classic_dark) {
            (false, _) => LIGHT,
            (true, false) => DARK,
            (true, true) => DARK_CLASSIC,
        }
    }

    // -------------------------------------------------------------- search

    fn pump_search(&mut self) {
        let v = self.indexer.version.load(SeqCst);
        if v != self.seen_version {
            self.seen_version = v;
        }
        // [Liam] A `pl:` search shows the playlist's own order; clicking a
        // heading sorts as usual.
        let is_pl = mentions_playlist(&self.query);
        if is_pl && !self.was_pl {
            (self.cfg.sort, self.cfg.ascending) = (SortKey::Playlist, true);
        } else if !is_pl && self.cfg.sort == SortKey::Playlist {
            (self.cfg.sort, self.cfg.ascending) = (SortKey::Name, true);
        }
        self.was_pl = is_pl;
        let Some(index) = self.indexer.index() else { return };
        let key = (self.query.clone(), self.cfg.opts, self.cfg.sort, self.cfg.ascending, self.pl_gen, v);
        if self.sent.as_ref() != Some(&key) {
            // Only the index version changed: a background refresh.
            let user_change = self
                .sent
                .as_ref()
                .is_none_or(|s| (&s.0, s.1, s.2, s.3, s.4) != (&key.0, key.1, key.2, key.3, key.4));
            self.sent = Some(key);
            self.sent_gen += 1;
            self.scroll_intent.sent(self.sent_gen, user_change);
            let _ = self.search_tx.send(SearchReq {
                generation: self.sent_gen,
                query: self.query.clone(),
                opts: self.cfg.opts,
                sort: self.cfg.sort,
                ascending: self.cfg.ascending,
                index,
                playlists: self.pls.clone(),
            });
        }
        let mut latest = None;
        while let Ok(r) = self.search_rx.try_recv() {
            latest = Some(r);
        }
        if let Some(r) = latest
            && r.generation > self.got_gen
        {
            self.got_gen = r.generation;
            self.results = r.ids;
            // Freeing a replaced index (100+ MB of columns) took ~8 ms; if
            // this was its last holder, let another thread do it.
            if let Some(old) = self.res_index.replace(r.index)
                && Arc::strong_count(&old) == 1
            {
                std::thread::spawn(move || drop(old));
            }
            self.res_error = r.error;
            self.highlights = r.highlights;
            self.res_missing = r.missing;
            self.res_lists = r.lists;
            let scroll = self.scroll_intent.arrived(r.generation);
            self.reselect(scroll);
        }
    }

    /// Keep the selected file selected across new results, scrolling to it
    /// only when `scroll` (the user changed the query, options or sort).
    fn reselect(&mut self, scroll: bool) {
        let old = self.sel_row.take();
        let (Some(path), Some(ix)) = (&self.sel_path, &self.res_index) else { return };
        match reselect_row(ix, &self.results, path, old) {
            Reselect::Same(row) => {
                self.sel_row = row;
                if scroll {
                    self.scroll_to = self.sel_row;
                }
            }
            // The file is gone (deleted or moved): its neighbour takes its
            // place, so the arrow keys carry on from there, not row 0 (B36).
            Reselect::Gone(Some(row)) => {
                self.select(row, false, false);
            }
            Reselect::Gone(None) => {
                self.sel_path = None;
                self.wsel = None;
            }
        }
    }

    fn entry_path(&self, row: usize) -> Option<PathBuf> {
        let ix = self.res_index.as_ref()?;
        let id = *self.results.get(row)? as usize;
        Some(ix.full_path(id))
    }

    /// Select `row`. With `play` (and Autoplay on), play it if it isn't
    /// already the current sound. Returns whether playback started.
    fn select(&mut self, row: usize, scroll: bool, play: bool) -> bool {
        if self.results.is_empty() {
            return false;
        }
        let row = row.min(self.results.len() - 1);
        let changed = self.sel_row != Some(row);
        self.sel_row = Some(row);
        self.sel_path = self.entry_path(row);
        if self.wsel.as_ref().map(|w| &w.0) != self.sel_path.as_ref() {
            self.wsel = None;
        }
        if scroll {
            self.scroll_to = Some(row);
        }
        let play = play
            && self.cfg.autoplay
            && (changed || self.player.current.as_ref().map(|c| &c.path) != self.sel_path.as_ref());
        if play {
            self.play_selected(0.0);
        }
        play
    }

    /// The waveform selection, if it belongs to `path`.
    fn selection_for(&self, path: &std::path::Path) -> Option<(f64, f64)> {
        self.wsel.as_ref().filter(|(p, _, _)| p == path).map(|(_, a, b)| (*a, *b))
    }

    /// Play the selected file from the start, or just its waveform selection.
    fn play_selected_or_range(&mut self) {
        let Some(path) = self.sel_path.clone() else { return };
        match self.selection_for(&path) {
            Some((a, b)) => self.player.play_range(path, a, Some(b)),
            None => self.player.play(path, 0.0),
        }
    }

    fn play_selected(&mut self, at: f64) {
        if let Some(p) = self.sel_path.clone() {
            self.player.play(p, at);
        }
    }

    /// Autoplay Next: a file that played to its end (not stopped with Esc,
    /// not a selection being auditioned) moves the list on one row and
    /// plays it. Files that won't play are skipped, up to `MAX_NEXT_FAILS`
    /// in a row.
    fn autoplay_next(&mut self) {
        if self.player.state() != State::Ended {
            return;
        }
        let Some(cur) = self.player.current.clone() else { return };
        if cur.load_gen == self.advanced_gen {
            return;
        }
        // Each ending is seen once, setting on or off: switching it on
        // later doesn't jump on from a file that ended long ago.
        self.advanced_gen = cur.load_gen;
        if !self.cfg.autoplay_next || self.cfg.looping {
            return;
        }
        self.next_fails = if self.player.error().is_some() { self.next_fails + 1 } else { 0 };
        if self.next_fails >= MAX_NEXT_FAILS {
            self.next_fails = 0;
            self.notice = Some(Notice::error(format!("Autoplay Next stopped: {MAX_NEXT_FAILS} files in a row wouldn't play")));
            return;
        }
        let row = self.res_index.as_ref().and_then(|ix| row_of(ix, &self.results, &cur.path));
        if let Some(next) = next_row(row, cur.end.is_some(), self.results.len()) {
            self.select(next, true, false);
            self.play_selected(0.0);
        }
    }

    // ----------------------------------------------------------- playlists

    fn playlist_dir(&self) -> PathBuf {
        self.cfg.playlist_dir.clone().unwrap_or_else(crate::playlist::default_dir)
    }

    /// Take in playlists read in the background (startup, the window
    /// regaining focus, a new folder), unless edited since they were asked
    /// for: the edit is newer, and its save is queued behind the read.
    fn pump_playlists(&mut self) {
        while let Ok(done) = self.pl_io.rx.try_recv() {
            match done {
                crate::playlist::Done::Loaded(tag, pls) if tag == self.pl_edits => {
                    if *pls != *self.pls {
                        self.pls = pls;
                        self.pl_gen += 1;
                    }
                    // Always: the indexer drops outside files no playlist
                    // names any more (a cached index may hold some).
                    self.indexer.set_extras(self.pls.all_paths());
                }
                crate::playlist::Done::Loaded(..) => {}
                crate::playlist::Done::Failed(e) => self.notice = Some(Notice::error(e)),
            }
        }
    }

    fn reload_playlists(&mut self) {
        self.pl_io.load(self.playlist_dir(), self.pl_edits);
    }

    /// Change playlist `name` (made if new), save it in the background and
    /// re-run the search.
    fn edit_playlist<R>(&mut self, name: &str, f: impl FnOnce(&mut Playlist) -> R) -> R {
        let pls = Arc::make_mut(&mut self.pls);
        let list = pls.get_or_create(name);
        let r = f(list);
        self.pl_io.save(list);
        self.pl_edits += 1;
        self.pl_gen += 1;
        self.indexer.set_extras(self.pls.all_paths());
        r
    }

    /// Ctrl+P: add the selected file (or, `all`, every result) to `name`.
    fn add_to_playlist(&mut self, name: &str, all: bool) {
        let paths: Vec<PathBuf> = if all {
            if self.results.len() > MAX_PLAYLIST_ADD {
                self.notice = Some(Notice::error(format!("Too many results to add (most is {})", fmt_int(MAX_PLAYLIST_ADD as u64))));
                return;
            }
            (0..self.results.len()).filter_map(|r| self.entry_path(r)).collect()
        } else {
            self.sel_path.iter().cloned().collect()
        };
        if paths.is_empty() {
            return;
        }
        let tried = paths.len();
        let added = self.edit_playlist(name, |l| l.add(paths));
        let name = self.pls.get(name).map_or(name.to_string(), |l| l.name.clone());
        let text = match (added, tried) {
            (0, 1) => format!("Already in {name}"),
            (0, _) => format!("All already in {name}"),
            (1, _) => format!("Added to {name}"),
            (a, _) => format!("Added {} to {name}", fmt_int(a as u64)),
        };
        self.notice = Some(Notice::info(&text));
    }

    /// Delete in a `pl:` search: take the selected row out of its
    /// playlists (never the file off the disk). Ctrl+Z puts it back.
    fn remove_from_playlist(&mut self) {
        let (Some(row), Some(path)) = (self.sel_row, self.sel_path.clone()) else { return };
        let lists: Vec<String> = self.res_lists.clone();
        let mut undo = Vec::new();
        for name in lists {
            if self.pls.get(&name).and_then(|l| l.index_of(&path)).is_some()
                && let Some(at) = self.edit_playlist(&name, |l| l.remove(&path))
            {
                undo.push(Removed { list: name, at, path: path.clone() });
            }
        }
        if undo.is_empty() {
            return;
        }
        let names: Vec<&str> = undo.iter().map(|u| u.list.as_str()).collect();
        self.notice = Some(Notice::info(&format!("Removed from {} · Ctrl+Z puts it back", names.join(", "))));
        self.pl_undo.push(undo);
        if self.pl_undo.len() > 100 {
            self.pl_undo.remove(0);
        }
        // Its neighbour takes the selection, as it moves into its row.
        let next = if row + 1 < self.results.len() { Some(row + 1) } else { row.checked_sub(1) };
        self.sel_path = next.and_then(|r| self.entry_path(r));
        self.sel_row = self.sel_path.as_ref().map(|_| row.min(self.results.len().saturating_sub(2)));
        self.wsel = None;
    }

    /// Ctrl+Z: put the last removed row back where it was.
    fn undo_remove(&mut self) {
        let Some(group) = self.pl_undo.pop() else { return };
        let path = group.first().map(|u| u.path.clone());
        for u in group.into_iter().rev() {
            self.edit_playlist(&u.list, |l| {
                if l.index_of(&u.path).is_none() {
                    l.paths.insert(u.at.min(l.paths.len()), u.path);
                }
            });
        }
        self.sel_path = path;
        self.notice = Some(Notice::info("Put back"));
    }

    /// Alt+↑ / Alt+↓: swap the selected row with the one above / below it
    /// in their playlist.
    fn move_in_playlist(&mut self, up: bool) {
        if self.res_lists.is_empty() {
            return;
        }
        if self.cfg.sort != SortKey::Playlist {
            (self.cfg.sort, self.cfg.ascending) = (SortKey::Playlist, true);
            self.notice = Some(Notice::info("Back in playlist order: Alt+↑ ↓ moves the file"));
            return;
        }
        let Some(row) = self.sel_row else { return };
        let other = if up { row.checked_sub(1) } else { Some(row + 1).filter(|&r| r < self.results.len()) };
        let (Some(other), Some(a)) = (other, self.sel_path.clone()) else { return };
        let Some(b) = self.entry_path(other) else { return };
        let lists = self.res_lists.clone();
        let target = lists.into_iter().find(|n| self.pls.get(n).is_some_and(|l| l.index_of(&a).is_some() && l.index_of(&b).is_some()));
        match target {
            Some(name) => {
                self.edit_playlist(&name, |l| {
                    if let (Some(i), Some(j)) = (l.index_of(&a), l.index_of(&b)) {
                        l.paths.swap(i, j);
                    }
                });
                self.sel_row = Some(other);
                self.scroll_to = Some(other);
            }
            None => self.notice = Some(Notice::info("Those two are in different playlists")),
        }
    }

    /// Open Ctrl+P's menu under the selected row (or the list's top).
    fn open_playlist_menu(&mut self, ctx: &egui::Context, all: bool) {
        if all && self.results.is_empty() || !all && self.sel_path.is_none() {
            self.notice = Some(Notice::info(if all { "No results to add" } else { "Select a file first" }));
            return;
        }
        let list = self.list_rect.unwrap_or(egui::Rect::from_min_size(egui::pos2(40.0, 80.0), Vec2::ZERO));
        let at = match self.sel_rect.filter(|r| !all && list.contains(r.left_bottom() - Vec2::new(-1.0, 1.0))) {
            Some(r) => r.left_bottom() + Vec2::new(NAME_PAD, 0.0),
            None => list.left_top() + Vec2::new(NAME_PAD + WAVE_PAD, 0.0),
        };
        let hot = self.res_lists.first().and_then(|n| self.pls.position(n)).unwrap_or(0);
        self.pl_menu = Some(PlMenu { all, hot, new_name: None, at, opened: ctx.cumulative_frame_nr() });
    }

    /// Keys while Ctrl+P's menu is open: ↑ ↓ move, Enter picks, Esc closes.
    fn playlist_menu_keys(&mut self, ctx: &egui::Context) {
        let Some(m) = self.pl_menu.as_mut() else { return };
        let n = self.pls.lists.len() + 1; // + "New…"
        let consume = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if consume(Key::Escape) {
            self.pl_menu = None;
            return;
        }
        if m.new_name.is_none() {
            if consume(Key::ArrowDown) {
                m.hot = (m.hot + 1) % n;
            }
            if consume(Key::ArrowUp) {
                m.hot = (m.hot + n - 1) % n;
            }
        }
        if consume(Key::Enter) {
            self.pick_playlist_item();
        }
    }

    /// The menu's highlighted item: a playlist (add to it), "New…" (type a
    /// name), or the typed name (make it and add).
    fn pick_playlist_item(&mut self) {
        let Some(m) = self.pl_menu.as_mut() else { return };
        let all = m.all;
        let name = match &m.new_name {
            Some(typed) => {
                let name = crate::playlist::clean_name(typed);
                if name.is_empty() {
                    return;
                }
                name
            }
            None => match self.pls.lists.get(m.hot) {
                Some(l) => l.name.clone(),
                None => {
                    m.new_name = Some(String::new());
                    return;
                }
            },
        };
        self.pl_menu = None;
        self.add_to_playlist(&name, all);
    }

    fn playlist_menu(&mut self, ctx: &egui::Context) {
        let Some(m) = &self.pl_menu else { return };
        let p = self.pal();
        let (at, hot, opened) = (m.at, m.hot, m.opened);
        let title = if m.all { format!("Add {} to", fmt_int(self.results.len() as u64)) } else { "Add to".to_string() };
        let mut new_name = m.new_name.clone();
        let mut picked: Option<usize> = None;
        let small = FontId::proportional(12.0);
        let area = egui::Area::new(egui::Id::unique("playlist-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(at)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if new_name.is_some() {
                        // Room for the name box: the menu was sized to the
                        // names last frame, which cut its hint to "Na…".
                        ui.set_min_width(NEW_NAME_W);
                    }
                    ui.label(RichText::new(title).font(small.clone()).color(p.dim));
                    // Items like egui's menu buttons (Semilight 12, padding
                    // 4 px across, 18 high), as wide as the longest name, so
                    // ↑ ↓ can highlight one without the pointer.
                    let font = egui::TextStyle::Button.resolve(ui.style());
                    let pad = ui.spacing().button_padding.x;
                    let names = self.pls.lists.iter().map(|l| l.name.as_str()).chain(["New…"]);
                    let widest = names.map(|t| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), p.text).size().x).fold(0.0, f32::max);
                    let w = (widest + 2.0 * pad).max(ui.min_rect().width());
                    let item = |ui: &mut egui::Ui, text: &str, is_hot: bool| {
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(w, ui.spacing().interact_size.y), Sense::click());
                        if is_hot || resp.hovered() {
                            ui.painter().rect_filled(r, 1.0, p.hover);
                        }
                        ui.painter().text(r.left_center() + Vec2::new(pad, 0.0), egui::Align2::LEFT_CENTER, text, font.clone(), p.text);
                        resp.clicked()
                    };
                    for (i, l) in self.pls.lists.iter().enumerate() {
                        if item(ui, &l.name, new_name.is_none() && i == hot) {
                            picked = Some(i);
                        }
                    }
                    let n = self.pls.lists.len();
                    match &mut new_name {
                        None => {
                            if item(ui, "New…", hot == n) {
                                picked = Some(n);
                            }
                        }
                        Some(text) => {
                            let id = egui::Id::unique("playlist-new-name");
                            ui.add(
                                egui::TextEdit::singleline(text)
                                    .id(id)
                                    .hint_text("Name, then Enter")
                                    .font(small.clone())
                                    .frame(
                                        egui::Frame::new()
                                            .fill(p.status_bg)
                                            .stroke(Stroke::new(1.0, p.sel_border))
                                            .corner_radius(1)
                                            .inner_margin(egui::Margin::symmetric(4, 1)),
                                    )
                                    .desired_width(NEW_NAME_W),
                            );
                            ui.memory_mut(|mem| mem.request_focus(id));
                        }
                    }
                });
            });
        if let Some(m) = self.pl_menu.as_mut() {
            m.new_name = new_name;
            if let Some(i) = picked {
                m.hot = i;
            }
        }
        if picked.is_some() {
            self.pick_playlist_item();
        }
        let rect = area.response.rect;
        let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|o| !rect.contains(o)));
        if outside && ctx.cumulative_frame_nr() != opened {
            self.pl_menu = None;
        }
    }

    /// Ctrl+P / Ctrl+Shift+P, and in a `pl:` search Delete, Ctrl+Z and
    /// Alt+↑ ↓. Tab after `pl:` fills in playlist names.
    fn playlist_keys(&mut self, ctx: &egui::Context, search_id: egui::Id) {
        let consume_m = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));
        if consume_m(Modifiers::COMMAND | Modifiers::SHIFT, Key::P) {
            self.open_playlist_menu(ctx, true);
        } else if consume_m(Modifiers::COMMAND, Key::P) {
            self.open_playlist_menu(ctx, false);
        }
        if !self.res_lists.is_empty() && self.sel_row.is_some() {
            if consume_m(Modifiers::ALT, Key::ArrowUp) {
                self.move_in_playlist(true);
            }
            if consume_m(Modifiers::ALT, Key::ArrowDown) {
                self.move_in_playlist(false);
            }
        }
        if self.list_focus {
            if !self.res_lists.is_empty() && consume_m(Modifiers::NONE, Key::Delete) {
                self.remove_from_playlist();
            }
            if !self.pl_undo.is_empty() && consume_m(Modifiers::COMMAND, Key::Z) {
                self.undo_remove();
            }
        } else if let Some(done) = complete_playlists(&self.query, &self.pls)
            && consume_m(Modifiers::NONE, Key::Tab)
        {
            match done {
                Ok(q) => {
                    self.query = q;
                    caret_to_end(ctx, search_id, &self.query);
                }
                Err(e) => self.notice = Some(Notice::info(&e)),
            }
        }
    }

    // ------------------------------------------------------------ keyboard

    fn handle_keys(&mut self, ctx: &egui::Context, search_id: egui::Id) {
        self.playlist_keys(ctx, search_id);
        let n = self.results.len();
        let cur = self.sel_row;
        let mut nav: Option<usize> = None;
        let page = self.page_rows.max(1);
        let list_focus = self.list_focus;
        let consume = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        let consume_m = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_key(m, k));

        if n > 0 {
            // Ctrl+↑: top of the list. Ctrl+↓: next file with a different name.
            if consume_m(Modifiers::COMMAND, Key::ArrowUp) {
                nav = Some(0);
            }
            if consume_m(Modifiers::COMMAND, Key::ArrowDown) {
                nav = Some(match (cur, &self.res_index) {
                    (Some(r), Some(ix)) if r < n => {
                        let results = &self.results;
                        next_different(n, r, |i| ix.name_lower(results[i] as usize))
                    }
                    _ => 0,
                });
            }
            if consume(Key::ArrowDown) {
                nav = Some(cur.map_or(0, |r| (r + 1).min(n - 1)));
            }
            if consume(Key::ArrowUp) {
                nav = Some(cur.map_or(0, |r| r.saturating_sub(1)));
            }
            if consume(Key::PageDown) {
                nav = Some(cur.map_or(0, |r| (r + page).min(n - 1)));
            }
            if consume(Key::PageUp) {
                nav = Some(cur.map_or(0, |r| r.saturating_sub(page)));
            }
            if consume_m(Modifiers::COMMAND, Key::Home) || (list_focus && consume(Key::Home)) {
                nav = Some(0);
            }
            if consume_m(Modifiers::COMMAND, Key::End) || (list_focus && consume(Key::End)) {
                nav = Some(n - 1);
            }
        }
        if let Some(r) = nav {
            self.list_focus = true;
            self.select(r, true, true);
        }

        // Ctrl+←: back to the start of the selected file, from the list or
        // the search box (where it would otherwise jump a word). If paused,
        // it stays paused there.
        if consume_m(Modifiers::COMMAND, Key::ArrowLeft) && self.sel_path.is_some() {
            let on_selected = self.player.current.as_ref().map(|c| &c.path) == self.sel_path.as_ref();
            if on_selected && matches!(self.player.state(), State::Playing | State::Paused) {
                self.player.seek(0.0);
            } else {
                self.play_selected(0.0);
            }
        }

        if consume(Key::Enter) {
            if self.sel_row.is_none() && n > 0 {
                self.select(0, true, false);
            }
            self.list_focus = true;
            self.play_selected_or_range();
        }

        if self.list_focus {
            // Space toggles playback; it must not reach the search box.
            let space = ctx.input_mut(|i| {
                let had = i.events.iter().any(|e| matches!(e, Event::Text(t) if t == " "));
                i.events.retain(|e| !matches!(e, Event::Text(t) if t == " "));
                i.consume_key(Modifiers::NONE, Key::Space) || had
            });
            if space {
                let other = self.player.current.as_ref().map(|c| &c.path) != self.sel_path.as_ref();
                if other || matches!(self.player.state(), State::Idle | State::Ended) {
                    self.play_selected_or_range();
                } else {
                    self.player.toggle_pause();
                }
            }
            if let Some(d) = consume_seek(ctx) {
                let on_selected = self.player.current.as_ref().map(|c| &c.path) == self.sel_path.as_ref();
                if on_selected {
                    let pos = self.player.position();
                    self.player.seek(pos + d);
                } else {
                    // Stopped (Esc) or another file is playing: start the
                    // selected file, the one the waveform shows, at the step.
                    self.play_selected(d.max(0.0));
                }
            }
            // Ctrl+C copies the selected file's full path.
            let copy = ctx.input_mut(|i| {
                let had = i.events.iter().any(|e| matches!(e, Event::Copy));
                i.events.retain(|e| !matches!(e, Event::Copy));
                had
            });
            if copy && let Some(p) = &self.sel_path {
                ctx.copy_text(p.to_string_lossy().into_owned());
                self.notice = Some(Notice::info("Path copied"));
            }
            // Typing goes back to the search box.
            let typing = ctx.input(|i| {
                i.events.iter().any(|e| {
                    matches!(e, Event::Text(_) | Event::Paste(_))
                        || matches!(e, Event::Key { key: Key::Backspace | Key::Delete, pressed: true, .. })
                })
            });
            if typing {
                self.list_focus = false;
            }
        }

        if consume(Key::Escape) {
            if self.show_help {
                self.show_help = false;
            } else if self.player.state() == State::Playing || self.player.state() == State::Paused {
                self.player.stop();
            } else if self.wsel.is_some() {
                self.wsel = None;
            } else {
                self.list_focus = false;
            }
        }
        // Ctrl+1 … Ctrl+0: a bookmarked folder in the search box, caret at
        // the end to narrow it down.
        for (k, key) in BOOKMARK_KEYS {
            if consume_m(Modifiers::COMMAND, key) {
                match self.cfg.bookmarks.iter().find(|m| m.key == Some(k)) {
                    Some(m) => {
                        self.query = bookmark_query(&m.path);
                        self.list_focus = false;
                        caret_to_end(ctx, search_id, &self.query);
                    }
                    None => self.notice = Some(Notice::info(&format!("No bookmark on Ctrl+{} (File › Bookmarks)", key_label(k)))),
                }
            }
        }
        if consume_m(Modifiers::COMMAND, Key::F) || consume(Key::F3) || consume_m(Modifiers::COMMAND, Key::L) {
            self.list_focus = false;
            select_all(ctx, search_id, &self.query);
        }
        if consume(Key::Tab) {
            self.list_focus = !self.list_focus;
            if self.list_focus && self.sel_row.is_none() && n > 0 {
                self.select(0, true, true);
            }
        }
    }

    // ------------------------------------------------------------------ ui

    fn menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Folders…").clicked() {
                    self.show_folders = true;
                    ui.close();
                }
                if ui.button("Bookmarks…").clicked() {
                    self.show_bookmarks = true;
                    ui.close();
                }
                if ui.button("Playlists…").clicked() {
                    self.show_playlists = true;
                    self.reload_playlists();
                    ui.close();
                }
                if ui.button("Rebuild Index").clicked() {
                    self.indexer.rescan();
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Search", |ui| {
                ui.checkbox(&mut self.cfg.opts.match_case, "Match Case");
                ui.checkbox(&mut self.cfg.opts.match_path, "Match Path");
                ui.checkbox(&mut self.cfg.opts.regex, "Enable Regex");
            });
            ui.menu_button("Playback", |ui| {
                ui.checkbox(&mut self.cfg.autoplay, "Autoplay on Select")
                    .on_hover_text("Play a file as soon as it's selected. Off: Enter, Space or double-click plays.");
                ui.checkbox(&mut self.cfg.autoplay_next, "Autoplay Next")
                    .on_hover_text("When a file plays to its end, play the next one in the list. Loop wins.");
                ui.separator();
                if ui.checkbox(&mut self.cfg.looping, "Loop").on_hover_text("Repeat the file, or the waveform selection.").changed() {
                    self.player.set_loop(self.cfg.looping);
                }
                if ui
                    .checkbox(&mut self.cfg.loop_xfade, "Crossfade Loop")
                    .on_hover_text("Blend each loop's end into its start over 10 ms. Off: the loop repeats sample-exact.")
                    .changed()
                {
                    self.player.set_loop_crossfade(self.cfg.loop_xfade);
                }
                ui.separator();
                ui.menu_button("Output", |ui| self.output_menu(ui));
            });
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.cfg.dark, "Dark").clicked() {
                    ui.close();
                }
                let dark = self.cfg.dark;
                if ui
                    .add_enabled(dark, egui::Checkbox::new(&mut self.cfg.classic_dark, "Classic Dark"))
                    .on_hover_text("The earlier dark colours: one grey for the list, search box and waveform.")
                    .clicked()
                {
                    ui.close();
                }
            });
            let help = ui.menu_button("Help", |ui| {
                if ui.button("Search Syntax").clicked() {
                    self.show_help = true;
                    self.help_opened = ui.ctx().cumulative_frame_nr();
                    ui.close();
                }
                if ui.button("Reset Folders to Defaults").clicked() {
                    self.cfg.roots = default_roots();
                    self.indexer.set_roots(self.cfg.roots.clone());
                    ui.close();
                }
            });
            self.help_anchor = help.response.rect.left_bottom();
        });
    }

    /// Playback › Output: the system default (followed when it changes), or
    /// one device to stay on.
    fn output_menu(&mut self, ui: &mut egui::Ui) {
        // Listing devices asks the OS (about a millisecond): once each time
        // the submenu opens, never per frame.
        let frame = ui.ctx().cumulative_frame_nr();
        if self.out_devices_frame + 1 < frame {
            self.out_devices = crate::player::output_devices();
            // The chosen device was missing and is back: return to it.
            if self.player.on_fallback
                && let Some(d) = &self.cfg.output_device
                && self.out_devices.iter().any(|o| o.id == d.id)
            {
                self.player.set_device(Some(d.id.clone()));
            }
        }
        self.out_devices_frame = frame;
        let chosen = self.cfg.output_device.clone();
        let mut pick = None;
        let default = if cfg!(windows) { "Windows Default" } else { "System Default" };
        if ui.radio(chosen.is_none(), default).on_hover_text("Follows the default playback device, also when it changes.").clicked() {
            pick = Some(None);
        }
        ui.separator();
        for d in &self.out_devices {
            if ui.radio(chosen.as_ref().is_some_and(|c| c.id == d.id), &d.name).clicked() {
                pick = Some(Some(d.clone()));
            }
        }
        if let Some(c) = &chosen
            && !self.out_devices.iter().any(|o| o.id == c.id)
        {
            ui.radio(true, format!("{} (not connected)", c.name))
                .on_hover_text("Playing through the default device until this one is back.");
        }
        if let Some(d) = pick {
            self.player.set_device(d.as_ref().map(|d| d.id.clone()));
            self.notice = match (&self.player.device_error, &d) {
                (Some(e), _) => Some(Notice::error(e.clone())),
                (None, Some(d)) if self.player.on_fallback => Some(Notice::error(format!("{} won't open: playing through the default device", d.name))),
                (None, Some(d)) => Some(Notice::info(&format!("Output: {}", d.name))),
                (None, None) => Some(Notice::info(&format!("Output: {default}"))),
            };
            self.cfg.output_device = d;
            ui.close();
        }
    }

    fn header_cell(&mut self, ui: &mut egui::Ui, title: &str, key: Option<SortKey>, right: bool, pad: f32) {
        let p = self.pal();
        let active = key.is_some() && key == Some(self.cfg.sort);
        let layout = if right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
        let resp = ui
            .with_layout(layout, |ui| {
                ui.add_space(pad); // same as the column's rows, so text lines up
                ui.add(
                    egui::Label::new(RichText::new(title).color(p.header_text))
                        .selectable(false)
                        .truncate(),
                )
            })
            .inner;
        let full = ui.interact(ui.max_rect(), ui.scope_id().with(title), Sense::click());
        let gap = ui.spacing().item_spacing.x * 0.5;
        self.header_rects.push(ui.max_rect().expand2(Vec2::new(gap, 0.0)));
        if active {
            // Sort chevron centred along the top edge, like native list views.
            let r = ui.max_rect();
            let (cx, y) = (r.center().x, r.top() + 3.0);
            let (dy, w) = (3.0, 4.0);
            let pts = if self.cfg.ascending {
                [egui::pos2(cx - w, y + dy), egui::pos2(cx, y), egui::pos2(cx + w, y + dy)]
            } else {
                [egui::pos2(cx - w, y), egui::pos2(cx, y + dy), egui::pos2(cx + w, y)]
            };
            ui.painter().add(egui::Shape::line(pts.to_vec(), Stroke::new(1.0, p.dim)));
        }
        if let Some(k) = key
            && (full.clicked() || resp.clicked())
        {
            if self.cfg.sort == k {
                self.cfg.ascending = !self.cfg.ascending;
            } else {
                self.cfg.sort = k;
                self.cfg.ascending = matches!(k, SortKey::Name | SortKey::Path);
            }
        }
    }

    fn name_job(&self, name: &str, color: Color32, font: &FontId) -> LayoutJob {
        let p = self.pal();
        let mut marks = vec![false; name.len()];
        let lower = name.to_ascii_lowercase();
        for h in &self.highlights {
            if h.is_empty() || !h.is_ascii() {
                continue;
            }
            let mut from = 0;
            while let Some(i) = lower[from..].find(h.as_str()) {
                let s = from + i;
                marks[s..s + h.len()].iter_mut().for_each(|m| *m = true);
                from = s + h.len().max(1);
            }
        }
        let bold = if self.has_bold {
            FontId::new(font.size, FontFamily::Name("bold".into()))
        } else {
            font.clone()
        };
        let mut job = LayoutJob::default();
        // Some names start with spaces (" (Freeze) ….wav"); showing them
        // would indent those rows. Search still sees the real name.
        let mut i = name.len() - name.trim_start().len();
        while i < name.len() {
            let m = marks[i];
            let mut j = i;
            while j < name.len() && (marks[j] == m || !name.is_char_boundary(j)) {
                j += 1;
            }
            let fmt = if m {
                TextFormat { font_id: bold.clone(), color: if self.has_bold { color.lerp_to_gamma(p.list_bg, 0.12) } else { p.accent }, ..Default::default() }
            } else {
                TextFormat { font_id: font.clone(), color, ..Default::default() }
            };
            job.append(&name[i..j], 0.0, fmt);
            i = j;
        }
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        let Some(ix) = self.res_index.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Indexing…").color(p.dim));
            });
            return;
        };
        let results = self.results.clone();
        let font = FontId::proportional(13.0);
        let mut clicked: Option<(usize, bool)> = None;
        let mut dragged: Option<usize> = None;
        let mut reveal: Option<PathBuf> = None;
        let mut copy_dir: Option<String> = None;
        let ctrl = ui.input(|i| i.modifiers.command);
        // Whole-row scrolling: the top row is always a full row lined up
        // under the headings. Wheel, keyboard and our own scroll bar move by
        // rows; anything else (resizing the window) is floored to a row next
        // frame. One blank spacer row after the last lets the last real row
        // sit fully in view with the top still on a row boundary.
        let total_rows = results.len() + 1;
        let view_h = self.list_rect.map_or(0.0, |r| r.height());
        let mut top =
            list_top_row(self.list_offset, view_h, total_rows, std::mem::take(&mut self.list_wheel_rows), self.scroll_to.take());
        let (view_rows, max_top) = list_geom(view_h, total_rows);
        self.page_rows = view_rows;

        // Our scroll bar fills the last LIST_BAR_WIDTH points of the
        // window, flush against the black border that ends the last column
        // (`list_border_x`). egui's own bar is hidden: it can't be outlined,
        // and ours moves in whole rows by construction.
        let area = ui.available_rect_before_wrap();
        let ppp = ui.ctx().pixels_per_point();
        let border_x = list_border_x(area.right());
        // Right edge of the border's physical pixel.
        let bar_left = ((border_x * ppp).round() + 1.0) / ppp;
        // The last heading's outline (the border) sits half a column gap
        // past its cell, so the table stops that much short of it.
        let table_rect = egui::Rect::from_min_max(area.min, egui::pos2(border_x - ui.spacing().item_spacing.x * 0.5, area.bottom()));
        let track = self.list_rect.map(|b| egui::Rect::from_min_max(egui::pos2(bar_left, b.top()), egui::pos2(area.right(), b.bottom())));
        let mut bar_hovered = false;
        if let Some(track) = track {
            let resp = ui.interact(track, ui.scope_id().with("list-scrollbar"), Sense::click_and_drag());
            let (hy, len) = bar_handle(track.top(), track.height(), top, max_top, view_rows, total_rows);
            bar_hovered = resp.hovered() || resp.is_pointer_button_down_on();
            if resp.is_pointer_button_down_on() && max_top > 0 {
                // Grab the handle where it was pressed; a press on the track
                // grabs its middle, so the handle jumps under the pointer.
                let grab = *self.bar_grab.get_or_insert_with(|| {
                    let y = ui.input(|i| i.pointer.press_origin()).map_or(hy + len * 0.5, |o| o.y);
                    if (hy..=hy + len).contains(&y) { y - hy } else { len * 0.5 }
                });
                if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                    top = bar_top_at(pos.y - grab, track.top(), track.height(), len, max_top);
                }
            } else {
                self.bar_grab = None;
            }
        }
        let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(table_rect).layout(*ui.layout()));
        // egui_extras draws its column resize lines unsnapped (a soft 2 px
        // blur at 150 %) and a pixel off the heading outlines. Hide them and
        // draw our own below, on the heading dividers.
        let w = &mut tui.visuals_mut().widgets;
        (w.noninteractive.bg_stroke, w.hovered.bg_stroke, w.active.bg_stroke) = (Stroke::NONE, Stroke::NONE, Stroke::NONE);
        // The table keeps room for its (hidden) scroll bar on the right;
        // ours is outside the table, so it needs none.
        let sc = &mut tui.spacing_mut().scroll;
        (sc.bar_width, sc.bar_inner_margin, sc.bar_outer_margin) = (0.0, 0.0, 0.0);

        let mut tb = TableBuilder::new(&mut tui)
            .id_salt("results")
            .striped(false)
            .resizable(true)
            .sense(Sense::click_and_drag())
            .drag_to_scroll(egui::containers::scroll_area::DragScroll::Never)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(320.0).at_least(60.0).clip(true))
            .column(Column::initial(420.0).at_least(60.0).clip(true))
            .column(Column::initial(84.0).at_least(40.0).clip(true))
            .column(Column::initial(90.0).at_least(40.0).clip(true))
            .column(Column::remainder().at_least(60.0).clip(true))
            .animate_scrolling(false)
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        if self.list_rect.is_some() {
            tb = tb.vertical_scroll_offset(top as f32 * ROW_H);
        }
        let sel_row = self.sel_row;
        let sel_fill = if self.list_focus { p.sel } else { p.sel_inactive };
        let playing_path = self
            .player
            .current
            .as_ref()
            .filter(|_| matches!(self.player.state(), State::Playing | State::Paused) && !self.player.loading())
            .map(|c| c.path.clone());
        let infos = self.infos.clone();
        let mut marker: Option<egui::Pos2> = None;
        let mut sel_rect: Option<egui::Rect> = None;
        self.header_rects.clear();
        let out = tb.header(22.0, |mut h| {
            h.col(|ui| self.header_cell(ui, "Name", Some(SortKey::Name), false, NAME_PAD));
            h.col(|ui| self.header_cell(ui, "Path", Some(SortKey::Path), false, CELL_PAD));
            h.col(|ui| self.header_cell(ui, "Length", None, true, CELL_PAD));
            h.col(|ui| self.header_cell(ui, "Size", Some(SortKey::Size), true, CELL_PAD));
            h.col(|ui| self.header_cell(ui, "Date Modified", Some(SortKey::Modified), false, CELL_PAD));
        })
        .body(|body| {
            body.rows(ROW_H, total_rows, |mut row| {
                let r = row.index();
                if r >= results.len() {
                    return; // the blank spacer row
                }
                let id = results[r] as usize;
                let selected = sel_row == Some(r);
                row.set_selected(selected);
                let name = ix.name(id);
                let path = ix.full_path(id);
                let is_playing = playing_path.as_ref() == Some(&path);
                let color = p.text;
                let job = self.name_job(name, color, &font);
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                        sel_rect = Some(ui.max_rect());
                    }
                    // The ▶ marker is painted into the left padding, so it
                    // never pushes the name sideways, clear of the black
                    // line at WAVE_PAD (see `wave_frame`).
                    if is_playing {
                        // Painted after the table, on the grey gutter.
                        marker = Some(egui::pos2(ui.max_rect().left() + 2.0, ui.max_rect().center().y));
                    }
                    ui.add_space(NAME_PAD);
                    ui.add(egui::Label::new(job).selectable(false).truncate());
                });
                let (path_cell, _) = row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.add_space(CELL_PAD);
                    ui.add(egui::Label::new(RichText::new(ix.dir_str(id)).size(SUB_FONT).color(color)).selectable(false).truncate());
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    let len = match infos.get(&path, (ix.size[id], ix.mtime[id])) {
                        Some(Some(Info { duration: Some(d), .. })) => fmt_len(d),
                        _ => String::new(),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(CELL_PAD);
                        ui.add(egui::Label::new(RichText::new(len).size(SUB_FONT).color(color)).selectable(false));
                    });
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(CELL_PAD);
                        ui.add(egui::Label::new(RichText::new(fmt_size(ix.size[id])).size(SUB_FONT).color(color)).selectable(false));
                    });
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.add_space(CELL_PAD);
                    ui.add(egui::Label::new(RichText::new(fmt_date(ix.mtime[id])).size(SUB_FONT).color(color)).selectable(false).truncate());
                });
                let resp = row.response();
                // Ctrl+click on the Path cell opens the folder; anywhere
                // else in the row it's an ordinary click.
                let on_path = |pos: Option<egui::Pos2>| ctrl && pos.is_some_and(|p| path_cell.contains(p));
                if on_path(resp.hover_pos()) {
                    resp.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                // Right-click on the Path cell copies the folder path shown.
                if resp.secondary_clicked() && resp.interact_pointer_pos().is_some_and(|p| path_cell.contains(p)) {
                    copy_dir = Some(ix.dir_str(id).to_string());
                }
                if resp.clicked() && on_path(resp.interact_pointer_pos()) {
                    reveal = Some(path.clone());
                } else if resp.clicked() {
                    clicked = Some((r, false));
                }
                if resp.double_clicked() {
                    clicked = Some((r, true));
                }
                if resp.drag_started() {
                    dragged = Some(r);
                }
            });
        });
        self.list_offset = out.state.clamped_offset().y;
        self.list_rect = Some(out.inner_rect);
        self.sel_rect = sel_rect;
        if !on_row_grid(self.list_offset) {
            ui.ctx().request_repaint(); // snap to a whole row next frame
        }
        if let Some(track) = track {
            // Light grey handle with a pencil-thin blue outline.
            let painter = ui.painter();
            painter.rect_filled(track, 0.0, p.scroll_track);
            // [Liam] Beside the headings the scroll column is the frame's
            // grey too, with no line under it: the handle stops there.
            if let Some(head) = self.header_rects.first() {
                let beside = egui::Rect::from_x_y_ranges(track.x_range(), head.top()..=track.top());
                painter.rect_filled(beside, 0.0, p.scroll_track);
            }
            if max_top > 0 {
                let (hy, len) = bar_handle(track.top(), track.height(), top, max_top, view_rows, total_rows);
                let handle = egui::Rect::from_x_y_ranges(track.x_range(), hy..=hy + len).shrink(0.5);
                let fill = if bar_hovered { p.scroll_handle_hover } else { p.scroll_handle };
                // [Liam] Red while it rests against either end.
                let edge = if top == 0 || top >= max_top { p.scroll_end } else { p.header_text };
                painter.rect(handle, 1.0, fill, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
            }
        }
        let painter = ui.painter();
        let px = |v: f32| ((v * ppp).round() + 0.5) / ppp; // centre of a physical pixel
        // The gutter left of the Name column, up to the black line at
        // WAVE_PAD (`wave_frame`): the waveform pads' grey, headings to the
        // bottom of the list, the ▶ marker on top.
        if let (Some(head), Some(list)) = (self.header_rects.first(), self.list_rect) {
            let right = ((area.left() + WAVE_PAD) * ppp).round() / ppp;
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(area.left(), head.top()), egui::pos2(right, list.bottom())), 0.0, p.frame_bg);
            if let Some(at) = marker.filter(|m| list.y_range().contains(m.y)) {
                painter.text(at, egui::Align2::LEFT_CENTER, "▶", FontId::proportional(9.0), p.accent);
            }
        }
        // [Liam] The headings' top line is the first row that isn't grey:
        // at some scales (100 %) the list's dark fill starts a fraction of a
        // pixel above it and smudged it to two pixels. Grey up to the line.
        if let Some(head) = self.header_rects.first() {
            let line = (head.top() * ppp).round();
            let above = egui::Rect::from_min_max(egui::pos2(area.left(), (line - 2.0) / ppp), egui::pos2(ui.ctx().content_rect().right(), line / ppp));
            painter.with_clip_rect(egui::Rect::EVERYTHING).rect_filled(above, 0.0, p.frame_bg);
        }
        // Pencil-thin outline around each heading. Drawn here, outside the
        // header cells, because each cell clips to its own rect. [Liam] The
        // first starts on column L (`wave_frame` draws that side): no line
        // runs over the grey gutter.
        let col_l = ((area.left() + WAVE_PAD) * ppp).round() - 0.5;
        for (i, r) in self.header_rects.iter().enumerate() {
            let left = if i == 0 { col_l / ppp } else { px(r.left()) };
            let r = egui::Rect::from_min_max(egui::pos2(left, px(r.top())), egui::pos2(px(r.right()), px(r.bottom() - 1.0)));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0 / ppp, p.header_border), egui::StrokeKind::Middle);
        }
        // Column dividers down the body: the heading outlines carried on,
        // same pixel column, same one-pixel width, always black: hovering
        // or dragging one only changes the cursor. The last heading's right
        // edge carries on as the border beside the scroll bar.
        if let (Some(head), Some(list)) = (self.header_rects.first(), self.list_rect) {
            let (top, bottom) = (px(head.bottom() - 1.0), list.bottom());
            for r in &self.header_rects {
                painter.vline(px(r.right()), top..=bottom, Stroke::new(1.0 / ppp, p.header_border));
            }
        }

        if let Some(r) = dragged
            && DRAG_OUT_SUPPORTED
        {
            self.list_focus = true;
            self.select(r, false, false);
            self.drag_out = self.sel_path.clone().map(DragOut::File);
        }

        if let Some(dir) = copy_dir {
            ui.ctx().copy_text(dir);
            self.notice = Some(Notice::info("Folder path copied"));
        }
        if let Some(path) = reveal
            && let Err(e) = reveal_in_explorer(&path)
        {
            self.notice = Some(Notice::error(format!("Couldn't open the folder: {e}")));
        }

        if let Some((r, dbl)) = clicked {
            self.list_focus = true;
            if dbl {
                self.select(r, false, false);
                if self.click_play.double(r) {
                    self.play_selected_or_range();
                }
            } else {
                let started = self.select(r, false, true);
                self.click_play.single(r, started);
            }
        }
    }

    /// Black one-pixel lines framing the waveform: its top edge (also the
    /// resize handle) and bottom edge across the window, a line on each side
    /// of the wave between it and its pads, and the left one carried up
    /// through the list to the top of the headings. Drawn after the list so
    /// it sits on top of it.
    fn wave_frame(&self, ui: &egui::Ui, wave: egui::Rect) {
        let p = self.pal();
        let ppp = ui.ctx().pixels_per_point();
        let stroke = Stroke::new(1.0 / ppp, p.header_border);
        // The physical pixel starting at `v`, and the one ending at it.
        let after = |v: f32| ((v * ppp).round() + 0.5) / ppp;
        let before = |v: f32| ((v * ppp).round() - 0.5) / ppp;
        let painter = ui.painter();
        let (top, bottom) = (after(wave.top()), before(wave.bottom()));
        let (left, right) = (before(wave.left() + WAVE_PAD), after(wave.right() - WAVE_PAD));
        // [Liam] The top and bottom lines run between the side lines only:
        // the grey pads join the gutter, scroll column and status bar with
        // no line across them.
        painter.hline(left..=right, top, stroke);
        painter.hline(left..=right, bottom, stroke);
        let list_top = self.header_rects.first().map_or(wave.top(), |h| h.top());
        painter.vline(left, after(list_top)..=bottom, stroke);
        painter.vline(right, top..=bottom, stroke);
    }

    fn waveform(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        // As tall as the (resizable) panel; the top border is drawn by the
        // caller, over the panel's resize handle.
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, p.bg);
        // The wave is inset by the same pad on both sides, framed by black
        // lines drawn by the caller (`wave_frame`). `resp` still covers both
        // pads: a click on the left one maps to 0:00, on the right one to the
        // end. Both pads are the status bar's grey.
        let wave_right = rect.right() - WAVE_PAD;
        for x in [rect.left()..=rect.left() + WAVE_PAD, wave_right..=rect.right()] {
            painter.rect_filled(egui::Rect::from_x_y_ranges(x, rect.y_range()), 0.0, p.frame_bg);
        }
        let rect = egui::Rect::from_min_max(rect.min + Vec2::new(WAVE_PAD, 0.0), egui::pos2(wave_right, rect.bottom()));
        let Some(path) = self.sel_path.clone().or_else(|| self.player.current.as_ref().map(|c| c.path.clone())) else {
            return;
        };
        // The file's size and mtime from the index: a file rewritten in
        // place gets a fresh waveform and Length (B34).
        let stamp = self.res_index.as_ref().and_then(|ix| ix.find(&path).map(|i| (ix.size[i], ix.mtime[i]))).unwrap_or((0, 0));
        let wave = self.waves.get(&path, stamp);
        let is_current = self.player.current.as_ref().is_some_and(|c| c.path == path);
        let from_player = if is_current { self.player.info() } else { None };
        if let Some(i) = &from_player {
            self.infos.put(&path, stamp, i);
        }
        let info = from_player.or_else(|| self.infos.get(&path, stamp).flatten());
        let peaks = wave.peaks.read();
        let block = *wave.block.read();
        let expected = wave.expected_blocks.read().unwrap_or(peaks.len()).max(1);
        let duration = info
            .as_ref()
            .and_then(|i| i.duration.or_else(|| (i.rate > 0).then(|| (expected * block) as f64 / i.rate as f64)));
        let pos = if is_current { self.player.position() } else { 0.0 };
        let state = self.player.state();
        let frac = match (duration, is_current && state != State::Idle) {
            (Some(d), true) if d > 0.0 => (pos / d).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        };
        let w = rect.width().max(1.0);
        let mid = rect.center().y;
        let half = rect.height() * 0.5 - 4.0;
        let play_x = rect.left() + frac * w;
        let sel = self.selection_for(&path);
        // Selection as an x range on screen.
        let sel_x = match (sel, duration.filter(|d| *d > 0.0)) {
            (Some((a, b)), Some(d)) => Some((rect.left() + (a / d) as f32 * w, rect.left() + (b / d) as f32 * w)),
            _ => None,
        };
        if let Some((xa, xb)) = sel_x {
            let r = egui::Rect::from_x_y_ranges(xa..=xb.max(xa + 1.0), rect.top() + 1.0..=rect.bottom());
            painter.rect_filled(r, 0.0, p.accent.gamma_multiply(0.16));
            for x in [xa, xb] {
                painter.line_segment([egui::pos2(x, r.top()), egui::pos2(x, r.bottom())], Stroke::new(1.0, p.accent.gamma_multiply(0.7)));
            }
        }
        let cols = w as usize;
        let mut mesh = egui::Mesh::default();
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| (v * ppp).round() / ppp;
        let play_at = (frac > 0.0).then_some(play_x);
        for x in 0..cols {
            let b0 = x * expected / cols;
            let b1 = ((x + 1) * expected / cols).max(b0 + 1);
            if b0 >= peaks.len() {
                break;
            }
            let (mut lo, mut hi) = (0f32, 0f32);
            for &(a, b) in &peaks[b0..b1.min(peaks.len())] {
                lo = lo.min(a);
                hi = hi.max(b);
            }
            let px = rect.left() + x as f32;
            let y0 = snap(mid - hi.clamp(-1.0, 1.0) * half);
            let y1 = snap(mid - lo.clamp(-1.0, 1.0) * half).max(y0 + 1.0 / ppp);
            let c = p.wave_color(px + 0.5, play_at, sel_x);
            mesh.add_colored_rect(egui::Rect::from_x_y_ranges(px..=px + 1.0, y0..=y1), c);
        }
        painter.add(egui::Shape::mesh(mesh));
        drop(peaks);
        if wave.failed.load(Relaxed) {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No preview", FontId::proportional(12.0), p.dim);
        }
        if let (Some((a, b)), Some(_)) = (sel, duration) {
            let txt = format!("{} – {}   ({:.3} s)", fmt_time(a), fmt_time(b), b - a);
            let font = FontId::proportional(11.0);
            let galley = painter.layout_no_wrap(txt, font, p.text);
            let at = rect.left_top() + Vec2::new(4.0, 3.0);
            painter.rect_filled(egui::Rect::from_min_size(at, galley.size()).expand(2.0), 2.0, p.bg.gamma_multiply(0.85));
            painter.galley(at, galley, p.text);
        }
        // Crowns go on top of the wave and the label: a small white
        // downward triangle on each selection edge...
        let top = rect.top() + 1.0;
        if let Some((xa, xb)) = sel_x {
            for x in [xa, xb] {
                let pts = vec![
                    egui::pos2(x - CROWN_HALF, top),
                    egui::pos2(x + CROWN_HALF, top),
                    egui::pos2(x, top + CROWN_HALF + 1.0),
                ];
                painter.add(egui::Shape::convex_polygon(pts, p.crown, Stroke::new(1.0, p.crown_edge)));
            }
        }
        // ...and a purple playhead with a small black-outlined orange square
        // on top. Hidden once playback has ended, so it doesn't sit at the
        // end of the timeline after a sound finishes.
        if let Some(x) = play_at.filter(|_| state != State::Ended && !(is_current && self.player.loading())) {
            painter.line_segment([egui::pos2(x, top), egui::pos2(x, rect.bottom())], Stroke::new(1.0, p.playhead));
            let h = PLAYHEAD_CROWN_HALF;
            // Keep the whole crown inside the strip at either end.
            let cx = x.clamp(rect.left() + h + 1.0, rect.right() - h - 1.0);
            // Flush under the strip's black top border: the crown's fill
            // starts on the pixel row below it, so its one-pixel black
            // outline's top side is the border itself. Snapped, so the
            // outline is one crisp pixel.
            let snap_px = |v: f32| (v * ppp).round() / ppp;
            let y0 = snap_px(rect.top()) + 1.0 / ppp;
            let x0 = snap_px(cx - h);
            let crown = egui::Rect::from_min_size(egui::pos2(x0, y0), Vec2::splat(snap_px(h * 2.0)));
            painter.rect(crown, 0.0, p.playhead_crown, Stroke::new(1.0 / ppp, Color32::BLACK), egui::StrokeKind::Outside);
        }

        // ---- interaction ------------------------------------------------
        // Left click: seek. Left drag: select (auditions on release). Left drag
        // from inside the selection: drag it out as a WAV. [Liam] Right drag:
        // a new selection, from anywhere, inside the old one too. Ctrl+left
        // drag: scrub. Right click: seek, keeping the selection.
        let Some(d) = duration.filter(|d| *d > 0.0) else { return };
        let t_at = |x: f32| ((x - rect.left()) / w).clamp(0.0, 1.0) as f64 * d;
        let in_sel = |t: f64| sel.is_some_and(|(a, b)| t >= a && t <= b);
        let ctx = ui.ctx().clone();
        // Inside the selection, playing stays inside it (it still stops or
        // loops at its end); anywhere else, the selection goes.
        let seek_or_play = |me: &mut Self, t: f64, clear: bool| {
            if let Some((a, b)) = sel.filter(|_| in_sel(t)) {
                me.player.play_range_from(path.clone(), a, b, t);
                return;
            }
            if clear {
                me.wsel = None;
            }
            if is_current && matches!(state, State::Playing | State::Paused) {
                me.player.seek(t);
            } else {
                me.player.play(path.clone(), t);
            }
        };

        if let Some(h) = resp.hover_pos() {
            let hx = h.x.clamp(rect.left(), rect.right());
            painter.line_segment(
                [egui::pos2(hx, rect.top() + 1.0), egui::pos2(hx, rect.bottom())],
                Stroke::new(1.0, p.dim.gamma_multiply(0.6)),
            );
            let ctrl = ctx.input(|i| i.modifiers.command);
            let icon = if self.wave_scrub || (ctrl && self.wave_drag.is_none()) {
                egui::CursorIcon::ResizeHorizontal
            } else if DRAG_OUT_SUPPORTED && self.wave_drag.is_none() && in_sel(t_at(h.x)) {
                egui::CursorIcon::Grab
            } else {
                egui::CursorIcon::Text
            };
            ctx.set_cursor_icon(icon);
        }

        use egui::PointerButton::{Primary, Secondary};
        let (left, right) = (resp.drag_started_by(Primary), resp.drag_started_by(Secondary));
        if left && ctx.input(|i| i.modifiers.command) {
            self.wave_scrub = true;
        } else if left || right {
            let origin = ctx.input(|i| i.pointer.press_origin()).map(|o| t_at(o.x)).unwrap_or(0.0);
            if left && DRAG_OUT_SUPPORTED && let Some((a, b)) = sel.filter(|_| in_sel(origin)) {
                self.drag_out = Some(DragOut::Region(path.clone(), a, b));
            } else {
                self.wave_drag = Some(origin);
                self.wsel = Some((path.clone(), origin, origin));
            }
        }
        let selecting = (resp.dragged_by(Primary) && !self.wave_scrub) || resp.dragged_by(Secondary);
        if let (Some(anchor), true) = (self.wave_drag, selecting)
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let t = t_at(pos.x);
            self.wsel = Some((path.clone(), anchor.min(t), anchor.max(t)));
        }
        if resp.drag_stopped_by(Primary) {
            self.wave_scrub = false;
        }
        if (resp.drag_stopped_by(Primary) || resp.drag_stopped_by(Secondary)) && self.wave_drag.take().is_some() {
            match self.selection_for(&path) {
                // Anything under ~3 px is a sloppy click, not a selection.
                Some((a, b)) if (b - a) / d * w as f64 >= 3.0 => {
                    self.player.play_range(path.clone(), a, Some(b));
                }
                _ => self.wsel = None,
            }
        }
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            seek_or_play(self, t_at(pos.x), true);
        }
        let moved = ctx.input(|i| i.pointer.delta().x != 0.0);
        if (resp.secondary_clicked() || (self.wave_scrub && resp.dragged_by(Primary) && moved))
            && let Some(pos) = resp.interact_pointer_pos()
        {
            seek_or_play(self, t_at(pos.x), false); // scrubbing keeps the selection
        }
    }

    fn status(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let notice = self.notice.as_ref().and_then(|n| n.remaining().map(|left| (n, left)));
            let left = if let Some((n, left)) = notice {
                ui.ctx().request_repaint_after(Duration::from_secs_f32(left));
                let color = if n.error { Color32::from_rgb(0xC4, 0x2B, 0x1C) } else { p.dim };
                RichText::new(&n.text).color(color)
            } else if let Some(e) = &self.res_error {
                RichText::new(e).color(Color32::from_rgb(0xC4, 0x2B, 0x1C))
            } else if self.res_index.is_none() {
                RichText::new(indexing_text(&self.indexer)).color(p.text)
            } else {
                let n = self.results.len() as u64;
                let mut s = format!("{} {}", fmt_int(n), if n == 1 { "object" } else { "objects" });
                if self.res_missing > 0 {
                    s += &format!("   ·   {} missing", fmt_int(self.res_missing as u64));
                }
                if self.indexer.scanning.load(Relaxed) {
                    s += &format!("   ·   {}", indexing_text(&self.indexer));
                }
                RichText::new(s).color(p.text)
            };
            // A long notice is cut short ("…") before it reaches the volume.
            let right_min = 8.0 + VOL_LABEL_W + 160.0 + 14.0 + TRANSPORT_H + 4.0 * ui.spacing().item_spacing.x;
            let max_left = (ui.available_width() - right_min).max(40.0);
            ui.scope(|ui| {
                ui.set_max_width(max_left);
                ui.add(egui::Label::new(left).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                self.volume_ui(ui);
                ui.add_space(14.0);
                // While a new file is loading, keep what was shown, so a file
                // that ends or fails at once doesn't flash ▶ (B5).
                if !self.player.loading() {
                    self.status_shown = self.player_status(p);
                }
                if let Some((icon, s, color)) = &self.status_shown {
                    // A narrow window drops the format, then the times, so
                    // nothing runs over the text on the left.
                    let room = ui.available_width() - TRANSPORT_H - 2.0 * ui.spacing().item_spacing.x;
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let fits = |t: &str| ui.painter().layout_no_wrap(t.to_owned(), font.clone(), *color).size().x <= room;
                    let short = s.split("    ").next().unwrap_or("");
                    let s = if fits(s) {
                        s.as_str()
                    } else if icon.is_some() {
                        if fits(short) { short } else { "" }
                    } else {
                        s.as_str() // an error: truncated with "…" below
                    };
                    let mut resp = ui.add(egui::Label::new(RichText::new(s).color(*color)).truncate().sense(Sense::click()));
                    if let Some(icon) = icon {
                        // Right to left: the icon goes left of the text.
                        let size = Vec2::new(TRANSPORT_H, resp.rect.height());
                        let (r, icon_resp) = ui.allocate_exact_size(size, Sense::click());
                        paint_transport(ui.painter(), egui::pos2(r.center().x, resp.rect.center().y), *icon, *color);
                        resp = resp.union(icon_resp);
                    }
                    if self.player.current.is_some() && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.go_to_playing();
                    }
                }
            });
        });
    }

    /// Select and scroll to the file that's playing (clicking ▶ in the
    /// status bar), wherever the list has been scrolled to.
    fn go_to_playing(&mut self) {
        let Some(path) = self.player.current.as_ref().map(|c| c.path.clone()) else { return };
        match self.res_index.as_ref().and_then(|ix| row_of(ix, &self.results, &path)) {
            Some(r) => {
                self.list_focus = true;
                self.select(r, true, false);
            }
            None => self.notice = Some(Notice::info("The playing file isn't in these results")),
        }
    }

    /// Right-hand status text for the player, or None when idle.
    fn player_status(&self, p: Palette) -> Option<(Option<Transport>, String, Color32)> {
        if let Some(e) = &self.player.device_error {
            return Some((None, e.clone(), p.dim));
        }
        if let Some(e) = self.player.error() {
            return Some((None, e, Color32::from_rgb(0xC4, 0x2B, 0x1C)));
        }
        let state = self.player.state();
        if state == State::Idle {
            return None;
        }
        let icon = match state {
            State::Playing => Transport::Play,
            State::Paused => Transport::Pause,
            _ => Transport::Stop,
        };
        let mut s = String::new();
        if let Some(i) = self.player.info() {
            let d = i.duration.unwrap_or(0.0);
            let pos = self.player.position().min(d.max(0.0));
            s += &format!("{} / {}", fmt_time(pos), fmt_time(d));
            let desc = i.describe();
            if !desc.is_empty() {
                s += &format!("    {desc}");
            }
        }
        Some((Some(icon), s, p.text))
    }

    /// Volume: a label, then a fader from -inf to +24 dB (`fader_db` spreads
    /// the travel: finest round 0 dB). Double-click resets to 0 dB, the wheel
    /// moves 0.5 dB, right-click picks the clipping mode used when boosting.
    fn volume_ui(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| (v * ppp).round() / ppp; // a pixel edge
        let thin = Stroke::new(1.0 / ppp, p.header_border);

        // ---- label (right to left: it sits right of the fader)
        let db = self.cfg.volume_db;
        let label = if db <= MIN_DB {
            "-∞ dB".to_string()
        } else if db == 0.0 {
            "0.0 dB".to_string()
        } else {
            format!("{db:+.1} dB")
        };
        let boost = db > 0.0;
        let color = if boost { BOOST } else { p.text };
        let (slot, _) = ui.allocate_exact_size(Vec2::new(VOL_LABEL_W, 16.0), Sense::hover());
        let galley = ui.painter().layout_no_wrap(label, egui::TextStyle::Body.resolve(ui.style()), color);
        let at = egui::pos2(snap(slot.left() + 4.0), slot.center().y - galley.size().y / 2.0);
        if boost {
            // [Liam] Boosting: the orange label in a pencil-thin black box,
            // hugging the text.
            let r = egui::Rect::from_min_size(at, galley.size()).expand2(Vec2::new(4.0, 0.0));
            let r = egui::Rect::from_min_max(egui::pos2(snap(r.left()), snap(r.top())), egui::pos2(snap(r.right()), snap(r.bottom())));
            ui.painter().rect_stroke(r.shrink(0.5 / ppp), 0.0, thin, egui::StrokeKind::Middle);
        }
        ui.painter().galley(at, galley, color);

        // ---- fader. egui places and drives the handle; we paint the groove
        // and the handle ourselves (egui's are made invisible here).
        let mut t = fader_pos(self.cfg.volume_db);
        let resp = ui
            .scope(|ui| {
                ui.spacing_mut().slider_width = 160.0;
                let v = ui.visuals_mut();
                v.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: FADER_ASPECT };
                for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
                    w.bg_fill = Color32::TRANSPARENT;
                    w.fg_stroke = Stroke::NONE;
                }
                ui.add(egui::Slider::new(&mut t, 0.0..=1.0).show_value(false))
            })
            .inner
            .on_hover_text("Volume · double-click for 0 dB · right-click for clipping");
        let mut changed = resp.changed();
        if changed {
            self.cfg.volume_db = (fader_db(t) * 10.0).round() / 10.0;
        }
        // Sliders only sense drags; add clicks for double-click and right-click.
        let resp = resp.union(ui.interact(resp.rect, egui::Id::unique("volume-clicks"), Sense::click()));
        if resp.double_clicked() {
            self.cfg.volume_db = 0.0;
            changed = true;
        }
        if resp.hovered() {
            let steps = ui.input(|i| wheel_steps(&i.events, &mut self.wheel_acc, 1.0, WHEEL_POINTS_PER_STEP));
            if steps != 0 {
                self.cfg.volume_db = (self.cfg.volume_db + steps as f32 * 0.5).clamp(MIN_DB, MAX_DB);
                changed = true;
            }
        } else {
            self.wheel_acc = 0.0;
        }

        {
            let visuals = *ui.style().interact(&resp);
            let widgets = &ui.visuals().widgets;
            let edge = widgets.inactive.fg_stroke.color;
            let r = resp.rect;
            // egui's geometry (`SliderGeometry`): handle radius from the
            // height; the centre travels between the rect's ends inset by
            // half a handle.
            let half_w = r.height() / 2.5 * FADER_ASPECT;
            let (x0, x1) = (r.left() + half_w, r.right() - half_w);
            let half = ui.spacing().slider_rail_height / 2.0;
            let painter = ui.painter();
            // Groove: between the handle's end positions, so its square
            // ends hide under the handle. [Liam] Light theme: `groove` grey
            // with a pencil-thin outline in the handle's colour.
            let groove = egui::Rect::from_min_max(
                egui::pos2(snap(x0), snap(r.center().y - half)),
                egui::pos2(snap(x1), snap(r.center().y + half)),
            );
            if self.cfg.dark {
                painter.rect_filled(groove, widgets.inactive.corner_radius, widgets.inactive.bg_fill);
            } else {
                painter.rect(groove.shrink(0.5 / ppp), 0.0, p.groove, Stroke::new(1.0 / ppp, edge), egui::StrokeKind::Middle);
            }
            // [Liam] Handle: a fader cap a little bigger than egui's round
            // one, with three pencil-thin gripper lines across its travel.
            // Laid out in physical pixels: an odd width, so the middle
            // gripper has a centre column and the cap is symmetric round it.
            let t = fader_pos(self.cfg.volume_db);
            let mid = (egui::lerp(x0..=x1, t) * ppp).floor();
            let half_px = (((FADER_CAP.x * ppp).round() as i32) | 1) / 2;
            let cap = egui::Rect::from_min_max(
                egui::pos2((mid - half_px as f32) / ppp, snap(r.center().y - FADER_CAP.y / 2.0)),
                egui::pos2((mid + half_px as f32 + 1.0) / ppp, snap(r.center().y + FADER_CAP.y / 2.0)),
            );
            let ink = visuals.fg_stroke.color;
            painter.rect(cap.shrink(0.5 / ppp), 1.0, visuals.bg_fill, Stroke::new(1.0 / ppp, ink), egui::StrokeKind::Middle);
            let gap = (2.0 * ppp).round();
            for k in [-1.0, 0.0, 1.0] {
                let x = (mid + k * gap + 0.5) / ppp;
                painter.vline(x, (cap.top() + 4.0)..=(cap.bottom() - 4.0), Stroke::new(1.0 / ppp, ink));
            }
        }

        resp.context_menu(|ui| {
            ui.label(RichText::new("When boosting above 0 dB").color(p.dim));
            if ui.radio_value(&mut self.cfg.soft_clip, true, "Soft Clip").clicked()
                || ui.radio_value(&mut self.cfg.soft_clip, false, "Hard Clip").clicked()
            {
                self.player.set_soft_clip(self.cfg.soft_clip);
                ui.close();
            }
        });
        if changed {
            self.player.set_volume_db(self.cfg.volume_db);
        }
    }

    /// The search box's caret: one physical pixel wide, as tall as the text,
    /// blinking 0.5 s on / 0.5 s off and shown solid right after it moves.
    fn paint_caret(&mut self, ui: &egui::Ui, out: &egui::text_edit::TextEditOutput, color: Color32) {
        let window_focused = ui.input(|i| i.focused);
        let Some(range) = out.cursor_range.filter(|_| out.response.has_focus() && window_focused) else { return };
        let now = ui.input(|i| i.time);
        let key = (range.primary.index.0, range.secondary.index.0, self.query.len());
        if self.caret_key != Some(key) {
            self.caret_key = Some(key);
            self.caret_at = now;
        }
        let phase = (now - self.caret_at) % 1.0;
        ui.ctx().request_repaint_after(Duration::from_secs_f64(if phase < 0.5 { 0.5 - phase } else { 1.0 - phase }));
        if phase >= 0.5 {
            return;
        }
        let r = out.galley.pos_from_cursor(range.primary).translate(out.galley_pos.to_vec2());
        let ppp = ui.ctx().pixels_per_point();
        let x = ((r.center().x * ppp).floor() + 0.5) / ppp; // centre of one physical pixel
        let half = CARET_H * 0.5;
        let (y0, y1) = (r.center().y - half, r.center().y + half);
        ui.painter()
            .with_clip_rect(out.text_clip_rect.expand(1.0))
            .line_segment([egui::pos2(x, y0), egui::pos2(x, y1)], Stroke::new(1.0 / ppp, color));
    }

    fn folders_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_folders;
        let mut changed = false;
        let p = self.pal();
        let small = FontId::proportional(12.0);
        // egui centres the title between the left edge and the × button, so
        // it sits left of centre. An invisible spacer as wide as the × (one
        // heading row square) balances it over the whole window.
        // Resolve the font first: reading the style inside `fonts_mut`
        // deadlocks the context (it froze the app).
        let heading = egui::TextStyle::Heading.resolve(&ctx.global_style());
        let close_w = ctx.fonts_mut(|f| f.row_height(&heading));
        let title = (egui::Atom::custom(egui::IdSalt::new("folders-title-pad"), Vec2::splat(close_w)), "Folders");
        egui::Window::new(title)
            // A new id drops the 460 px width an earlier, resizable version
            // saved; the window now sizes to its content.
            .id(egui::Id::unique("folders-v2"))
            .open(&mut open)
            .collapsible(false)
            // Sized to the content: `resizable(false)` alone keeps egui's
            // 320 px default width.
            .auto_sized()
            // The right edge sits 9 px past the + square (8 margin + 1 border).
            .frame(egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin { left: 6, right: 8, top: 6, bottom: 6 }))
            .show(ctx, |ui| {
                ui.label(RichText::new("Indexed").font(small.clone()).color(p.dim));
                ui.add_space(4.0);
                let mut remove = None;
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (i, r) in self.cfg.roots.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if icon_square(ui, p, false).on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                            ui.label(RichText::new(r.to_string_lossy()).font(small.clone()));
                        });
                    }
                    if self.cfg.roots.is_empty() {
                        ui.label(RichText::new("No folders").font(small.clone()).color(p.dim));
                    }
                });
                if let Some(i) = remove {
                    self.cfg.roots.remove(i);
                    changed = true;
                }
                ui.add_space(6.0);
                if let Some(dir) = folder_entry_row(ui, p, &mut self.folder_input, "folder-input")
                    && add_root(&mut self.cfg.roots, &dir)
                {
                    self.folder_input.clear();
                    changed = true;
                }
            });
        self.show_folders = open;
        if changed {
            self.indexer.set_roots(self.cfg.roots.clone());
        }
    }

    /// File › Bookmarks: the Folders window's twin, for bookmarked folders.
    /// Each row: × to remove, its key (click to change), the folder.
    fn bookmarks_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_bookmarks;
        let p = self.pal();
        let small = FontId::proportional(12.0);
        let heading = egui::TextStyle::Heading.resolve(&ctx.global_style());
        let close_w = ctx.fonts_mut(|f| f.row_height(&heading));
        let title = (egui::Atom::custom(egui::IdSalt::new("bookmarks-title-pad"), Vec2::splat(close_w)), "Bookmarks");
        egui::Window::new(title)
            .id(egui::Id::unique("bookmarks"))
            .open(&mut open)
            .collapsible(false)
            .auto_sized()
            .frame(egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin { left: 6, right: 8, top: 6, bottom: 6 }))
            .show(ctx, |ui| {
                ui.label(RichText::new("Ctrl + key").font(small.clone()).color(p.dim));
                ui.add_space(4.0);
                let mut remove = None;
                let mut rekey = None;
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (i, m) in self.cfg.bookmarks.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if icon_square(ui, p, false).on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                            let resp = key_square(ui, p, m.key);
                            egui::Popup::menu(&resp).show(|ui| {
                                for k in 1..=10u8 {
                                    if ui.button(format!("Ctrl+{}", key_label(k))).clicked() {
                                        rekey = Some((i, Some(k)));
                                    }
                                }
                                if ui.button("None").clicked() {
                                    rekey = Some((i, None));
                                }
                            });
                            ui.label(RichText::new(m.path.to_string_lossy()).font(small.clone()));
                        });
                    }
                    if self.cfg.bookmarks.is_empty() {
                        ui.label(RichText::new("No bookmarks").font(small.clone()).color(p.dim));
                    }
                });
                if let Some(i) = remove {
                    self.cfg.bookmarks.remove(i);
                }
                if let Some((i, k)) = rekey {
                    assign_key(&mut self.cfg.bookmarks, i, k);
                }
                ui.add_space(6.0);
                if let Some(dir) = folder_entry_row(ui, p, &mut self.bookmark_input, "bookmark-input")
                    && add_bookmark(&mut self.cfg.bookmarks, &dir)
                {
                    self.bookmark_input.clear();
                }
            });
        self.show_bookmarks = open;
    }

    /// File › Playlists: the Folders window's twin. Where the playlists
    /// live (Browse… to move them, e.g. to a synced drive), and each
    /// playlist: click to search it.
    fn playlists_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_playlists;
        let p = self.pal();
        let small = FontId::proportional(12.0);
        let heading = egui::TextStyle::Heading.resolve(&ctx.global_style());
        let close_w = ctx.fonts_mut(|f| f.row_height(&heading));
        let title = (egui::Atom::custom(egui::IdSalt::new("playlists-title-pad"), Vec2::splat(close_w)), "Playlists");
        let mut search: Option<String> = None;
        let mut new_dir: Option<PathBuf> = None;
        egui::Window::new(title)
            .id(egui::Id::unique("playlists"))
            .open(&mut open)
            .collapsible(false)
            .auto_sized()
            .frame(egui::Frame::window(&ctx.global_style()).inner_margin(egui::Margin { left: 6, right: 8, top: 6, bottom: 6 }))
            .show(ctx, |ui| {
                ui.label(RichText::new("Click to search  ·  Ctrl+P adds").font(small.clone()).color(p.dim));
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for l in &self.pls.lists {
                        ui.horizontal(|ui| {
                            let resp = ui
                                .add(egui::Label::new(RichText::new(&l.name).font(small.clone())).sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(l.file.to_string_lossy());
                            if resp.clicked() {
                                search = Some(l.name.clone());
                            }
                            ui.label(RichText::new(fmt_int(l.paths.len() as u64)).font(small.clone()).color(p.dim));
                        });
                    }
                    if self.pls.lists.is_empty() {
                        ui.label(RichText::new("No playlists").font(small.clone()).color(p.dim));
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new("Folder").font(small.clone()).color(p.dim));
                ui.label(RichText::new(self.pls.dir.to_string_lossy()).font(small.clone()));
                ui.add_space(2.0);
                if let Some(dir) = folder_entry_row(ui, p, &mut self.playlist_input, "playlist-input") {
                    new_dir = Some(dir);
                }
            });
        self.show_playlists = open;
        if let Some(name) = search {
            self.query = playlist_query(&[&name]);
            self.list_focus = false;
            caret_to_end(ctx, egui::Id::unique("search"), &self.query);
        }
        if let Some(dir) = new_dir {
            self.playlist_input.clear();
            self.cfg.playlist_dir = (dir != crate::playlist::default_dir()).then(|| dir.clone());
            // Edits from here on go to the new folder; a read of the old
            // one still under way is stale.
            self.pl_edits += 1;
            self.pls = Arc::new(Playlists { dir, lists: Vec::new() });
            self.pl_gen += 1;
            self.reload_playlists();
        }
    }

    /// Search Syntax: a small pop-out under the Help menu, styled like the
    /// menus. Closes on Esc or a click anywhere outside it.
    fn help_window(&mut self, ctx: &egui::Context) {
        let p = self.pal();
        const SEARCH: &[(&str, &str)] = &[
            ("door slam", "both words"),
            ("kick | snare", "either"),
            ("!loop", "not"),
            ("<a | b> c", "group"),
            ("\"door slam\"", "exact phrase"),
            ("*.wav  kick_??", "wildcards"),
            (r"foley\door", "match the path"),
            (r"F:\…\x.wav", "a pasted path"),
            ("ext:wav;flac", "file types"),
            ("size:>2mb  1mb..5mb", "size"),
            ("size:small", "tiny small large huge"),
            ("case:  path:  regex:", "per word"),
            ("pl:Impacts", "a playlist (pl: Tab: all)"),
        ];
        let mut keys: Vec<(&str, &str)> = vec![
            ("↑ ↓  PgUp PgDn", "select (Autoplay plays)"),
            ("Ctrl+Home / End", "first / last"),
            ("Ctrl+↑ / ↓", "top / next new name"),
            ("Enter, double-click", "play"),
            ("Space", "play / pause"),
            ("← →  (Shift)", "seek 1 s (5 s)"),
            ("Ctrl+←", "back to start"),
            ("Esc", "stop, clear selection"),
            ("Tab", "search ↔ list"),
            ("Ctrl+F  /  Ctrl+C", "search  /  copy path"),
            ("Ctrl+1 … 0", "bookmarked folder"),
            ("Ctrl+P  (Shift)", "add file (all) to playlist"),
            ("Del  /  Ctrl+Z", "out of playlist / undo"),
            ("Alt+↑ / ↓", "move in playlist"),
            ("Ctrl+click path", "open in Explorer"),
            ("Right-click path", "copy folder"),
            ("Click ▶ status", "go to playing file"),
            ("Click / drag wave", "seek / select"),
            ("Right-drag wave", "new selection"),
            ("Ctrl+drag wave", "scrub"),
        ];
        if DRAG_OUT_SUPPORTED {
            keys.push(("Drag row / selection", "copy file / WAV"));
        }
        let font = FontId::proportional(12.0);
        let column = |ui: &mut egui::Ui, id: &str, title: &str, rows: &[(&str, &str)]| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).font(font.clone()).color(p.header_text));
                egui::Grid::new(id).num_columns(2).spacing([10.0, 1.0]).show(ui, |ui| {
                    for (a, b) in rows {
                        ui.label(RichText::new(*a).font(font.clone()).color(p.text));
                        ui.label(RichText::new(*b).font(font.clone()).color(p.dim));
                        ui.end_row();
                    }
                });
            });
        };
        let area = egui::Area::new(egui::Id::unique("help-popout"))
            .order(egui::Order::Foreground)
            .fixed_pos(self.help_anchor)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        column(ui, "help-search", "Search", SEARCH);
                        ui.add_space(14.0);
                        column(ui, "help-keys", "Keys and mouse", &keys);
                    });
                });
            });
        let rect = area.response.rect;
        let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.press_origin().is_some_and(|o| !rect.contains(o)));
        if outside && ctx.cumulative_frame_nr() != self.help_opened {
            self.show_help = false;
        }
    }

    fn apply_style(&self, ctx: &egui::Context) {
        let p = self.pal();
        let mut v = if self.cfg.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = p.bg;
        v.window_fill = p.bg;
        v.extreme_bg_color = p.bg;
        v.override_text_color = Some(p.text);
        v.selection.bg_fill = p.sel;
        v.selection.stroke = Stroke::new(1.0, p.text);
        v.widgets.hovered.weak_bg_fill = p.hover;
        v.widgets.hovered.bg_fill = p.hover;
        v.window_corner_radius = CornerRadius::same(2);
        v.menu_corner_radius = CornerRadius::same(2);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(1);
        }
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.grid);
        v.text_edit_bg_color = Some(p.bg);
        // A 1 px caret, like native Windows text boxes (egui's is 2 px).
        v.text_cursor.stroke = Stroke::new(1.0, p.text);
        let theme = if self.cfg.dark { egui::Theme::Dark } else { egui::Theme::Light };
        ctx.set_theme(theme);
        ctx.set_visuals_of(theme, v);
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(6.0, 0.0);
            s.spacing.interact_size.y = 18.0;
            // Menus and pop-outs sit snug round their text (egui: 6 px).
            s.spacing.menu_margin = egui::Margin::symmetric(3, 2);
            s.interaction.selectable_labels = false;
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.0));
            // Buttons (and menu items) and window titles: smaller and in
            // Segoe UI Semilight, with padding hugging the text (egui: 8×2).
            let light = FontFamily::Name("light".into());
            s.text_styles.insert(egui::TextStyle::Button, FontId::new(12.0, light.clone()));
            s.text_styles.insert(egui::TextStyle::Heading, FontId::new(13.0, light));
            s.spacing.button_padding = Vec2::new(4.0, 1.0);
            s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.0));
            // A thin scroll bar that's always there (not one that appears on
            // hover), with a handle 30% taller than egui's minimum so it's
            // easy to grab when a million rows shrink it to that minimum.
            let mut scroll = egui::style::ScrollStyle::solid();
            scroll.bar_width = SCROLL_BAR_WIDTH;
            scroll.handle_min_length = (scroll.handle_min_length * SCROLL_HANDLE_SCALE).round();
            // egui dims the first and last rows of a scrolled list with a
            // fade; every row should read the same.
            scroll.fade.strength = 0.0;
            s.spacing.scroll = scroll;
        });
    }
}

/// Autoplay Next gives up after this many files in a row fail to play.
const MAX_NEXT_FAILS: u32 = 20;

/// The row Autoplay Next plays after `row` (the file that just ended):
/// none after the last row, or after a selection was auditioned.
fn next_row(row: Option<usize>, was_range: bool, len: usize) -> Option<usize> {
    row.filter(|_| !was_range).map(|r| r + 1).filter(|&r| r < len)
}

/// Player state icon in the status bar.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Transport {
    Play,
    Pause,
    Stop,
}

/// Height (and box width) of the status bar's ▶ ‖ ■ icons, in points:
/// about the digits' height at 13 px.
const TRANSPORT_H: f32 = 9.0;

/// The icon's shapes in physical pixels, centred on (0, 0): one triangle,
/// two bars or one square, all `TRANSPORT_H` tall (the square a little
/// smaller, so it doesn't look bigger than the others). Bars and square sit
/// on whole pixels so their edges are sharp.
fn transport_shapes(icon: Transport, ppp: f32) -> Vec<Vec<egui::Pos2>> {
    let h = (TRANSPORT_H * ppp).round();
    let rect = |x0: f32, y0: f32, w: f32, h: f32| vec![egui::pos2(x0, y0), egui::pos2(x0 + w, y0), egui::pos2(x0 + w, y0 + h), egui::pos2(x0, y0 + h)];
    match icon {
        Transport::Play => {
            let w = (h * 0.87).round();
            vec![vec![egui::pos2(-w / 2.0, -h / 2.0), egui::pos2(w / 2.0, 0.0), egui::pos2(-w / 2.0, h / 2.0)]]
        }
        Transport::Pause => {
            let bar = (h * 0.32).round().max(1.0);
            let gap = (h * 0.26).round().max(1.0);
            let x0 = -((2.0 * bar + gap) / 2.0).round();
            vec![rect(x0, -h / 2.0, bar, h), rect(x0 + bar + gap, -h / 2.0, bar, h)]
        }
        Transport::Stop => {
            let side = (h * 0.84).round();
            // x from the (whole-pixel) centre, y from the (whole-pixel) top.
            vec![rect(-(side / 2.0).round(), -h / 2.0 + ((h - side) / 2.0).floor(), side, side)]
        }
    }
}

fn paint_transport(painter: &egui::Painter, center: egui::Pos2, icon: Transport, color: Color32) {
    let ppp = painter.ctx().pixels_per_point();
    // The centre on a pixel corner (or centre, for an odd height), so every
    // edge lands on the pixel grid.
    let h = (TRANSPORT_H * ppp).round();
    let snap = |v: f32| ((v * ppp - h / 2.0).round() + h / 2.0) / ppp;
    let (cx, cy) = ((center.x * ppp).round() / ppp, snap(center.y));
    for pts in transport_shapes(icon, ppp) {
        let pts = pts.into_iter().map(|q| egui::pos2(cx + q.x / ppp, cy + q.y / ppp)).collect();
        painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    }
}

const DRAG_OUT_SUPPORTED: bool = cfg!(windows);
const SCROLL_HANDLE_SCALE: f32 = 1.3;
const SCROLL_BAR_WIDTH: f32 = 8.0;
/// The list's own scroll bar: as wide as the waveform's right pad, so its
/// left edge and the waveform's right line are one straight column.
const LIST_BAR_WIDTH: f32 = WAVE_PAD;
/// x of the black border that ends the list's last column, given the
/// window content's right edge: the scroll bar is right of it. The search
/// box ends on it too.
fn list_border_x(right: f32) -> f32 {
    right - LIST_BAR_WIDTH
}
/// Search caret height in points: about the height of the 13.5 pt text.
const CARET_H: f32 = 13.0;
/// Shortest the list's scroll handle gets (egui's 12 px, 30% taller).
const SCROLL_HANDLE_MIN: f32 = 16.0;
/// Left padding of the Name column: room for the ▶ playing marker, so every
/// name starts at the same x whether or not it's playing.
const NAME_PAD: f32 = 16.0;
/// Padding between a column's border and its text.
const CELL_PAD: f32 = 6.0;
/// Rows the list moves per mouse-wheel notch (Windows' default).
const WHEEL_ROWS_PER_NOTCH: f32 = 3.0;
/// Half the width of the waveform's selection and playhead crowns.
const CROWN_HALF: f32 = 3.0;
/// Half the size of the playhead's square crown (4 px, plus a 1 px outline).
const PLAYHEAD_CROWN_HALF: f32 = 2.0;

enum DragOut {
    File(PathBuf),
    /// A waveform selection, exported to its own WAV when the drag starts.
    Region(PathBuf, f64, f64),
}

/// Hand a file to the OS drag-and-drop as a copy: the drop target (Explorer,
/// a DAW, a mail client...) receives its own copy of the file.
#[cfg(windows)]
fn start_os_drag(frame: &eframe::Frame, path: PathBuf) {
    let icon = include_bytes!("../assets/drag.png").to_vec();
    let opts = drag::Options { skip_animatation_on_cancel_or_failure: false, mode: drag::DragMode::Copy };
    let _ = drag::start_drag(frame, drag::DragItem::Files(vec![path]), drag::Image::Raw(icon), |_, _| {}, opts);
}

#[cfg(not(windows))]
fn start_os_drag(_frame: &eframe::Frame, _path: PathBuf) {}

/// A small square with an × (remove) or + (add) in it, for list entries.
/// The Folders and Bookmarks windows' last row: Browse…, an entry box and
/// a + square. Returns a folder picked, or typed and confirmed with + or
/// Enter (only if it exists).
fn folder_entry_row(ui: &mut egui::Ui, p: Palette, input: &mut String, id: &str) -> Option<PathBuf> {
    let small = FontId::proportional(12.0);
    ui.horizontal(|ui| {
        if ui.button("Browse…").clicked()
            && let Some(dir) = rfd::FileDialog::new().pick_folder()
        {
            return Some(dir);
        }
        // An obvious entry box: its own fill and outline (blue while
        // typing), "…" until something is typed.
        let id = ui.make_persistent_id(id);
        let focused = ui.memory(|m| m.focused()) == Some(id);
        let te = ui.add(
            egui::TextEdit::singleline(input)
                .id(id)
                .hint_text("…")
                .font(small)
                .frame(
                    egui::Frame::new()
                        .fill(p.status_bg)
                        .stroke(Stroke::new(1.0, if focused { p.sel_border } else { p.dim.gamma_multiply(0.6) }))
                        .corner_radius(1)
                        .inner_margin(egui::Margin::symmetric(4, 1)),
                )
                .desired_width(74.0),
        );
        let enter = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if icon_square(ui, p, true).on_hover_text("Add this folder").clicked() || enter {
            let pb = PathBuf::from(input.trim());
            if pb.is_dir() {
                return Some(pb);
            }
        }
        None
    })
    .inner
}

/// A bookmark's key in a 13 px square like the × and + squares: its digit,
/// or "·" for none. Click for a menu of keys.
fn key_square(ui: &mut egui::Ui, p: Palette, key: Option<u8>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(13.0), Sense::click());
    let ppp = ui.ctx().pixels_per_point();
    let snap = |v: f32| ((v * ppp).round() + 0.5) / ppp;
    let r = egui::Rect::from_min_max(egui::pos2(snap(rect.left()), snap(rect.top())), egui::pos2(snap(rect.right()), snap(rect.bottom())));
    let hot = resp.hovered();
    let (fill, edge, mark) = if hot { (p.hover, p.dim, p.text) } else { (Color32::TRANSPARENT, p.dim.gamma_multiply(0.6), p.text) };
    let painter = ui.painter();
    painter.rect(r, 1.0, fill, Stroke::new(1.0 / ppp, edge), egui::StrokeKind::Middle);
    let label = key.map_or("·".to_string(), key_label);
    painter.text(r.center(), egui::Align2::CENTER_CENTER, label, FontId::proportional(11.0), if key.is_some() { mark } else { p.dim });
    let hover = key.map_or("No key: click to set one".to_string(), |k| format!("Ctrl+{}: click to change", key_label(k)));
    resp.on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn icon_square(ui: &mut egui::Ui, p: Palette, plus: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(13.0), Sense::click());
    let ppp = ui.ctx().pixels_per_point();
    let snap = |v: f32| ((v * ppp).round() + 0.5) / ppp; // centre of a physical pixel
    let r = egui::Rect::from_min_max(egui::pos2(snap(rect.left()), snap(rect.top())), egui::pos2(snap(rect.right()), snap(rect.bottom())));
    let hot = resp.hovered();
    let (fill, edge, mark) = if hot { (p.hover, p.dim, p.text) } else { (Color32::TRANSPARENT, p.dim.gamma_multiply(0.6), p.dim) };
    let painter = ui.painter();
    painter.rect(r, 1.0, fill, Stroke::new(1.0 / ppp, edge), egui::StrokeKind::Middle);
    let s = Stroke::new(1.0, mark);
    if plus {
        // On pixel centres, so both bars are one crisp pixel.
        let c = egui::pos2(snap(r.center().x - 0.5), snap(r.center().y - 0.5));
        let d = 3.0;
        painter.line_segment([c - Vec2::new(d, 0.0), c + Vec2::new(d + 1.0 / ppp, 0.0)], s);
        painter.line_segment([c - Vec2::new(0.0, d), c + Vec2::new(0.0, d + 1.0 / ppp)], s);
    } else {
        let (c, d) = (r.center(), 2.5);
        painter.line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], s);
        painter.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], s);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Open the file's folder in Explorer with the file selected.
#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // Explorer parses its own command line: the path must be quoted after
    // the comma (a quoted "/select,..." breaks on commas in names).
    // Windows paths can't contain '"', so this quoting is safe.
    // The new explorer.exe hands the request to the running shell process,
    // which Windows won't let take the foreground from us unless we allow
    // it; without this the window opens behind Totems.
    // SAFETY: plain Win32 call with no pointers.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
            windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
        );
    }
    std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn().map(drop)
}

#[cfg(not(windows))]
fn reveal_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    std::process::Command::new("xdg-open").arg(dir).spawn().map(drop)
}

/// The result row showing `path`, if the current results include it.
enum Reselect {
    /// The selected file is still indexed: its row in these results, if any.
    Same(Option<usize>),
    /// It's gone from the index: the row that now sits where it was.
    Gone(Option<usize>),
}

fn reselect_row(ix: &crate::index::Index, results: &[u32], path: &std::path::Path, old: Option<usize>) -> Reselect {
    match ix.find(path) {
        Some(id) => Reselect::Same(results.iter().position(|&r| r == id as u32)),
        None => Reselect::Gone(old.filter(|_| !results.is_empty()).map(|r| r.min(results.len() - 1))),
    }
}

fn row_of(ix: &crate::index::Index, results: &[u32], path: &std::path::Path) -> Option<usize> {
    let id = ix.find(path)?;
    results.iter().position(|&x| x as usize == id)
}

/// The status bar while a scan runs. The folder count moves even on drives
/// with no audio, so a long scan never looks stuck.
fn indexing_text(ix: &Indexer) -> String {
    format!(
        "Indexing… {} files in {} folders",
        fmt_int(ix.scanned.load(Relaxed) as u64),
        fmt_int(ix.scanned_dirs.load(Relaxed) as u64)
    )
}

/// Width of Ctrl+P's "New…" name box.
const NEW_NAME_W: f32 = 140.0;

/// Most files Ctrl+Shift+P adds at once (an empty search is everything).
const MAX_PLAYLIST_ADD: usize = 100_000;

/// The search for playlists `names`: `pl:A;B `, quoted if a name has a
/// space or search syntax in it.
fn playlist_query(names: &[&str]) -> String {
    let body = names.join(";");
    if body.contains(|c: char| c.is_whitespace() || "|<>!\"".contains(c)) {
        format!("pl:\"{body}\" ")
    } else {
        format!("pl:{body} ")
    }
}

/// Tab after a `pl:` word at the end of the query: fill in the playlists
/// whose names start with what's typed after the last `;` (all of them
/// after a bare `pl:`), skipping ones already named. None: not in a `pl:`
/// word. Err: nothing to fill in.
fn complete_playlists(q: &str, pls: &Playlists) -> Option<Result<String, String>> {
    let lower = q.to_ascii_lowercase();
    let start = lower.rmatch_indices("pl:").map(|(i, _)| i).find(|&i| {
        i == 0 || q[..i].ends_with(|c: char| c.is_whitespace() || matches!(c, '<' | '!' | '|'))
    })?;
    let tail = &q[start + 3..];
    let quoted = tail.starts_with('"');
    let body = tail.trim_start_matches('"');
    let finished = if quoted { body.contains('"') } else { body.contains(char::is_whitespace) };
    if finished {
        return None; // a finished word, not the one being typed
    }
    let mut names: Vec<String> = body.split(';').map(|s| s.trim().to_string()).collect();
    let partial = names.pop().unwrap_or_default();
    names.retain(|n| !n.is_empty());
    let have = |n: &str| names.iter().any(|h| crate::index::lower(h) == crate::index::lower(n));
    let fill: Vec<String> = pls.names_starting(&partial).into_iter().filter(|n| !have(n)).map(String::from).collect();
    if fill.is_empty() {
        return Some(Err(if pls.lists.is_empty() {
            "No playlists yet: Ctrl+P adds the selected file".to_string()
        } else if partial.is_empty() {
            "Every playlist is in already".to_string()
        } else {
            format!("No playlist starts with \"{partial}\"")
        }));
    }
    names.extend(fill);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    Some(Ok(format!("{}{}", &q[..start], playlist_query(&refs))))
}

fn select_all(ctx: &egui::Context, id: egui::Id, text: &str) {
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        let n = text.chars().count();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(n),
        )));
        st.store(ctx, id);
    }
    ctx.memory_mut(|m| m.request_focus(id));
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // The list scrolls in whole rows: take wheel events over it before
        // egui's smooth scrolling sees them, and turn them into row steps.
        if let Some(list) = self.list_rect {
            let pos = raw
                .events
                .iter()
                .rev()
                .find_map(|e| if let Event::PointerMoved(p) = e { Some(*p) } else { None })
                .or_else(|| ctx.input(|i| i.pointer.latest_pos()));
            if pos.is_some_and(|p| wheel_over_list(ctx, list, p)) {
                let is_list_wheel =
                    |e: &Event| matches!(e, Event::MouseWheel { modifiers, .. } if !modifiers.command && !modifiers.ctrl);
                let wheel: Vec<Event> = raw.events.iter().filter(|e| is_list_wheel(e)).cloned().collect();
                if !wheel.is_empty() {
                    raw.events.retain(|e| !is_list_wheel(e));
                    self.list_wheel_rows -= wheel_steps(&wheel, &mut self.list_wheel_acc, WHEEL_ROWS_PER_NOTCH, ROW_H);
                }
            }
        }
        if std::mem::take(&mut self.release_pointer) {
            let pos = raw
                .events
                .iter()
                .rev()
                .find_map(|e| if let Event::PointerMoved(p) = e { Some(*p) } else { None })
                .unwrap_or_default();
            raw.events.push(Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            });
            raw.events.push(Event::PointerGone);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.styled != Some(self.cfg.dark) {
            self.styled = Some(self.cfg.dark);
            self.apply_style(&ctx);
        }
        let p = self.pal();
        let search_id = egui::Id::unique("search");
        self.player.poll();
        self.autoplay_next();

        let focused = ctx.input(|i| i.focused);
        if focused && !self.window_focused {
            // Back from another app: it may have changed the playlists
            // (a DAW, a synced folder).
            self.reload_playlists();
        }
        self.window_focused = focused;
        self.pump_playlists();
        if self.pl_menu.is_some() {
            self.playlist_menu_keys(&ctx);
        } else if keyboard_is_ours(&ctx, search_id) {
            self.handle_keys(&ctx, search_id);
        }
        self.pump_search();

        egui::Panel::top("menu")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(2, 1)))
            .show(ui, |ui| self.menu(ui));
        // [Liam] No egui separator under the search panel: over the headings
        // it doubled their black top line. Grey panel meets grey gutter and
        // scroll column with no line, like the rest of the frame.
        let search_top = egui::Panel::top("search")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.frame_bg).inner_margin(egui::Margin { left: WAVE_PAD as i8, right: LIST_BAR_WIDTH as i8, top: 3, bottom: 4 }))
            .show(ui, |ui| {
                // Pencil-thin black, like every other line, focused or not.
                let border = p.header_border;
                // egui's caret is a full line tall and 1 point wide (a soft
                // ~2 px line on scaled displays). Hide it here and draw a
                // native-looking one below: one physical pixel, text height.
                ui.visuals_mut().text_cursor.stroke = Stroke::NONE;
                // egui would still blink its hidden caret, scheduling its own
                // wake-ups out of phase with ours (up to 4 frames/s idle).
                ui.visuals_mut().text_cursor.blink = false;
                // The box's fill goes in here once its outline is known,
                // under the text: whole pixels, exactly inside the outline.
                // egui's own fill has fractional edges, which at 100 % left
                // a dark row above the top line (it read as a smudge).
                let fill_slot = ui.painter().add(egui::Shape::Noop);
                let out = egui::TextEdit::singleline(&mut self.query)
                        .id(search_id)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(13.5))
                        // Padding goes on the frame: with a custom frame egui
                        // ignores `.margin()`, which left the text and caret
                        // touching the border.
                        .frame(
                            egui::Frame::new()
                                // Room for the outline, which is drawn below.
                                .stroke(Stroke::new(1.0, Color32::TRANSPARENT))
                                .fill(Color32::TRANSPARENT)
                                // Left room for the ">" prompt.
                                .inner_margin(egui::Margin { left: 20, right: 6, top: 3, bottom: 3 }),
                        )
                    .show(ui);
                let resp = out.response.response.clone();
                // Outline: one physical pixel, snapped, its right side on the
                // pixel column of the list's border below (egui's own 1 pt
                // stroke is a 2 px blur ending a pixel short of it), and
                // [Liam] its left side on column L, the gutter's line.
                {
                    let ppp = ui.ctx().pixels_per_point();
                    let (start, end) = (|v: f32| ((v * ppp).round() + 0.5) / ppp, |v: f32| ((v * ppp).round() - 0.5) / ppp);
                    let r = resp.rect;
                    let outline = egui::Rect::from_min_max(egui::pos2(end(r.left()), start(r.top())), egui::pos2(start(r.right()), end(r.bottom())));
                    ui.painter().set(fill_slot, egui::Shape::rect_filled(outline.expand(0.5 / ppp), 0.0, p.search_bg));
                    ui.painter().rect_stroke(outline, 0.0, Stroke::new(1.0 / ppp, border), egui::StrokeKind::Middle);
                }
                self.paint_caret(ui, &out, p.text);
                // A static blue ">" prompt at the start of the box.
                ui.painter().text(
                    egui::pos2(resp.rect.left() + 7.0, resp.rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    ">",
                    FontId::proportional(13.5),
                    p.header_text,
                );
                if resp.clicked() || resp.gained_focus() {
                    self.list_focus = false;
                }
                // The search box owns the keyboard, like a launcher: take focus
                // back whenever nothing else (a dialog, a menu) wants it.
                let window_focused = ctx.input(|i| i.focused);
                let free = ctx.memory(|m| m.focused().is_none());
                if window_focused
                    && free
                    && !self.show_folders
                    && !self.show_bookmarks
                    && !self.show_playlists
                    && self.pl_menu.is_none()
                    && !egui::Popup::is_any_open(&ctx)
                {
                    resp.request_focus();
                }
            })
            .response
            .rect
            .top();
        // [Liam] A crisp edge between the menu bar and the grey search panel:
        // the panel's fill starts a fraction of a pixel into a row, which
        // blended it. Repaint the rows either side of the nearest boundary.
        {
            let ppp = ctx.pixels_per_point();
            let edge = (search_top * ppp).round();
            let x = ctx.content_rect().x_range();
            let rows = |a: f32, b: f32| egui::Rect::from_x_y_ranges(x, (a / ppp)..=(b / ppp));
            ui.painter().rect_filled(rows(edge - 2.0, edge), 0.0, p.bg);
            ui.painter().rect_filled(rows(edge, edge + 2.0), 0.0, p.frame_bg);
        }
        // [Liam] No egui separator above the status bar: it ran across the
        // waveform's grey pads. The waveform's own bottom line (`wave_frame`)
        // is the only line there.
        egui::Panel::bottom("status")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.frame_bg).inner_margin(egui::Margin::symmetric(0, 3)))
            .show(ui, |ui| self.status(ui));
        // The waveform's height is dragged at its top edge. egui would draw
        // that edge itself, unsnapped and thicker when hovered: switch its
        // lines off for the panel; `wave_frame` draws a black one instead
        // (hovering only changes the cursor).
        let saved = ui.style().clone();
        let w = &mut ui.style_mut().visuals.widgets;
        (w.noninteractive.bg_stroke, w.hovered.fg_stroke, w.active.fg_stroke) = (Stroke::NONE, Stroke::NONE, Stroke::NONE);
        let wave = egui::Panel::bottom("wave")
            .resizable(true)
            .show_separator_line(false)
            .default_size(WAVE_H)
            .size_range(WAVE_H_MIN..=WAVE_H_MAX)
            .frame(egui::Frame::new().fill(p.bg))
            .show(ui, |ui| self.waveform(ui));
        ui.set_style(saved);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.list_bg)).show(ui, |ui| self.table(ui));
        self.wave_frame(ui, wave.response.rect);

        if self.show_folders {
            self.folders_window(&ctx);
        }
        if self.show_bookmarks {
            self.bookmarks_window(&ctx);
        }
        if self.show_playlists {
            self.playlists_window(&ctx);
        }
        if self.pl_menu.is_some() {
            self.playlist_menu(&ctx);
        }
        if self.show_help {
            self.help_window(&ctx);
        }

        if let Some(d) = self.drag_out.take() {
            let file = match d {
                DragOut::File(p) => Ok(p),
                DragOut::Region(p, a, b) => crate::region::export(&p, a, b),
            };
            match file {
                Ok(f) => start_os_drag(frame, f),
                Err(e) => self.notice = Some(Notice::error(format!("Couldn't export selection: {e}"))),
            }
            self.release_pointer = true;
            ctx.request_repaint();
        }

        let title = if self.query.is_empty() { APP_NAME.to_string() } else { format!("{} - {APP_NAME}", self.query) };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
        if self.player.state() == State::Playing {
            // Redraw as often as the playhead moves a whole pixel, 10–60 Hz.
            let width = ctx.content_rect().width().max(100.0) as f64;
            let dur = self.player.info().and_then(|i| i.duration).unwrap_or(1.0);
            let mut ms = (dur / width * 1000.0).clamp(16.0, 100.0);
            // The audio callback never wakes the UI, so wake when the sound
            // ends (+5 ms for the callback to post its End).
            // Never faster than 60 Hz: a sound that outlasts its header's
            // length (chained Ogg, MP3 without a Xing header) has 0 s left
            // until its real end, which made this 5 ms (200 Hz).
            if let Some(left) = self.player.time_left() {
                ms = ms.min(left * 1000.0 + 5.0).max(16.0);
            }
            ctx.request_repaint_after(Duration::from_millis(ms as u64));
        } else if self.indexer.scanning.load(Relaxed) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "config", &self.cfg);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::region::cleanup(crate::region::GRACE);
    }
}

// ------------------------------------------------------------- formatting

pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_size(b: u64) -> String {
    format!("{} KB", fmt_int(b.div_ceil(1024)))
}

/// Local date and time to the minute. Converting to local time asks the OS
/// for the time zone (~0.8 µs, for every visible row every frame), so
/// minutes already shown are remembered.
fn fmt_date(t: i64) -> String {
    thread_local! {
        static SEEN: std::cell::RefCell<std::collections::HashMap<i64, String>> = Default::default();
    }
    let minute = t.div_euclid(60);
    SEEN.with_borrow_mut(|seen| {
        if seen.len() > 4096 {
            seen.clear();
        }
        seen.entry(minute).or_insert_with(|| fmt_date_uncached(minute * 60)).clone()
    })
}

fn fmt_date_uncached(t: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(t, 0) {
        chrono::LocalResult::Single(d) | chrono::LocalResult::Ambiguous(d, _) => d.format("%Y-%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

fn fmt_len(secs: f64) -> String {
    let ms = (secs * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}.{:03}", ms) }
}

fn fmt_time(secs: f64) -> String {
    fmt_len(secs.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting() {
        assert_eq!(fmt_int(1234567), "1,234,567");
        assert_eq!(fmt_int(12), "12");
        assert_eq!(fmt_size(1), "1 KB");
        assert_eq!(fmt_size(2_000_000), "1,954 KB");
        assert_eq!(fmt_len(4.5123), "0:04.512");
        assert_eq!(fmt_len(3725.0), "1:02:05");
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// Run one egui frame with `events` and return `consume_seek`'s result.
    fn seek_for(events: Vec<Event>) -> Option<f64> {
        let ctx = egui::Context::default();
        let mut got = None;
        let raw = egui::RawInput { events, ..Default::default() };
        let _ = ctx.run_ui(raw, |ui| got = consume_seek(ui.ctx()));
        got
    }

    #[test]
    fn shift_arrow_seeks_five_seconds() {
        assert_eq!(seek_for(vec![key(Key::ArrowRight, Modifiers::SHIFT)]), Some(5.0));
        assert_eq!(seek_for(vec![key(Key::ArrowLeft, Modifiers::SHIFT)]), Some(-5.0));
        assert_eq!(seek_for(vec![key(Key::ArrowRight, Modifiers::NONE)]), Some(1.0));
        assert_eq!(seek_for(vec![key(Key::ArrowLeft, Modifiers::NONE)]), Some(-1.0));
        assert_eq!(seek_for(vec![]), None);
    }

    fn wheel(unit: egui::MouseWheelUnit, y: f32) -> Event {
        Event::MouseWheel {
            unit,
            delta: Vec2::new(0.0, y),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn fader_law_round_trips_and_rises() {
        assert_eq!((fader_db(0.0), fader_db(1.0)), (MIN_DB, MAX_DB));
        assert_eq!(fader_db(0.7), 0.0);
        assert_eq!((fader_db(f32::NAN), fader_pos(f32::NAN)), (MIN_DB, 0.7));
        let mut last = f32::NEG_INFINITY;
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let db = fader_db(t);
            assert!(db >= last, "rises at {t}");
            assert!((fader_pos(db) - t).abs() < 1e-5, "inverse at {t}");
            last = db;
        }
    }

    #[test]
    fn one_wheel_notch_is_one_volume_step() {
        use egui::MouseWheelUnit::*;
        let mut acc = 0.0;
        assert_eq!(wheel_steps(&[wheel(Line, 1.0)], &mut acc, 1.0, WHEEL_POINTS_PER_STEP), 1);
        assert_eq!(wheel_steps(&[], &mut acc, 1.0, WHEEL_POINTS_PER_STEP), 0, "a notch must not keep stepping on later frames");
        assert_eq!(wheel_steps(&[wheel(Line, -1.0)], &mut acc, 1.0, WHEEL_POINTS_PER_STEP), -1);
        // Trackpad: small point deltas add up to one step.
        let quarter = WHEEL_POINTS_PER_STEP / 4.0;
        assert_eq!(wheel_steps(&[wheel(Point, quarter), wheel(Point, quarter)], &mut acc, 1.0, WHEEL_POINTS_PER_STEP), 0);
        assert_eq!(wheel_steps(&[wheel(Point, quarter), wheel(Point, quarter)], &mut acc, 1.0, WHEEL_POINTS_PER_STEP), 1);
        assert!(acc.abs() < 1e-6);
    }

    #[test]
    fn row_of_finds_the_playing_file_in_results() {
        use crate::index::{Index, RawEntry};
        let e = |name: &str| RawEntry { dir: 0, name: name.into(), size: 1, mtime: 1 };
        let ix = Index::build(vec!["/s".into()], vec![e("a.wav"), e("b.wav"), e("c.wav")]);
        let p = |n: &str| std::path::Path::new("/s").join(n);
        // Results in a different order, without a.wav.
        let results = [2u32, 1];
        assert_eq!(row_of(&ix, &results, &p("b.wav")), Some(1));
        assert_eq!(row_of(&ix, &results, &p("c.wav")), Some(0));
        assert_eq!(row_of(&ix, &results, &p("a.wav")), None, "filtered out");
        assert_eq!(row_of(&ix, &results, &p("zzz.wav")), None, "not indexed");
    }

    #[test]
    fn index_refresh_does_not_scroll() {
        let mut s = ScrollIntent::default();
        s.sent(1, true); // first query
        assert!(s.arrived(1));
        s.sent(2, false); // file changed on disk
        assert!(!s.arrived(2));
        // A user change coalesced into a later refresh still scrolls once.
        s.sent(3, true);
        s.sent(4, false);
        assert!(s.arrived(4));
        assert!(!s.arrived(5));
    }

    /// B2: with the Folders path box focused, Enter must add the folder and
    /// Space must type, not play.
    #[test]
    fn shortcuts_leave_other_text_fields_alone() {
        let ctx = egui::Context::default();
        let (search, other) = (egui::Id::unique("search"), egui::Id::unique("folder-path"));
        let mut text = String::new();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(other)).request_focus();
        });
        let raw = egui::RawInput { events: vec![Event::Text(" ".into()), key(Key::Enter, Modifiers::NONE)], ..Default::default() };
        let mut lost = false;
        let _ = ctx.run_ui(raw, |ui| {
            if keyboard_is_ours(ui.ctx(), search) {
                // What handle_keys does with these keys.
                ui.ctx().input_mut(|i| {
                    i.consume_key(Modifiers::NONE, Key::Enter);
                    i.events.retain(|e| !matches!(e, Event::Text(t) if t == " "));
                });
            }
            lost = ui.add(egui::TextEdit::singleline(&mut text).id(other)).lost_focus();
        });
        assert_eq!(text, " ", "Space must type into the path box");
        assert!(lost, "Enter must reach the path box");
        ctx.memory_mut(|m| m.request_focus(search));
        assert!(keyboard_is_ours(&ctx, search), "the search box doesn't block shortcuts");
    }

    /// B3: the wheel over a window floating above the list (Folders, Help)
    /// scrolls that window, not the list underneath.
    #[test]
    fn wheel_over_a_window_is_not_the_lists() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(800.0, 600.0));
        for _ in 0..2 {
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let _ = ctx.run_ui(raw, |ui| {
                egui::Window::new("Folders").fixed_pos(egui::pos2(100.0, 100.0)).fixed_size(Vec2::splat(200.0)).show(ui.ctx(), |ui| {
                    ui.label("x");
                });
            });
        }
        assert!(!wheel_over_list(&ctx, screen, egui::pos2(150.0, 150.0)), "inside the window");
        assert!(wheel_over_list(&ctx, screen, egui::pos2(600.0, 500.0)), "plain list");
        assert!(!wheel_over_list(&ctx, screen, egui::pos2(900.0, 500.0)), "outside the list");
    }

    /// B4: past 16.7M points an f32 can't hold every whole number, so with
    /// more than ~880k results `top * ROW_H` can come back 1 point short and
    /// the list snapped up a row (hiding the selected row at the bottom).
    #[test]
    fn huge_lists_keep_their_top_row() {
        let view = 30.0 * ROW_H;
        let total = 1_000_001;
        let mut bad = Vec::new();
        for top in 880_000..999_000 {
            let offset = top as f32 * ROW_H; // what the scroll area stores
            if list_top_row(offset, view, total, 0, None) != top || !on_row_grid(offset) {
                bad.push(top);
            }
        }
        assert!(bad.is_empty(), "{} rows drift, e.g. {:?}", bad.len(), &bad[..bad.len().min(5)]);
    }

    #[test]
    fn list_scrolls_in_whole_rows() {
        let h = ROW_H;
        let view = 10.0 * h + 10.0; // 10 full rows and part of an 11th
        let total = 101; // 100 results + the spacer row
        assert_eq!(list_top_row(0.0, view, total, 0, None), 0);
        assert_eq!(list_top_row(3.5 * h, view, total, 0, None), 3, "a part-row at the top is dropped");
        assert_eq!(list_top_row(0.0, view, total, 3, None), 3, "one wheel notch down");
        assert_eq!(list_top_row(1.0 * h, view, total, -3, None), 0, "can't scroll above the top");
        // Keyboard: the row ends up fully visible, moving as little as possible.
        assert_eq!(list_top_row(0.0, view, total, 0, Some(15)), 6);
        assert_eq!(list_top_row(5.0 * h, view, total, 0, Some(2)), 2);
        assert_eq!(list_top_row(5.0 * h, view, total, 0, Some(9)), 5, "already visible: no move");
        // The end: the last result (99) is fully visible and the top is still a whole row.
        let end = list_top_row(0.0, view, total, 1_000, None);
        assert_eq!(end, 90);
        assert!(end + 10 > 99);
        assert_eq!(list_top_row(0.0, view, total, 0, Some(99)), 90);
        // 10.2 rows of view: max offset is 90.8 rows, so the limit is 90,
        // never rounded up to 91 (egui would clamp that off the row grid).
        assert_eq!(list_geom(10.2 * h, total).1, 90);
        // Fewer results than fit: never scrolls.
        assert_eq!(list_top_row(0.0, view, 4, 3, None), 0);
        // A wheel notch over the list is 3 rows; trackpad points add up per row.
        let mut acc = 0.0;
        assert_eq!(wheel_steps(&[wheel(egui::MouseWheelUnit::Line, -1.0)], &mut acc, WHEEL_ROWS_PER_NOTCH, ROW_H), -3);
        assert_eq!(wheel_steps(&[wheel(egui::MouseWheelUnit::Point, -ROW_H)], &mut acc, WHEEL_ROWS_PER_NOTCH, ROW_H), -1);
    }

    #[test]
    fn scroll_bar_maps_to_whole_rows() {
        let (track_top, track_h) = (100.0, 400.0);
        let (total, view_rows) = (1_000_001, 30); // a million results + spacer
        let max_top = total - view_rows;
        let (y0, len) = bar_handle(track_top, track_h, 0, max_top, view_rows, total);
        assert_eq!(y0, track_top);
        assert_eq!(len, SCROLL_HANDLE_MIN, "huge lists get the minimum, not a sliver");
        let (y_end, _) = bar_handle(track_top, track_h, max_top, max_top, view_rows, total);
        assert_eq!(y_end + len, track_top + track_h, "bottom of the track = last page");
        // Dragging the handle to where a row puts it gives that row back.
        for top in [0, 1, 12_345, max_top / 2, max_top] {
            let (y, len) = bar_handle(track_top, track_h, top, max_top, view_rows, total);
            let back = bar_top_at(y, track_top, track_h, len, max_top);
            assert!(back.abs_diff(top) <= max_top / 384 + 1, "{top} -> {back}");
        }
        assert_eq!(bar_top_at(-50.0, track_top, track_h, len, max_top), 0);
        assert_eq!(bar_top_at(10_000.0, track_top, track_h, len, max_top), max_top);
        // Short list: the handle fills the track, and nothing moves.
        let (y, len) = bar_handle(track_top, track_h, 0, 0, 30, 10);
        assert_eq!((y, len), (track_top, track_h));
    }

    /// ▶ ‖ ■ are one family: the same height at every scale, the square a
    /// little smaller, bars and square on whole pixels.
    #[test]
    fn transport_icons_match() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let h = (TRANSPORT_H * ppp).round();
            let span = |icon| {
                let pts: Vec<_> = transport_shapes(icon, ppp).into_iter().flatten().collect();
                let ys = pts.iter().map(|q| q.y);
                (ys.clone().fold(f32::MAX, f32::min), ys.fold(f32::MIN, f32::max), pts)
            };
            for icon in [Transport::Play, Transport::Pause] {
                let (top, bottom, _) = span(icon);
                assert_eq!((top, bottom), (-h / 2.0, h / 2.0), "{icon:?} at {ppp}");
            }
            let (top, bottom, pts) = span(Transport::Stop);
            assert!(bottom - top < h && bottom - top >= h * 0.8, "stop at {ppp}");
            for icon in [Transport::Pause, Transport::Stop] {
                for q in transport_shapes(icon, ppp).into_iter().flatten() {
                    assert_eq!(q.x, q.x.round(), "{icon:?} x on a pixel at {ppp}");
                    assert_eq!(q.y + h / 2.0, (q.y + h / 2.0).round(), "{icon:?} y on a pixel at {ppp}");
                }
            }
            let _ = pts;
        }
    }

    /// Bookmarks take the lowest free key 1–10 (Ctrl+0 is 10); an 11th
    /// gets none; removing one frees its key for the next.
    #[test]
    fn bookmarks_auto_assign_keys() {
        let mut m = Vec::new();
        for i in 0..11 {
            assert!(add_bookmark(&mut m, std::path::Path::new(&format!("C:\\b{i}"))));
        }
        let keys: Vec<_> = m.iter().map(|b| b.key).collect();
        assert_eq!(&keys[..10], &(1..=10).map(Some).collect::<Vec<_>>()[..]);
        assert_eq!(keys[10], None, "only ten keys");
        assert!(!add_bookmark(&mut m, std::path::Path::new("c:\\B3\\")), "the same folder twice");
        m.remove(2); // key 3
        assert!(add_bookmark(&mut m, std::path::Path::new("C:\\new")));
        assert_eq!(m.last().unwrap().key, Some(3));
        assert_eq!(key_label(10), "0");
    }

    /// Giving a bookmark a key takes it from whichever bookmark had it.
    #[test]
    fn a_key_belongs_to_one_bookmark() {
        let mut m = Vec::new();
        add_bookmark(&mut m, std::path::Path::new("C:\\a"));
        add_bookmark(&mut m, std::path::Path::new("C:\\b"));
        assign_key(&mut m, 1, Some(1));
        assert_eq!((m[0].key, m[1].key), (None, Some(1)));
        assign_key(&mut m, 1, None);
        assert_eq!(m[1].key, None);
    }

    /// A bookmark's search finds the folder's files, its subfolders' too,
    /// not a sibling with the same start, and narrows with words typed after.
    #[cfg(windows)]
    #[test]
    fn bookmark_search_finds_the_folder() {
        use crate::index::{Index, RawEntry};
        use crate::query::{Query, search};
        let dirs = vec![r"F:\SFX\Foley".to_string(), r"F:\SFX\Foley\Doors".to_string(), r"F:\SFX\Foley 2".to_string()];
        let e = |dir, name: &str| RawEntry { dir, name: name.into(), size: 1, mtime: 0 };
        let ix = Index::build(dirs, vec![e(0, "step.wav"), e(1, "door slam.wav"), e(2, "door.wav")]);
        let hits = |q: &str| {
            let q = Query::parse(q, &Options::default()).unwrap();
            let mut v: Vec<_> = search(&ix, &q, SortKey::Name, true).iter().map(|&i| ix.full_path(i as usize)).collect();
            v.sort();
            v
        };
        let q = bookmark_query(std::path::Path::new(r"F:\SFX\Foley"));
        assert_eq!(q, "\"F:\\SFX\\Foley\\\" ");
        assert_eq!(hits(&q), vec![PathBuf::from(r"F:\SFX\Foley\Doors\door slam.wav"), PathBuf::from(r"F:\SFX\Foley\step.wav")]);
        assert_eq!(hits(&format!("{q}door")), vec![PathBuf::from(r"F:\SFX\Foley\Doors\door slam.wav")]);
        assert_eq!(bookmark_query(std::path::Path::new("F:\\")), "\"F:\\\" ", "a drive keeps one separator");
    }

    /// Tab after `pl:` fills in playlist names: all of them, or the ones
    /// starting with what's typed after the last `;`.
    #[test]
    fn tab_completes_playlist_names() {
        let pl = |n: &str| Playlist { name: n.into(), ..Default::default() };
        let pls = Playlists { dir: PathBuf::new(), lists: vec![pl("Impacts"), pl("Ivory Keys"), pl("Whooshes")] };
        let done = |q: &str| complete_playlists(q, &pls).map(|r| r.unwrap_or_else(|e| format!("ERR {e}")));
        assert_eq!(done("pl:").as_deref(), Some(r#"pl:"Impacts;Ivory Keys;Whooshes" "#));
        assert_eq!(done("door pl:wh").as_deref(), Some("door pl:Whooshes "));
        assert_eq!(done("pl:Whooshes;im").as_deref(), Some("pl:Whooshes;Impacts "));
        assert_eq!(done("PL:i").as_deref(), Some(r#"pl:"Impacts;Ivory Keys" "#));
        assert!(done("pl:zz").unwrap().starts_with("ERR"));
        assert_eq!(done("door"), None);
        assert_eq!(done("pl:Impacts door"), None, "not the word being typed");
        assert_eq!(done(r#"pl:"Ivory Keys" "#), None);
        // What it fills in searches those playlists.
        let q = Query::parse(r#"pl:"Impacts;Ivory Keys" "#, &Options::default()).unwrap();
        assert!(q.has_playlist());
    }

    #[test]
    fn autoplay_next_moves_on_one_row() {
        assert_eq!(next_row(Some(3), false, 10), Some(4));
        assert_eq!(next_row(Some(9), false, 10), None, "the last row is the end");
        assert_eq!(next_row(Some(3), true, 10), None, "an auditioned selection doesn't move on");
        assert_eq!(next_row(None, false, 10), None, "the file isn't in these results");
    }

    #[test]
    fn only_the_selection_is_coloured() {
        for p in [LIGHT, DARK, DARK_CLASSIC] {
            let sel = Some((100.0, 200.0));
            let play = Some(150.0);
            assert_eq!(p.wave_color(50.0, play, sel), p.wave_out, "played but outside: grey");
            assert_eq!(p.wave_color(250.0, play, sel), p.wave_out, "unplayed, outside: grey");
            // [Liam] Inside the selection: lighter blue, played or not.
            assert_eq!(p.wave_color(120.0, play, sel), p.wave_sel, "inside, played: never grey");
            assert_eq!(p.wave_color(180.0, play, sel), p.wave_sel, "inside, not yet played");
            assert_eq!(p.wave_color(180.0, None, sel), p.wave_sel, "inside, stopped");
            assert_ne!(p.wave_sel, p.accent);
            // No selection: played grey, not yet played blue.
            assert_eq!(p.wave_color(50.0, play, None), p.wave_played);
            assert_eq!(p.wave_color(250.0, play, None), p.accent);
            assert_eq!(p.wave_color(50.0, None, None), p.accent, "stopped: all blue");
        }
    }

    #[test]
    fn ctrl_down_skips_to_next_different_name() {
        let names = ["bb", "bb", "bb", "cc", "dd", "dd"];
        let key = |i: usize| names[i];
        assert_eq!(next_different(names.len(), 0, key), 3, "skips the run of bb");
        assert_eq!(next_different(names.len(), 1, key), 3, "from inside the run");
        assert_eq!(next_different(names.len(), 3, key), 4);
        assert_eq!(next_different(names.len(), 4, key), 5, "last run: go to the last row");
        assert_eq!(next_different(names.len(), 5, key), 5, "already last");
        assert_eq!(next_different(1, 0, key), 0);
    }

    #[test]
    fn double_click_does_not_restart_what_its_first_click_started() {
        let mut c = ClickPlay::default();
        c.single(3, true); // first click started row 3
        assert!(!c.double(3));
        c.single(3, false); // row 3 was already playing; click changed nothing
        assert!(c.double(3), "double-click on the playing row restarts it");
        c.single(3, true);
        assert!(c.double(4), "different row");
    }

    /// B36: after the selected file was deleted, the next arrow press jumped
    /// to row 0 (and Space still played the deleted file).
    #[test]
    fn a_vanished_selection_passes_to_its_neighbour() {
        use crate::index::RawEntry;
        let e = |n: String| RawEntry { dir: 0, name: n, size: 1, mtime: 1 };
        let all: Vec<RawEntry> = (0..100).map(|i| e(format!("f{i:03}.wav"))).collect();
        let before = Index::build(vec!["/s".into()], all.clone());
        let gone = before.full_path(60);
        let after = Index::build(vec!["/s".into()], all.into_iter().filter(|x| x.name != "f060.wav").collect());
        let results: Vec<u32> = (0..99).collect();
        assert!(matches!(reselect_row(&after, &results, &gone, Some(60)), Reselect::Gone(Some(60))));
        assert!(matches!(reselect_row(&after, &results[..10], &gone, Some(60)), Reselect::Gone(Some(9))));
        assert!(matches!(reselect_row(&after, &[], &gone, Some(60)), Reselect::Gone(None)));
        // Still indexed but filtered out by the query: no row, kept for later.
        let kept = after.full_path(5);
        assert!(matches!(reselect_row(&after, &[1, 2], &kept, Some(5)), Reselect::Same(None)));
        assert!(matches!(reselect_row(&after, &results, &kept, Some(5)), Reselect::Same(Some(5))));
    }

    struct Mem(std::collections::HashMap<String, String>);
    impl eframe::Storage for Mem {
        fn get_string(&self, k: &str) -> Option<String> {
            self.0.get(k).cloned()
        }
        fn set_string(&mut self, k: &str, v: String) {
            self.0.insert(k.into(), v);
        }
        fn remove_string(&mut self, k: &str) {
            self.0.remove(k);
        }
        fn flush(&mut self) {}
    }

    /// B41: one unreadable field (another version's sort key, a missing
    /// option, a wrong type) lost the whole config, folders included; B42:
    /// a NaN or huge volume was loaded as is.
    #[test]
    fn bad_settings_fields_fall_back_alone() {
        let c = Config { roots: vec![PathBuf::from("F:/Samples")], volume_db: -6.0, dark: true, ..Config::without_roots() };
        let mut m = Mem(Default::default());
        eframe::set_value(&mut m, "config", &c);
        let saved = m.0["config"].clone();
        let load = |text: String| load_config(&text);
        let same = load(saved.clone());
        assert_eq!((same.roots, same.volume_db, same.dark), (c.roots.clone(), -6.0, true), "round trip");
        for (from, to) in [("sort:Name", "sort:Length"), ("match_case:false,", ""), ("dark:true", "dark:1")] {
            assert!(saved.contains(from), "{from} in {saved}");
            let got = load(saved.replace(from, to));
            assert_eq!(got.roots, c.roots, "{to}: folders kept");
            assert_eq!(got.volume_db, -6.0, "{to}: other fields kept");
        }
        for (v, want) in [("NaN", 0.0), ("1000.0", MAX_DB), ("-1000.0", MIN_DB)] {
            let got = load(saved.replace("volume_db:-6.0", &format!("volume_db:{v}")));
            assert_eq!(got.volume_db, want, "{v}");
        }
    }

    #[test]
    fn cached_dates_match() {
        for t in [0i64, 59, 60, 1_700_000_000, 1_700_000_059, -1, -61, 1_711_846_800, 1_711_850_399, i64::MAX / 2] {
            assert_eq!(fmt_date(t), fmt_date_uncached(t), "{t}");
            assert_eq!(fmt_date(t), fmt_date(t));
        }
    }
}
