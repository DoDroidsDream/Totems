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
        }
    }
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
}

const LIGHT: Palette = Palette {
    bg: Color32::WHITE,
    text: Color32::from_rgb(0x1E, 0x1E, 0x1E),
    dim: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    header_text: Color32::from_rgb(0x4C, 0x60, 0x7A),
    grid: Color32::from_rgb(0xE5, 0xE5, 0xE5),
    hover: Color32::from_rgb(0xE5, 0xF3, 0xFF),
    sel: Color32::from_rgb(0xCC, 0xE8, 0xFF),
    sel_inactive: Color32::from_rgb(0xD9, 0xD9, 0xD9),
    sel_border: Color32::from_rgb(0x99, 0xD1, 0xFF),
    status_bg: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    wave: Color32::from_rgb(0xA8, 0xB4, 0xC2),
    wave_played: Color32::from_rgb(0x00, 0x78, 0xD7),
};

const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x1F, 0x1F, 0x1F),
    text: Color32::from_rgb(0xE6, 0xE6, 0xE6),
    dim: Color32::from_rgb(0x9A, 0x9A, 0x9A),
    header_text: Color32::from_rgb(0xB8, 0xC4, 0xD4),
    grid: Color32::from_rgb(0x33, 0x33, 0x33),
    hover: Color32::from_rgb(0x2A, 0x33, 0x3D),
    sel: Color32::from_rgb(0x1F, 0x4E, 0x79),
    sel_inactive: Color32::from_rgb(0x3A, 0x3A, 0x3A),
    sel_border: Color32::from_rgb(0x2F, 0x6E, 0xA8),
    status_bg: Color32::from_rgb(0x2B, 0x2B, 0x2B),
    wave: Color32::from_rgb(0x55, 0x60, 0x6C),
    wave_played: Color32::from_rgb(0x3A, 0x9B, 0xEF),
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
    has_bold: bool,
    last_title: String,
    styled: Option<bool>,
    /// File to hand to the OS drag-and-drop at the end of this frame.
    drag_out: Option<PathBuf>,
    /// The OS drag loop swallows the mouse-up; replay it to egui.
    release_pointer: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
        let cfg: Config = cc.storage.and_then(|s| eframe::get_value(s, "config")).unwrap_or_default();
        let has_bold = crate::fonts::install(&cc.egui_ctx);
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
            has_bold,
            last_title: String::new(),
            styled: None,
            drag_out: None,
            release_pointer: false,
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
            self.sent = Some(key);
            self.sent_gen += 1;
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
            self.res_index = Some(r.index);
            self.res_error = r.error;
            self.highlights = r.highlights;
            self.reselect();
        }
    }

    /// Keep the selected file selected across new results.
    fn reselect(&mut self) {
        self.sel_row = None;
        let (Some(path), Some(ix)) = (&self.sel_path, &self.res_index) else { return };
        if let Some(id) = ix.find(path) {
            let id = id as u32;
            self.sel_row = self.results.iter().position(|&r| r == id);
            self.scroll_to = self.sel_row;
        }
    }

    fn entry_path(&self, row: usize) -> Option<PathBuf> {
        let ix = self.res_index.as_ref()?;
        let id = *self.results.get(row)? as usize;
        Some(ix.full_path(id))
    }

    fn select(&mut self, row: usize, scroll: bool, play: bool) {
        if self.results.is_empty() {
            return;
        }
        let row = row.min(self.results.len() - 1);
        let changed = self.sel_row != Some(row);
        self.sel_row = Some(row);
        self.sel_path = self.entry_path(row);
        if scroll {
            self.scroll_to = Some(row);
        }
        if play && (changed || self.player.current.as_ref().map(|c| &c.path) != self.sel_path.as_ref()) {
            self.play_selected(0.0);
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

        if consume(Key::Enter) {
            if self.sel_row.is_none() && n > 0 {
                self.select(0, true, false);
            }
            self.list_focus = true;
            self.play_selected(0.0);
        }

        if self.list_focus {
            // Space toggles playback; it must not reach the search box.
            let space = ctx.input_mut(|i| {
                let had = i.events.iter().any(|e| matches!(e, Event::Text(t) if t == " "));
                i.events.retain(|e| !matches!(e, Event::Text(t) if t == " "));
                i.consume_key(Modifiers::NONE, Key::Space) || had
            });
            if space {
                if self.player.current.is_none() || self.player.current.as_ref().map(|c| &c.path) != self.sel_path.as_ref() {
                    self.play_selected(0.0);
                } else {
                    self.player.toggle_pause();
                }
            }
            for (m, k, d) in [
                (Modifiers::NONE, Key::ArrowLeft, -1.0),
                (Modifiers::NONE, Key::ArrowRight, 1.0),
                (Modifiers::SHIFT, Key::ArrowLeft, -5.0),
                (Modifiers::SHIFT, Key::ArrowRight, 5.0),
            ] {
                if consume_m(m, k) {
                    let pos = self.player.position();
                    self.player.seek(pos + d);
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
            if self.player.state() == State::Playing || self.player.state() == State::Paused {
                self.player.stop();
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
            ui.menu_button("View", |ui| {
                if ui.checkbox(&mut self.cfg.dark, "Dark").clicked() {
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Search Syntax").clicked() {
                    self.show_help = true;
                    ui.close();
                }
            });
        });
    }

    fn header_cell(&mut self, ui: &mut egui::Ui, title: &str, key: Option<SortKey>, right: bool) {
        let p = self.pal();
        let active = key.is_some() && key == Some(self.cfg.sort);
        let layout = if right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
        let resp = ui
            .with_layout(layout, |ui| {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(RichText::new(title).color(p.header_text))
                        .selectable(false)
                        .truncate(),
                )
            })
            .inner;
        let full = ui.interact(ui.max_rect(), ui.scope_id().with(title), Sense::click());
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
        let mut i = 0;
        while i < name.len() {
            let m = marks[i];
            let mut j = i;
            while j < name.len() && (marks[j] == m || !name.is_char_boundary(j)) {
                j += 1;
            }
            let fmt = if m {
                TextFormat { font_id: bold.clone(), color: if self.has_bold { color } else { p.wave_played }, ..Default::default() }
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
        let avail_h = ui.available_height();
        self.page_rows = ((avail_h - ROW_H) / ROW_H).floor().max(1.0) as usize;

        let mut tb = TableBuilder::new(ui)
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
            .max_scroll_height(f32::INFINITY);
        if let Some(r) = self.scroll_to.take() {
            tb = tb.scroll_to_row(r, None);
        }
        let sel_row = self.sel_row;
        let sel_fill = if self.list_focus { p.sel } else { p.sel_inactive };
        let playing_path = self
            .player
            .current
            .as_ref()
            .filter(|_| matches!(self.player.state(), State::Playing | State::Paused))
            .map(|c| c.path.clone());
        let infos = self.infos.clone();
        tb.header(22.0, |mut h| {
            h.col(|ui| self.header_cell(ui, "Name", Some(SortKey::Name), false));
            h.col(|ui| self.header_cell(ui, "Path", Some(SortKey::Path), false));
            h.col(|ui| self.header_cell(ui, "Length", None, true));
            h.col(|ui| self.header_cell(ui, "Size", Some(SortKey::Size), true));
            h.col(|ui| self.header_cell(ui, "Date Modified", Some(SortKey::Modified), false));
        })
        .body(|body| {
            body.rows(ROW_H, results.len(), |mut row| {
                let r = row.index();
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
                    ui.add_space(4.0);
                    if is_playing {
                        ui.add(egui::Label::new(RichText::new("▶").size(9.0).color(p.wave_played)).selectable(false));
                    }
                    ui.add(egui::Label::new(job).selectable(false).truncate());
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.add(egui::Label::new(RichText::new(ix.dir_str(id)).color(color)).selectable(false).truncate());
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    let len = match infos.get(&path) {
                        Some(Some(Info { duration: Some(d), .. })) => fmt_len(d),
                        _ => String::new(),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(RichText::new(len).color(color)).selectable(false));
                    });
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(RichText::new(fmt_size(ix.size[id])).color(color)).selectable(false));
                    });
                });
                row.col(|ui| {
                    if selected {
                        ui.painter().rect_filled(ui.max_rect().expand2(Vec2::new(4.0, 0.0)), 0.0, sel_fill);
                    }
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(RichText::new(fmt_date(ix.mtime[id])).color(color)).selectable(false).truncate());
                });
                let resp = row.response();
                if resp.clicked() {
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

        if let Some(r) = dragged
            && DRAG_OUT_SUPPORTED
        {
            self.list_focus = true;
            self.select(r, false, false);
            self.drag_out = self.sel_path.clone();
        }

        if let Some((r, dbl)) = clicked {
            self.list_focus = true;
            if dbl {
                self.select(r, false, false);
                self.play_selected(0.0);
            } else {
                self.select(r, false, true);
            }
        }
    }

    fn waveform(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, p.bg);
        painter.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, p.grid));
        let Some(path) = self.sel_path.clone().or_else(|| self.player.current.as_ref().map(|c| c.path.clone())) else {
            return;
        };
        let wave = self.waves.get(&path);
        let is_current = self.player.current.as_ref().is_some_and(|c| c.path == path);
        let info = if is_current { self.player.info() } else { None }.or_else(|| self.infos.get(&path).flatten());
        if let Some(i) = &info {
            self.infos.put(&path, i.clone());
        }
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
        let cols = w as usize;
        let mut mesh = egui::Mesh::default();
        let ppp = ui.ctx().pixels_per_point();
        let snap = |v: f32| (v * ppp).round() / ppp;
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
            let c = if px + 0.5 <= play_x && frac > 0.0 { p.wave_played } else { p.wave };
            mesh.add_colored_rect(egui::Rect::from_x_y_ranges(px..=px + 1.0, y0..=y1), c);
        }
        painter.add(egui::Shape::mesh(mesh));
        drop(peaks);
        if wave.failed.load(Relaxed) {
            painter.text(rect.center(), egui::Align2::CENTER_CENTER, "No preview", FontId::proportional(12.0), p.dim);
        }
        if frac > 0.0 {
            painter.line_segment(
                [egui::pos2(play_x, rect.top() + 1.0), egui::pos2(play_x, rect.bottom())],
                Stroke::new(1.0, p.wave_played),
            );
        }
        if let Some(h) = resp.hover_pos() {
            painter.line_segment(
                [egui::pos2(h.x, rect.top() + 1.0), egui::pos2(h.x, rect.bottom())],
                Stroke::new(1.0, p.dim.gamma_multiply(0.6)),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if (resp.clicked() || resp.dragged())
            && let (Some(d), Some(at)) = (duration, resp.interact_pointer_pos())
        {
            let t = ((at.x - rect.left()) / w).clamp(0.0, 1.0) as f64 * d;
            if is_current && state != State::Idle && state != State::Ended {
                self.player.seek(t);
            } else {
                self.player.play(path, t);
            }
        }
    }

    fn status(&mut self, ui: &mut egui::Ui) {
        let p = self.pal();
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            let left = if let Some(e) = &self.res_error {
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
                if let Some(e) = &self.player.device_error {
                    ui.label(RichText::new(e).color(p.dim));
                    return;
                }
                if let Some(e) = self.player.error() {
                    ui.label(RichText::new(e).color(Color32::from_rgb(0xC4, 0x2B, 0x1C)));
                    return;
                }
                let state = self.player.state();
                if state == State::Idle {
                    return;
                }
                let info = self.player.info();
                let icon = match state {
                    State::Playing => "▶",
                    State::Paused => "‖",
                    _ => "■",
                };
                let mut s = String::new();
                if let Some(i) = &info {
                    let d = i.duration.unwrap_or(0.0);
                    let pos = if state == State::Ended { d } else { self.player.position().min(d.max(0.0)) };
                    s += &format!("{icon}  {} / {}", fmt_time(pos), fmt_time(d));
                    let desc = i.describe();
                    if !desc.is_empty() {
                        s += &format!("    {desc}");
                    }
                } else {
                    s += icon;
                }
                ui.label(RichText::new(s).color(p.text));
            });
        });
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
        ui.spacing_mut().slider_width = 96.0;
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
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.cfg.volume_db = (self.cfg.volume_db + scroll.signum() * 0.5).clamp(MIN_DB, MAX_DB);
                changed = true;
            }
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

    fn folders_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_folders;
        let mut changed = false;
        egui::Window::new("Folders")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label("Audio files in these folders are indexed and kept up to date.");
                ui.add_space(6.0);
                let mut remove = None;
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (i, r) in self.cfg.roots.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("Remove").clicked() {
                                remove = Some(i);
                            }
                            ui.label(r.to_string_lossy());
                        });
                    }
                    if self.cfg.roots.is_empty() {
                        ui.label(RichText::new("No folders.").italics());
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
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut self.folder_input).hint_text("Type a folder path").desired_width(240.0),
                    );
                    let enter = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Add").clicked() || enter {
                        let pb = PathBuf::from(self.folder_input.trim());
                        if pb.is_dir() && !self.cfg.roots.contains(&pb) {
                            self.cfg.roots.push(pb);
                            self.folder_input.clear();
                            changed = true;
                        }
                    }
                });
                ui.add_space(4.0);
                if ui.button("Reset to Defaults").clicked() {
                    self.cfg.roots = default_roots();
                    changed = true;
                }
            });
        self.show_folders = open;
        if changed {
            self.indexer.set_roots(self.cfg.roots.clone());
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Search Syntax").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            egui::Grid::new("syntax").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                for (a, b) in [
                    ("door slam", "both words (AND)"),
                    ("whoosh | swish", "either word (OR)"),
                    ("!loop", "exclude (NOT)"),
                    ("<rain | wind> !city", "grouping"),
                    ("\"door slam\"", "exact phrase"),
                    ("*.wav   kick_??", "wildcards (whole name)"),
                    ("foley\\door", "match against full path"),
                    ("ext:wav;flac", "extensions"),
                    ("size:>2mb   size:1mb..5mb", "file size"),
                    ("size:tiny|small|large|huge", "size classes"),
                    ("case:  path:  regex:", "per-term modifiers"),
                ] {
                    ui.monospace(a);
                    ui.label(b);
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.strong("Keys");
            egui::Grid::new("keys").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                for (a, b) in [
                    ("↑ ↓  PgUp PgDn", "move selection and play instantly"),
                    ("Enter / double-click", "play from start"),
                    ("Space", "pause / resume (after navigating)"),
                    ("← →  (Shift)", "seek 1 s (5 s)"),
                    ("Click / drag waveform", "seek / scrub"),
                    ("Esc", "stop"),
                    ("Ctrl+F", "back to search"),
                    ("Ctrl+C", "copy full path"),
                ] {
                    ui.monospace(a);
                    ui.label(b);
                    ui.end_row();
                }
            });
        });
        self.show_help = open;
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
        let theme = if self.cfg.dark { egui::Theme::Dark } else { egui::Theme::Light };
        ctx.set_theme(theme);
        ctx.set_visuals_of(theme, v);
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(6.0, 0.0);
            s.spacing.interact_size.y = 18.0;
            s.interaction.selectable_labels = false;
            s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.0));
            s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
            s.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.0));
        });
    }
}

