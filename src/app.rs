//! The window: one search box, one result list, a waveform strip and a
//! status bar.

use crate::decode::Info;
use crate::index::{Index, default_roots};
use crate::indexer::Indexer;
use crate::player::{MAX_DB, MIN_DB, Player, State};
use crate::query::{Options, Query, SortKey, search};
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: default_roots(),
            opts: Options::default(),
            sort: SortKey::Name,
            ascending: true,
            dark: false,
            volume_db: 0.0,
            soft_clip: true,
            autoplay: true,
        }
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

/// Space left of the waveform, so its very start is easy to click: a click
/// in the gap lands on 0:00.
const WAVE_PAD_LEFT: f32 = 8.0;

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
}

struct SearchRes {
    generation: u64,
    ids: Arc<Vec<u32>>,
    index: Arc<Index>,
    error: Option<String>,
    highlights: Vec<String>,
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
                    Ok(q) => SearchRes {
                        generation: req.generation,
                        ids: Arc::new(search(&req.index, &q, req.sort, req.ascending)),
                        index: req.index,
                        error: None,
                        highlights: q.highlights,
                    },
                    Err(e) => SearchRes {
                        generation: req.generation,
                        ids: Arc::new(Vec::new()),
                        index: req.index,
                        error: Some(e),
                        highlights: vec![],
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
    text: Color32,
    dim: Color32,
    header_text: Color32,
    grid: Color32,
    hover: Color32,
    sel: Color32,
    sel_inactive: Color32,
    sel_border: Color32,
    status_bg: Color32,
    wave: Color32,
    wave_played: Color32,
    /// With a waveform selection: the not-yet-played part inside it, and
    /// everything outside it (greyed out).
    wave_sel: Color32,
    wave_out: Color32,
    /// Selection edge crowns: fill and outline (the outline keeps the white
    /// crown visible on the light theme's white background).
    crown: Color32,
    crown_edge: Color32,
    playhead: Color32,
    playhead_crown: Color32,
    /// The gap left of the waveform: darker than the background so it reads
    /// as padding.
    wave_pad: Color32,
    /// Pencil-thin outline around each column heading.
    header_border: Color32,
    /// The list's scroll bar: track, handle, handle under the pointer. The
    /// handle's outline is `header_text` blue.
    scroll_track: Color32,
    scroll_handle: Color32,
    scroll_handle_hover: Color32,
}

impl Palette {
    /// Colour of the waveform column centred at `x`. `play_x` is the
    /// playhead (None when not playing); `sel` the selection's x range.
    /// With a selection, only the part inside it is coloured.
    fn wave_color(&self, x: f32, play_x: Option<f32>, sel: Option<(f32, f32)>) -> Color32 {
        let played = play_x.is_some_and(|p| x <= p);
        match sel {
            Some((a, b)) if x < a || x > b => self.wave_out,
            Some(_) if played => self.wave_played,
            Some(_) => self.wave_sel,
            None if played => self.wave_played,
            None => self.wave,
        }
    }
}

const LIGHT: Palette = Palette {
    bg: Color32::WHITE,
    text: Color32::from_rgb(0x1E, 0x1E, 0x1E),
    dim: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    header_text: Color32::from_rgb(0x00, 0x5F, 0xB8), // 6.3:1 on white
    grid: Color32::from_rgb(0xE5, 0xE5, 0xE5),
    hover: Color32::from_rgb(0xE5, 0xF3, 0xFF),
    sel: Color32::from_rgb(0xCC, 0xE8, 0xFF),
    sel_inactive: Color32::from_rgb(0xD9, 0xD9, 0xD9),
    sel_border: Color32::from_rgb(0x99, 0xD1, 0xFF),
    status_bg: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    wave: Color32::from_rgb(0xA8, 0xB4, 0xC2),
    wave_played: Color32::from_rgb(0x00, 0x78, 0xD7),
    wave_sel: Color32::from_rgb(0x7F, 0xB6, 0xE6),
    wave_out: Color32::from_rgb(0xD6, 0xDA, 0xDF),
    crown: Color32::WHITE,
    crown_edge: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    // White would vanish on the light theme's white strip.
    playhead: Color32::from_rgb(0x1E, 0x1E, 0x1E),
    playhead_crown: Color32::from_rgb(0xF2, 0x7A, 0x00),
    wave_pad: Color32::from_rgb(0xC8, 0xC8, 0xC8),
    header_border: Color32::from_rgb(0xA0, 0xA0, 0xA0),
    scroll_track: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    scroll_handle: Color32::from_rgb(0xC2, 0xC2, 0xC2),
    scroll_handle_hover: Color32::from_rgb(0xA8, 0xA8, 0xA8),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1F, 0x1F, 0x1F),
    text: Color32::from_rgb(0xE6, 0xE6, 0xE6),
    dim: Color32::from_rgb(0x9A, 0x9A, 0x9A),
    header_text: Color32::from_rgb(0x4C, 0xA6, 0xF2), // 6.3:1 on #1F1F1F
    grid: Color32::from_rgb(0x33, 0x33, 0x33),
    hover: Color32::from_rgb(0x2A, 0x33, 0x3D),
    sel: Color32::from_rgb(0x1F, 0x4E, 0x79),
    sel_inactive: Color32::from_rgb(0x3A, 0x3A, 0x3A),
    sel_border: Color32::from_rgb(0x2F, 0x6E, 0xA8),
    status_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    wave: Color32::from_rgb(0x55, 0x60, 0x6C),
    wave_played: Color32::from_rgb(0x3A, 0x9B, 0xEF),
    wave_sel: Color32::from_rgb(0x2E, 0x66, 0x9A),
    wave_out: Color32::from_rgb(0x3A, 0x3E, 0x43),
    crown: Color32::WHITE,
    crown_edge: Color32::TRANSPARENT,
    playhead: Color32::WHITE,
    playhead_crown: Color32::from_rgb(0xFF, 0x8C, 0x1A),
    wave_pad: Color32::from_rgb(0x3C, 0x3C, 0x3C),
    header_border: Color32::BLACK,
    scroll_track: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    scroll_handle: Color32::from_rgb(0x9E, 0x9E, 0x9E),
    scroll_handle_hover: Color32::from_rgb(0xBE, 0xBE, 0xBE),
};

