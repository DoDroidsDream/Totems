# Totems style guide

How Totems looks and behaves, written so another agent can give a different desktop tool (a
file browser, a sample manager, a log viewer, any list-heavy utility) the same feel. Every value
here comes from the running code in `src/app.rs` and `src/fonts.rs` as of commit `850064db`
(2026-11-20). If this file and the code disagree, the code wins. Then fix this file.

Liam approved every rule marked **[Liam]** in so many words. Don't change those without asking him.

## 1. The idea in one paragraph

A native-feeling Windows utility in the spirit of *Everything* (voidtools): one search box, one
dense list, a thin status bar. Nothing decorative. The look comes from type, alignment and a few
exact colours, not from cards, shadows, icons or rounded corners. Speed is part of the style:
results on every keystroke, sound on every keypress, no spinners, no animation, no flicker.
**[Liam]** "I like Everything's uncluttered look": that is the reference.

## 2. Rules that define the feel

1. **Dense and quiet.** 19 px rows, 13 px text, no row stripes, no gridlines in the body.
2. **Native, not themed.** Segoe UI, Windows selection blues, a 1-pixel caret, square-ish
   corners (radius 1 to 2). It should look like it shipped with Windows, but sharper.
3. **One accent colour per job.** Blue is for headings, the prompt and played audio. Orange
   appears once (the playhead crown). Nothing else is coloured.
4. **Whole rows only.** The list always scrolls by whole rows, and the top row always sits fully
   under the headings. No half rows, no smooth-scroll drift.
5. **Every row looks the same.** No fades, no dimmed edges, no alternating stripes. Only the
   selection and the playing marker set a row apart.
6. **Keyboard first, mouse complete.** Everything works from the keyboard. The mouse gets direct
   actions (click to seek, drag out, Ctrl+click to open) and never a hidden right-click-only feature.
7. **Never flash a state that isn't true.** Don't show ▶ until audio is really playing.
   Don't show a playhead after the sound has ended.
8. **Idle means idle.** No animation and no repaint loop while nothing changes. The only timers
   are the caret blink and playback.

## 3. Colour tokens

Two themes with the same token names. Dark is the default look Liam reviews in.