const DRAG_OUT_SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

/// Hand a file to the OS drag-and-drop as a copy: the drop target (Explorer,
/// Finder, a DAW, a mail client...) receives its own copy of the file.
#[cfg(any(windows, target_os = "macos"))]
fn start_os_drag(frame: &eframe::Frame, path: PathBuf) {
    let icon = include_bytes!("../assets/drag.png").to_vec();
    let opts = drag::Options { skip_animatation_on_cancel_or_failure: false, mode: drag::DragMode::Copy };
    let _ = drag::start_drag(frame, drag::DragItem::Files(vec![path]), drag::Image::Raw(icon), |_, _| {}, opts);
}

#[cfg(not(any(windows, target_os = "macos")))]
fn start_os_drag(_frame: &eframe::Frame, _path: PathBuf) {}

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
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
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

        self.handle_keys(&ctx, search_id);
        self.pump_search();

        egui::Panel::top("menu").frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(2, 1))).show(ui, |ui| {
            self.menu(ui);
        });
        egui::Panel::top("search")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin { left: 2, right: 2, top: 2, bottom: 4 }))
            .show(ui, |ui| {
                let border = if self.list_focus { p.grid } else { p.sel_border };
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(search_id)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(13.5))
                        .margin(egui::Margin::symmetric(5, 4))
                        .frame(egui::Frame::new().stroke(Stroke::new(1.0, border)).fill(p.bg)),
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

        if let Some(path) = self.drag_out.take() {
            start_os_drag(frame, path);
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
            let ms = (dur / width * 1000.0).clamp(16.0, 100.0);
            ctx.request_repaint_after(Duration::from_millis(ms as u64));
        } else if self.indexer.scanning.load(Relaxed) {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "config", &self.cfg);
    }
}

// ------------------------------------------------------------- formatting

pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
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
}