pub struct App {
    cfg: Config,
    indexer: Arc<Indexer>,
    seen_version: u64,
    query: String,
    search_tx: crossbeam_channel::Sender<SearchReq>,
    search_rx: crossbeam_channel::Receiver<SearchRes>,
    sent: Option<(String, Options, SortKey, bool, u64)>,
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
    status_shown: Option<(String, Color32)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let cfg: Config = cc.storage.and_then(|s| eframe::get_value(s, "config")).unwrap_or_default();
        let has_bold = crate::fonts::install(&cc.egui_ctx);
        // Clear selection exports left by the last run (or a crash).
        std::thread::spawn(|| crate::region::cleanup(crate::region::GRACE));
        let indexer = Indexer::start(cfg.roots.clone(), repaint.clone());
        let (search_tx, search_rx) = spawn_searcher(repaint.clone());
        let player = Player::new(repaint.clone());
        player.set_volume_db(cfg.volume_db);
        player.set_soft_clip(cfg.soft_clip);
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 4);
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
            show_help: false,
            help_anchor: egui::Pos2::ZERO,
            help_opened: 0,
            has_bold,
            last_title: String::new(),
            styled: None,
            drag_out: None,
            wsel: None,
            wave_drag: None,
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
            cfg,
        }
    }

    fn pal(&self) -> Palette {
        if self.cfg.dark { DARK } else { LIGHT }
    }

    // -------------------------------------------------------------- search

    fn pump_search(&mut self) {
        let v = self.indexer.version.load(SeqCst);
        if v != self.seen_version {
            self.seen_version = v;
        }
        let Some(index) = self.indexer.index() else { return };
        let key = (self.query.clone(), self.cfg.opts, self.cfg.sort, self.cfg.ascending, v);
        if self.sent.as_ref() != Some(&key) {
            // Only the index version changed: a background refresh.
            let user_change = self
                .sent
                .as_ref()
                .is_none_or(|s| (&s.0, s.1, s.2, s.3) != (&key.0, key.1, key.2, key.3));
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

    // ------------------------------------------------------------ keyboard

    fn handle_keys(&mut self, ctx: &egui::Context, search_id: egui::Id) {
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
            });
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.cfg.dark, "Dark").clicked() {
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
                TextFormat { font_id: bold.clone(), color: if self.has_bold { color.lerp_to_gamma(p.bg, 0.12) } else { p.wave_played }, ..Default::default() }
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

        // Our scroll bar sits right of the table, beside the body rows.
        // egui's own bar is hidden: it can't be outlined, and ours moves in
        // whole rows by construction.
        let area = ui.available_rect_before_wrap();
        let strip = SCROLL_BAR_WIDTH + 4.0;
        let table_rect = egui::Rect::from_min_max(area.min, egui::pos2(area.right() - strip, area.bottom()));
        let track = self.list_rect.map(|b| {
            egui::Rect::from_min_max(egui::pos2(area.right() - strip + 2.0, b.top()), egui::pos2(area.right() - 2.0, b.bottom()))
        });
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
                    }
                    // The ▶ marker is painted into the left padding, so it
                    // never pushes the name sideways.
                    if is_playing {
                        let at = egui::pos2(ui.max_rect().left() + 4.0, ui.max_rect().center().y);
                        ui.painter().text(at, egui::Align2::LEFT_CENTER, "▶", FontId::proportional(9.0), p.wave_played);
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
        if !on_row_grid(self.list_offset) {
            ui.ctx().request_repaint(); // snap to a whole row next frame
        }
        if let Some(track) = track {
            // Light grey handle with a pencil-thin blue outline.
            let painter = ui.painter();
            painter.rect_filled(track, 0.0, p.scroll_track);
            if max_top > 0 {
                let (hy, len) = bar_handle(track.top(), track.height(), top, max_top, view_rows, total_rows);
                let handle = egui::Rect::from_x_y_ranges(track.x_range(), hy..=hy + len).shrink(0.5);
                let fill = if bar_hovered { p.scroll_handle_hover } else { p.scroll_handle };
                painter.rect(handle, 1.0, fill, Stroke::new(1.0, p.header_text), egui::StrokeKind::Inside);
            }
        }
        // Pencil-thin outline around each heading. Drawn here, outside the
        // header cells, because each cell clips to its own rect.
        let painter = ui.painter();
        let ppp = ui.ctx().pixels_per_point();
        let px = |v: f32| ((v * ppp).round() + 0.5) / ppp; // centre of a physical pixel
        for r in &self.header_rects {
            let r = egui::Rect::from_min_max(egui::pos2(px(r.left()), px(r.top())), egui::pos2(px(r.right()), px(r.bottom() - 1.0)));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0 / ppp, p.header_border), egui::StrokeKind::Middle);
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

    fn waveform(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, p.bg);
        painter.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.grid));
        // The wave is drawn inset from the left; `resp` still covers the gap,
        // and a click there maps to 0:00.
        painter.rect_filled(
            egui::Rect::from_x_y_ranges(rect.left()..=rect.left() + WAVE_PAD_LEFT, rect.top() + 1.0..=rect.bottom()),
            0.0,
            p.wave_pad,
        );
        let rect = egui::Rect::from_min_max(rect.min + Vec2::new(WAVE_PAD_LEFT, 0.0), rect.max);
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
            painter.rect_filled(r, 0.0, p.wave_played.gamma_multiply(0.16));
            for x in [xa, xb] {
                painter.line_segment([egui::pos2(x, r.top()), egui::pos2(x, r.bottom())], Stroke::new(1.0, p.wave_played.gamma_multiply(0.7)));
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
            let crown = egui::Rect::from_center_size(egui::pos2(cx, top + h + 1.0), Vec2::splat(h * 2.0));
            painter.rect(crown, 0.0, p.playhead_crown, Stroke::new(1.0, Color32::BLACK), egui::StrokeKind::Outside);
        }

        // ---- interaction ------------------------------------------------
        // Left click: seek. Left drag: select (auditions on release). Left drag
        // from inside the selection: drag it out as a WAV. Right drag: scrub.
        let Some(d) = duration.filter(|d| *d > 0.0) else { return };
        let t_at = |x: f32| ((x - rect.left()) / w).clamp(0.0, 1.0) as f64 * d;
        let in_sel = |t: f64| sel.is_some_and(|(a, b)| t >= a && t <= b);
        let ctx = ui.ctx().clone();
        let seek_or_play = |me: &mut Self, t: f64| {
            if is_current && matches!(state, State::Playing | State::Paused) {
                me.player.seek(t);
            } else {
                me.player.play(path.clone(), t);
            }
        };

        if let Some(h) = resp.hover_pos() {
            let hx = h.x.max(rect.left());
            painter.line_segment(
                [egui::pos2(hx, rect.top() + 1.0), egui::pos2(hx, rect.bottom())],
                Stroke::new(1.0, p.dim.gamma_multiply(0.6)),
            );
            let icon = if resp.dragged_by(egui::PointerButton::Secondary) {
                egui::CursorIcon::ResizeHorizontal
            } else if DRAG_OUT_SUPPORTED && self.wave_drag.is_none() && in_sel(t_at(h.x)) {
                egui::CursorIcon::Grab
            } else {
                egui::CursorIcon::Text
            };
            ctx.set_cursor_icon(icon);
        }

        if resp.drag_started_by(egui::PointerButton::Primary) {
            let origin = ctx.input(|i| i.pointer.press_origin()).map(|o| t_at(o.x)).unwrap_or(0.0);
            if DRAG_OUT_SUPPORTED && let Some((a, b)) = sel.filter(|_| in_sel(origin)) {
                self.drag_out = Some(DragOut::Region(path.clone(), a, b));
            } else {
                self.wave_drag = Some(origin);
                self.wsel = Some((path.clone(), origin, origin));
            }
        }
        if let (Some(anchor), true) = (self.wave_drag, resp.dragged_by(egui::PointerButton::Primary))
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let t = t_at(pos.x);
            self.wsel = Some((path.clone(), anchor.min(t), anchor.max(t)));
        }
        if resp.drag_stopped_by(egui::PointerButton::Primary) && self.wave_drag.take().is_some() {
            match self.selection_for(&path) {
                // Anything under ~3 px is a sloppy click, not a selection.
                Some((a, b)) if (b - a) / d * w as f64 >= 3.0 => {
                    self.player.play_range(path.clone(), a, Some(b));
                }
                _ => self.wsel = None,
            }
        }
        if resp.clicked() {
            self.wsel = None;
            if let Some(pos) = resp.interact_pointer_pos() {
                seek_or_play(self, t_at(pos.x));
            }
        }
        let moved = ctx.input(|i| i.pointer.delta().x != 0.0);
        if (resp.secondary_clicked() || (resp.dragged_by(egui::PointerButton::Secondary) && moved))
            && let Some(pos) = resp.interact_pointer_pos()
        {
            seek_or_play(self, t_at(pos.x));
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
                let n = self.indexer.scanned.load(Relaxed);
                RichText::new(format!("Indexing… {} files", fmt_int(n as u64))).color(p.text)
            } else {
                let n = self.results.len() as u64;
                let mut s = format!("{} {}", fmt_int(n), if n == 1 { "object" } else { "objects" });
                if self.indexer.scanning.load(Relaxed) {
                    s += &format!("   ·   Indexing… {} files", fmt_int(self.indexer.scanned.load(Relaxed) as u64));
                }
                RichText::new(s).color(p.text)
            };
            ui.label(left);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                self.volume_ui(ui);
                ui.add_space(14.0);
                // While a new file is loading, keep what was shown, so a file
                // that ends or fails at once doesn't flash ▶ (B5).
                if !self.player.loading() {
                    self.status_shown = self.player_status(p);
                }
                if let Some((s, color)) = &self.status_shown {
                    let resp = ui.add(egui::Label::new(RichText::new(s).color(*color)).sense(Sense::click()));
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
    fn player_status(&self, p: Palette) -> Option<(String, Color32)> {
        if let Some(e) = &self.player.device_error {
            return Some((e.clone(), p.dim));
        }
        if let Some(e) = self.player.error() {
            return Some((e, Color32::from_rgb(0xC4, 0x2B, 0x1C)));
        }
        let state = self.player.state();
        if state == State::Idle {
            return None;
        }
        let icon = match state {
            State::Playing => "▶",
            State::Paused => "‖",
            _ => "■",
        };
        let mut s = String::new();
        if let Some(i) = self.player.info() {
            let d = i.duration.unwrap_or(0.0);
            let pos = self.player.position().min(d.max(0.0));
            s += &format!("{icon}  {} / {}", fmt_time(pos), fmt_time(d));
            let desc = i.describe();
            if !desc.is_empty() {
                s += &format!("    {desc}");
            }
        } else {
            s += icon;
        }
        Some((s, p.text))
    }

    /// Volume slider: +12 dB to -inf, 0.5 dB steps. Double-click resets to
    /// 0 dB; right-click picks the clipping mode used when boosting.
    fn volume_ui(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        let db = self.cfg.volume_db;
        let label = if db <= MIN_DB {
            "-∞ dB".to_string()
        } else if db == 0.0 {
            "0.0 dB".to_string()
        } else {
            format!("{db:+.1} dB")
        };
        let color = if db > 0.0 { Color32::from_rgb(0xD8, 0x7A, 0x00) } else { p.text };
        ui.add_sized([52.0, 16.0], egui::Label::new(RichText::new(label).color(color)).selectable(false));
        ui.spacing_mut().slider_width = 160.0;
        let resp = ui
            .add(egui::Slider::new(&mut self.cfg.volume_db, MIN_DB..=MAX_DB).step_by(0.5).show_value(false))
            .on_hover_text("Volume · double-click for 0 dB · right-click for clipping");
        let mut changed = resp.changed();
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
        resp.context_menu(|ui| {
            ui.label(RichText::new("When boosting above 0 dB").color(p.dim));
            if ui.radio_value(&mut self.cfg.soft_clip, true, "Soft clip").clicked()
                || ui.radio_value(&mut self.cfg.soft_clip, false, "Hard clip").clicked()
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
                ui.horizontal(|ui| {
                    if ui.button("Browse…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                        && !self.cfg.roots.contains(&dir)
                    {
                        self.cfg.roots.push(dir);
                        changed = true;
                    }
                    // An obvious entry box: its own fill and outline (blue
                    // while typing), "…" until something is typed.
                    let focused = ui.memory(|m| m.focused()) == Some(ui.make_persistent_id("folder-input"));
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut self.folder_input)
                            .id(ui.make_persistent_id("folder-input"))
                            .hint_text("…")
                            .font(small.clone())
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
                        let pb = PathBuf::from(self.folder_input.trim());
                        if pb.is_dir() && !self.cfg.roots.contains(&pb) {
                            self.cfg.roots.push(pb);
                            self.folder_input.clear();
                            changed = true;
                        }
                    }
                });
            });
        self.show_folders = open;
        if changed {
            self.indexer.set_roots(self.cfg.roots.clone());
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
            ("Ctrl+click path", "open in Explorer"),
            ("Right-click path", "copy folder"),
            ("Click ▶ status", "go to playing file"),
            ("Click / drag wave", "seek / select"),
            ("Right-drag wave", "scrub"),
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

const DRAG_OUT_SUPPORTED: bool = cfg!(windows);
const SCROLL_HANDLE_SCALE: f32 = 1.3;
const SCROLL_BAR_WIDTH: f32 = 8.0;
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

        if keyboard_is_ours(&ctx, search_id) {
            self.handle_keys(&ctx, search_id);
        }
        self.pump_search();

        egui::Panel::top("menu").frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(2, 1))).show(ui, |ui| {
            self.menu(ui);
        });
        egui::Panel::top("search")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 4, right: 4, top: 3, bottom: 4 }))
            .show(ui, |ui| {
                let border = if self.list_focus { p.grid } else { p.sel_border };
                // egui's caret is a full line tall and 1 point wide (a soft
                // ~2 px line on scaled displays). Hide it here and draw a
                // native-looking one below: one physical pixel, text height.
                ui.visuals_mut().text_cursor.stroke = Stroke::NONE;
                // egui would still blink its hidden caret, scheduling its own
                // wake-ups out of phase with ours (up to 4 frames/s idle).
                ui.visuals_mut().text_cursor.blink = false;
                let out = egui::TextEdit::singleline(&mut self.query)
                        .id(search_id)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(13.5))
                        // Padding goes on the frame: with a custom frame egui
                        // ignores `.margin()`, which left the text and caret
                        // touching the border.
                        .frame(
                            egui::Frame::new()
                                .stroke(Stroke::new(1.0, border))
                                .fill(p.bg)
                                // Left room for the ">" prompt.
                                .inner_margin(egui::Margin { left: 20, right: 6, top: 3, bottom: 3 }),
                        )
                    .show(ui);
                let resp = out.response.response.clone();
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
                if window_focused && free && !self.show_folders && !egui::Popup::is_any_open(&ctx) {
                    resp.request_focus();
                }
            });
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(p.status_bg).inner_margin(egui::Margin::symmetric(0, 3)))
            .show(ui, |ui| self.status(ui));
        egui::Panel::bottom("wave").frame(egui::Frame::new().fill(p.bg)).show(ui, |ui| self.waveform(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg)).show(ui, |ui| self.table(ui));

        if self.show_folders {
            self.folders_window(&ctx);
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

fn fmt_date(t: i64) -> String {
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

    #[test]
    fn only_the_selection_is_coloured() {
        for p in [LIGHT, DARK] {
            let sel = Some((100.0, 200.0));
            let play = Some(150.0);
            assert_eq!(p.wave_color(50.0, play, sel), p.wave_out, "played but outside: grey");
            assert_eq!(p.wave_color(250.0, play, sel), p.wave_out, "unplayed, outside: grey");
            assert_eq!(p.wave_color(120.0, play, sel), p.wave_played, "inside, played");
            assert_eq!(p.wave_color(180.0, play, sel), p.wave_sel, "inside, not yet played");
            assert_eq!(p.wave_color(180.0, None, sel), p.wave_sel, "inside, stopped");
            // No selection: unchanged behaviour.
            assert_eq!(p.wave_color(50.0, play, None), p.wave_played);
            assert_eq!(p.wave_color(250.0, play, None), p.wave);
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
}