| Token | Dark | Light | Used for |
|---|---|---|---|
| `bg` | `#1F1F1F` | `#FFFFFF` | Window, panels, list, search box |
| `text` | `#E6E6E6` | `#1E1E1E` | All body text, every list column |
| `dim` | `#9A9A9A` | `#6D6D6D` | Secondary notes, "Indexing…", sort chevron, hover line |
| `header_text` | `#4CA6F2` | `#005FB8` | Column headings, `>` prompt, scroll handle outline (6.3:1 contrast in both themes) |
| `grid` | `#333333` | `#E5E5E5` | Hairlines (waveform top edge, unfocused search border, widget strokes) |
| `hover` | `#2A333D` | `#E5F3FF` | Hovered menu items and buttons |
| `sel` | `#1F4E79` | `#CCE8FF` | Selected row while the list has focus |
| `sel_inactive` | `#3A3A3A` | `#D9D9D9` | Selected row while focus is elsewhere |
| `sel_border` | `#2F6EA8` | `#99D1FF` | Search box border while it has focus |
| `status_bg` | `#2B2B2B` | `#F0F0F0` | Status bar, scroll track |
| `header_border` | `#000000` | `#A0A0A0` | One-physical-pixel outline round each heading |
| `scroll_handle` / hover | `#9E9E9E` / `#BEBEBE` | `#C2C2C2` / `#A8A8A8` | Scroll bar handle |
| `wave` | `#55606C` | `#A8B4C2` | Waveform, not yet played |
| `wave_played` | `#3A9BEF` | `#0078D7` | Waveform already played; ▶ marker in the list |
| `wave_sel` | `#2E669A` | `#7FB6E6` | Inside a selection, not yet played |
| `wave_out` | `#3A3E43` | `#D6DADF` | Outside a selection (greyed) |
| `wave_pad` | `#3C3C3C` | `#C8C8C8` | 8 px strip left of the waveform |
| `playhead` | `#FFFFFF` | `#1E1E1E` | Playhead line (white would vanish on the light strip) |
| `playhead_crown` | `#FF8C1A` | `#F27A00` | Small square on the playhead, black 1 px outline |
| `crown` / `crown_edge` | white / none | white / `#6D6D6D` | Selection edge triangles |
| error | `#C42B1C` | `#C42B1C` | Error notices (Windows' own error red) |

**Rule:** a new colour needs a job no existing token does. Check contrast (≥ 4.5:1 for text)
in both themes.

## 4. Type

- **Font: Segoe UI** (`segoeui.ttf`, bold `segoeuib.ttf`), loaded from `C:\Windows\Fonts`.
  **[Liam]** MS Shell Dlg (Tahoma/Microsoft Sans Serif) was tried and rejected: "too blobby and
  thick". Keep egui's bundled font *after* Segoe UI, only for symbols (▶ ‖ ■ chevrons).
- **Fallbacks for any script in file names:** Segoe UI Symbol, Microsoft YaHei, Yu Gothic,
  Malgun Gothic. A name must never show as boxes.
- **Sizes:** body 13, search box 13.5, secondary columns 12 (`SUB_FONT`), waveform label 11,
  monospace 12. **[Liam]** Secondary columns are one size smaller than the main one.
- **Sentence case** everywhere ("Autoplay on Select", "Search Syntax" are menu titles; notices
  are plain sentences). No all-caps.
- **Search matches are shown in bold**, same colour as the text. If no bold face loads, matches
  turn `wave_played` blue instead.
- Leading spaces in names are not drawn (they'd indent the row); search still sees them.

## 5. Layout

Top to bottom, all on `bg` except the status bar:

```
┌ Menu bar ─ File  Search  Playback  View  Help ──────────────┐  margin 2×1
│ ┌───────────────────────────────────────────────────────┐   │  search panel, margin L4 R4 T3 B4
│ │ >  query_                                             │   │  1 px border: grid, or sel_border when focused
│ └───────────────────────────────────────────────────────┘   │
│  Name ^            │ Path         │ Length │ Size │ Date   ▐ │  headings 22 px, blue, outlined
│  row (19 px)       │              │        │      │        ▐ │  scroll bar 8 px, right of table
│  ...                                                       ▐ │
├─ waveform strip, 56 px, 8 px grey pad on the left ─────────┤
└─ status bar (status_bg): "12,345 objects"   ▶ 0:12 / 0:30  vol ┘
```

- **Window:** opens 1100×680, minimum 420×240. Title is `query - Totems`, or `Totems` when
  the query is empty.
- **Columns:** Name 320 (left), Path 420 (left), Length 84 (right), Size 90 (right),
  Date Modified (rest, left). All resizable and clipped, never wrapped. Numbers are right-aligned.
- **Padding:** Name text starts 16 px in (`NAME_PAD`), so the ▶ marker fits in the gap and never
  moves the name. Other cells have 6 px (`CELL_PAD`). Headings use the same padding as their
  column, so heading and cell text line up exactly.
- **Spacing:** item spacing 6×0, interact height 18. Tight on purpose.
- **Sort chevron:** a 1 px `dim` line chevron (8×3 px) centred on the top edge of the active
  heading, like Explorer. Clicking a heading sorts by it; clicking again reverses. Name and Path
  start ascending; Size and Date start descending.

## 6. Components

**Search box.** Has the caret whenever nothing else needs the keyboard, like a launcher. Blue
`>` prompt painted 7 px in. Text starts 20 px in. **[Liam]** Caret: 1 *physical* pixel wide,
13 px tall, blinks 0.5 s on / 0.5 s off, solid right after any move. egui's own caret is hidden
and not blinking (see §8).

**List.** Whole-row scrolling (§2.4). Selected row: `sel` fill across every cell, or
`sel_inactive` when the list doesn't have focus. The playing file shows a 9 px ▶ in
`wave_played` in the Name padding. Mouse wheel moves 3 rows per notch. One blank spacer row
after the last result lets the last real row sit fully in view.

**Scroll bar.** **[Liam]** Custom, always visible, **thin (8 px)**: "I don't want wider, I liked
it thin." Track `status_bg`. Handle `scroll_handle` with a pencil-thin `header_text` outline,
radius 1. Handle at least 1.3× egui's minimum length so it stays grabbable over a million rows.
Pressing the track jumps the handle's centre to the pointer. It moves by whole rows.

**Headings.** `header_text` blue, 22 px tall, a one-physical-pixel `header_border` outline round
each one, drawn after the table (cells clip their own rect).

**Waveform strip.** 56 px. Min/max peak columns, one physical pixel each, snapped to the pixel
grid. **[Liam]** Only the selection is coloured: inside it `wave_sel`/`wave_played`, outside
`wave_out`. Selection edges get small white downward triangles ("crowns"). Playhead: 1 px line
with a small orange square crown, black outline. Hidden while loading and once the sound has
ended. Hover shows a faint `dim` line. Cursor: text beam to select, grab inside a selection,
resize while scrubbing. The selection label (`0:01.200 – 0:02.500 (1.300 s)`, 11 px) sits top
left on a translucent `bg` chip.

**Status bar.** `status_bg`, margin 0×3. Left: count ("12,345 objects"), or progress, or a
notice (info 2 s in `dim`, error 5 s in red). Right: player status (`▶ 0:12 / 0:30    48 kHz · 24-bit
· Stereo · PCM`, using ▶, ‖ or ■) then the volume slider (160 px). Clicking the player status
jumps the list to the playing file. While a new file loads, the old status stays up, so nothing
flashes.

**Menus and dialogs.** egui menus with radius 2, widgets radius 1, `hover` fill. Help is a
two-column grid: monospace keys on the left, plain descriptions on the right. Dialogs (Folders)
are plain egui windows, not collapsible, one sentence explaining what they do at the top.

## 7. Interaction conventions

| Input | Behaviour |
|---|---|
| Type | Search updates on every keystroke; latest query wins |
| ↑ ↓ PgUp PgDn, Ctrl+Home/End | Move selection; plays it if Autoplay is on |
| Ctrl+↑ / Ctrl+↓ | Top of list / next file with a different name |
| Enter, double-click | Play from start (or the selection) |
| Space | Play / pause |
| ← → (Shift) | Seek 1 s (5 s) |
| Esc | Stop, then clear the selection |
| Tab | Toggle search ↔ list |
| Ctrl+F / Ctrl+C | Back to search / copy full path (status bar confirms) |
| Ctrl+click Path cell | Open Explorer with the file selected (pointing-hand cursor while Ctrl is held over it) |
| Click status ▶ | Select and scroll to the playing file |
| Drag a row out | Copy the file to the drop target |

Conventions behind the table:
- Confirm silent actions in the status bar (copied path, export errors). Never use a modal for
  information.
- Cursors tell you what a click will do (pointing hand, text beam, grab, resize).
- A toggle that changes how clicks behave (Autoplay) has hover text that names both modes.

## 8. Building it in egui 0.37 (traps we hit)

```rust
ctx.all_styles_mut(|s| {
    s.spacing.item_spacing = Vec2::new(6.0, 0.0);
    s.spacing.interact_size.y = 18.0;
    s.interaction.selectable_labels = false;          // list text must not be selectable
    s.text_styles.insert(TextStyle::Body, FontId::proportional(13.0));
    let mut scroll = egui::style::ScrollStyle::solid(); // always-visible, not floating
    scroll.bar_width = 8.0;
    scroll.handle_min_length = (scroll.handle_min_length * 1.3).round();
    scroll.fade.strength = 0.0;                        // see trap 1
    s.spacing.scroll = scroll;
});
```

1. **Scroll areas fade their edges.** egui 0.37 dims the first and last visible rows of any
   scrolled area (`ScrollFadeStyle`, strength 0.5). That made the top row look dimmer.
   Set `scroll.fade.strength = 0.0`.
2. **The hidden caret still blinks.** Hiding egui's caret with `text_cursor.stroke =
   Stroke::NONE` leaves its blink timer running, so the app wakes up to 4 times a second out of
   phase with your own caret. Also set `text_cursor.blink = false`.
3. **Snap hairlines to physical pixels.** `x = ((x * ppp).round() + 0.5) / ppp` and a stroke of
   `1.0 / ppp`. Otherwise "1 px" lines blur to 2 soft pixels at 125–150 % scaling.
4. **egui's table scrolls smoothly; we don't.** Hide egui's bar
   (`ScrollBarVisibility::AlwaysHidden`), take wheel events in `raw_input_hook` and turn them
   into row steps, then set `vertical_scroll_offset(top_row * ROW_H)` each frame. If the offset
   ever lands off the row grid (window resize), floor it and repaint once.
5. **Custom frames ignore `.margin()`** on `TextEdit`: put the padding on the `Frame`.
6. **Cells clip to their own rect**, so outlines that span cells (heading borders) must be
   painted after the table, from rects you saved during layout.
7. **Repaint on purpose.** Workers call `request_repaint` when results arrive. While playing,
   use `request_repaint_after` sized so the playhead moves one pixel per frame (16–100 ms),
   capped at the time left in the sound. Never repaint from a real-time audio thread.
8. **Paint markers into padding** (▶ in `NAME_PAD`) so state changes never shift text sideways.

## 9. Things Liam rejected (don't bring back)

- MS Shell Dlg / Tahoma / Microsoft Sans Serif: "too blobby and thick".
- A wider scroll bar, or one that only appears on hover.
- egui's default 2 px, full-line-height caret.
- Rows that look different from each other (the fade, stripes).
- ▶ flashing for a file that ends or fails at once.

## 10. How to work with Liam on UI

1. Change one visible thing at a time and show it in the running app. On Windows, run
   `cargo run --release` and look at it. Screenshots beat descriptions.
2. Before changing anything marked **[Liam]**, ask. Those are his decisions, not defaults.
3. When he reports a visual glitch, find the cause in the framework source before working
   around it. The dim top row was egui's fade, not our colours.
4. Keep the reply short: what changed, how to see it, one next step. He reads in ADHD mode.
5. Record new decisions here, marked **[Liam]**, in the same commit as the code.
