# Totems style guide

How Totems looks and behaves, written so another agent can give a different desktop tool (a
file browser, a sample manager, a log viewer, any list-heavy utility) the same feel. Every value
here comes from the running code in `src/app.rs` and `src/fonts.rs` as of `e1a03ab`
(2026-11-24: loop, autoplay next, bookmarks). If this file and the code disagree, the code
wins. Then fix this file.

Liam approved every rule marked **[Liam]** in so many words. Don't change those without asking him.

**Start with §2: the screenshot checklist.** It lists the mistakes Liam had to correct again and
again in the 2026-11-20/21 sessions. Run through it before you show him anything.

## 1. The idea in one paragraph

A native-feeling Windows utility in the spirit of *Everything* (voidtools): one search box, one
dense list, a thin status bar. Nothing decorative. The look comes from type, alignment and a few
exact colours, not from cards, shadows, icons or rounded corners. Speed is part of the style:
results on every keystroke, sound on every keypress, no spinners, no animation, no flicker.
**[Liam]** "I like Everything's uncluttered look": that is the reference.

## 2. Screenshot checklist: Liam's repeated corrections

Agents keep making these mistakes, and Liam corrected every one of them at least once in the
2026-11-20/21 sessions. Check every screenshot against this list before you show it to him. Each item gives
the wrong version first, then the right one.

1. **Text entry boxes: the border must hug the text.** Wrong: egui's default frame, with roomy
   padding, body-size text and a box that stretches to fill the row. Right: 1 px outline,
   **4×1 px** inner margin, text **one size smaller than body (12 px)**, a fixed width just big
   enough for the job, and a caret that fits inside the box with room above and below. See §7
   *Text entry boxes*. Liam's words: "the cursor here is to big, should fit neatly into the
   box", "should also have a small amount of padding on the left of the text".
2. **Buttons: padding hugs the text, and the text is small and light.** Wrong: egui's 8×2
   padding, 13–14 px Regular or Bold. Right: **4×1 px** padding, **Segoe UI Semilight 12 px**.
   Liam: "reduce horizontal borders on buttons, note this as a consistent thing claude does
   wrong", "make the size of text smaller in all buttons, make it thinner it looks too bold".
3. **Menus and pop-outs sit snug.** Wrong: egui's 6 px menu margin. Right: **3×2 px**. Liam:
   "the border needs to sit nice and snug up against the text with a little bit of padding but
   much less than whats there right now."
4. **Symbols, not words.** Wrong: a "Remove" or "Add" text button, long hints, a sentence where
   a word will do. Right: a 13 px **×** or **+** square, the hint "…", a header that just says
   "Indexed", and Help descriptions of 2 to 4 words. Liam: "tighten up all the wording so its a
   much smaller box."
5. **Nothing too bold, too bright or too big.** This applies to emphasis (search matches use
   Semibold dimmed 12 %, not Bold), titles (Semilight 13) and fonts (Segoe UI, never Tahoma).
   Liam: "too bold and thick. Too bright", "too blobby and thick".

6. **Lines: pencil thin, black, in straight columns, never lit.** Wrong: egui's soft 1 pt
   lines, grey dividers, a hover highlight, a border one pixel off the line above it, a gap
   between a scroll bar and its border. Right: one *physical* pixel of `header_border`, snapped,
   and every line that continues another sits in **the same pixel column**. Measure it in the
   screenshot, don't eyeball it. Liam: "should line up perfectly", "the highlighted line should
   be no thicker than the column heading divider line", "the mouse icon change when hovering
   over the dividers is enough". See §7 *Lines and borders*.

Two more checks that follow from these:

- **Windows end exactly where Liam marks them.** He gives edges in pixels ("9 pixels to the
  right of the '+' buttons right border"). Size the window to its content and set the margin
  so the edge lands there. Never leave egui's default 320 px width.
- **Centre titles over the whole window**, not between the left edge and the × button (§9 trap 9).

## 3. Rules that define the feel

1. **Dense and quiet.** 19 px rows, 13 px text, no row stripes, no gridlines in the body.
2. **Native, not themed.** Segoe UI, Windows selection blues, a 1-pixel caret, square-ish
   corners (radius 1 to 2). It should look like it shipped with Windows, but sharper.
3. **One accent colour per job.** Blue is for headings, the prompt and audio not yet played.
   Orange means "look here": the playhead crown, and the volume label while it boosts above
   0 dB. Red is for errors. Nothing else is coloured.
4. **Whole rows only.** The list always scrolls by whole rows, and the top row always sits fully
   under the headings. No half rows, no smooth-scroll drift.
5. **Every row looks the same.** No fades, no dimmed edges, no alternating stripes. Only the
   selection and the playing marker set a row apart.
6. **Keyboard first, mouse complete.** Everything works from the keyboard. The mouse gets direct
   actions (click to seek, drag out, Ctrl+click to open). A right-click action must be named
   somewhere you can see: Help lists "Right-click path", and the volume slider's tooltip names
   its right-click menu.
7. **Never flash a state that isn't true.** Don't show ▶ until audio is really playing. Don't
   show it at all for sounds shorter than 0.1 s (`SHOWN_AFTER`). Don't show a playhead after the
   sound has ended.
8. **Idle means idle.** No animation and no repaint loop while nothing changes. The only timers
   are the caret blink and playback.
9. **[Liam] Grey frame, dark content, black lines between.** Everything that holds content is
   `bg` (search box, list, waveform). Everything round it is `status_bg` grey: the search
   panel, the gutter left of the Name column, the waveform's pads, the scroll track, the status
   bar. Where the two meet, a one-pixel black line. A new panel follows the same pattern.
10. **Small chrome, full-size content.** Body text (names) is 13 px. Everything around it
   (buttons, entry boxes, secondary columns, dialog text) is one step smaller at 12 px, and
   lighter where it's a control. The chrome should never compete with the list.

## 4. Colour tokens

Two themes with the same token names. Dark is the default look Liam reviews in.

| Token | Dark | Light | Used for |
|---|---|---|---|
| `bg` | `#1F1F1F` | `#FFFFFF` | Content: list, search box, waveform; menu bar |
| `text` | `#E6E6E6` | `#1E1E1E` | All body text, every list column |
| `dim` | `#9A9A9A` | `#6D6D6D` | Secondary notes, "Indexing…", sort chevron, hover line; at 60 % it outlines entry boxes and icon squares |
| `header_text` | `#4CA6F2` | `#005FB8` | Column headings, `>` prompt, scroll handle outline (6.3:1 contrast in both themes) |
| `grid` | `#333333` | `#E5E5E5` | egui widget strokes (none of our own lines) |
| `hover` | `#2A333D` | `#E5F3FF` | Hovered menu items, buttons and icon squares |
| `sel` | `#1F4E79` | `#CCE8FF` | Selected row while the list has focus |
| `sel_inactive` | `#3A3A3A` | `#D9D9D9` | Selected row while focus is elsewhere |
| `sel_border` | `#2F6EA8` | `#99D1FF` | Border of a secondary entry box while it has focus (not the search box) |
| `status_bg` | `#2B2B2B` | `#F0F0F0` | **The frame:** status bar, search panel (behind the box), Name gutter, the waveform's 12 px pads, scroll track; also the fill of secondary entry boxes |
| `header_border` | `#000000` | `#A0A0A0` | **Every line:** heading outlines, column dividers, the border beside the scroll bar, the search box outline, the waveform frame, the playhead crown's outline. One physical pixel |
| `groove` | egui's | `#C2C2C2` | **[Liam]** Volume slider groove, light theme only, with a one-physical-pixel outline in the handle's colour (`#3C3C3C`) |
| `scroll_handle` / hover | `#9E9E9E` / `#BEBEBE` | `#C2C2C2` / `#A8A8A8` | Scroll bar handle |
| `scroll_end` | `#C24A3E` | `#C42B1C` | **[Liam]** Scroll handle outline while it rests at the top or bottom |
| `accent` | `#3A9BEF` | `#0078D7` | **[Liam]** Waveform not yet played (all of it when stopped); ▶ marker in the list; waveform selection tint |
| `wave_played` | `#55606C` | `#A8B4C2` | **[Liam]** Waveform already played (grey: blue turns grey as it plays) |
| `wave_out` | `#3A3E43` | `#D6DADF` | Outside a selection (greyed) |
| `playhead` | `#FFFFFF` | `#1E1E1E` | Playhead line (white would vanish on the light strip) |
| `playhead_crown` | `#FF8C1A` | `#F27A00` | Small square on the playhead, black 1 px outline, flush under the waveform's top line |
| `crown` / `crown_edge` | white / none | white / `#6D6D6D` | Selection edge triangles |
| `BOOST` | `#D87A00` | `#D87A00` | Volume label while above 0 dB, in a pencil-thin `header_border` box (2.7:1 on the light status bar; the box is what makes it read) |
| error (literal) | `#C42B1C` | `#C42B1C` | Error notices (Windows' own error red) |

**Rule:** a new colour needs a job no existing token does. Check contrast (≥ 4.5:1 for text)
in both themes.

## 5. Type

- **Font: Segoe UI** (`segoeui.ttf`, bold `segoeuib.ttf`), loaded from `C:\Windows\Fonts`.
  **[Liam]** MS Shell Dlg (Tahoma/Microsoft Sans Serif) was tried and rejected: "too blobby and
  thick". Keep egui's bundled font *after* Segoe UI, only for symbols (the list's ▶, chevrons). The status bar's ▶ ‖ ■ are drawn as shapes, not glyphs (see *Status bar*).
- **Fallbacks for any script in file names:** Segoe UI Symbol (loaded at startup), then
  Microsoft YaHei, Yu Gothic, Malgun Gothic. The three CJK fonts (47 MB) load **lazily** as font
  providers, each the first time a character no loaded font has is drawn. A name must never show
  as boxes, and a user without CJK names must never pay for those fonts.
- **Size scale.** Use only these sizes:

  | Size | Face | Used for |
  |---|---|---|
  | 13.5 | Regular | Main search box text and `>` prompt (the one input that's the whole point of the app) |
  | 13 | Regular | Body: Name column, list text |
  | 13 | Semilight | Window titles, centred |
  | 12 | Regular | Secondary columns (`SUB_FONT`), **secondary text entry boxes**, dialog text, Help rows |
  | 12 | Semilight | Buttons and menu items |
  | 11 | Regular | Waveform selection label |
  | 12 | Monospace | Anything monospace |

- **[Liam] Secondary columns are one size smaller than the main one.** The same applies to
  every control: chrome is one step below body.
- **[Liam] Buttons, menu items and window titles use Segoe UI Semilight** (`segoeuisl.ttf`,
  family `light`). Regular Segoe UI read "too thick" and "too big" there.
- **Title Case for names, sentence case for sentences** (the Windows convention). Menus, menu
  items, column headings and window titles are names: "Autoplay on Select", "Crossfade Loop",
  "Date Modified", "Search Syntax". Notices, tooltips, Help descriptions and dialog lines are
  sentences: "Path copied", "Repeat the file, or the waveform selection." No all-caps.
- **[Liam] Search matches are shown in Segoe UI Semibold** (`seguisb.ttf`), the text colour
  blended 12 % toward `bg`. Bold read "too bold and thick, too bright". If no semibold or bold
  face loads, matches turn `accent` blue instead.
- Leading spaces in names are not drawn (they'd indent the row); search still sees them.

## 6. Layout

Top to bottom. `░` is the `status_bg` frame; every line drawn inside the window is a
one-pixel black line:

```
┌ Menu bar ─ File  Search  Playback  View  Help ─────────────────┐  bg, margin 2×1
│░┌──────────────────────────────────────────────────────────┐░░│  search panel ░, margin L12 R12 T3 B4
│░│ >  query_                                                │░░│  box bg, black outline
│░└──────────────────────────────────────────────────────────┘░░│  left side on column L, right on A
│░┌────────────────┬───────────┬────────┬──────┬─────────────┐░░│
│░│ Name ^         │ Path      │ Length │ Size │ Date        │░░│  headings 22 px, blue, outlined
│▶├────────────────┼───────────┼────────┼──────┼─────────────┤▐░│  dividers; scroll bar 12 px, flush
│░│ row (19 px)    │           │        │      │             │▐░│
│░├──────────────────────────────────────────────────────────┤░░│  waveform top line (resize handle)
│░│ waveform, 56 px (drag top edge 32–400)                   │░░│  12 px ░ pads either side
│░└──────────────────────────────────────────────────────────┘░░│  waveform bottom line
└░░ status bar "12,345 objects"        ▶ 0:12 / 0:30   vol ░░░░░┘
   ↑ column L (12 px in)                    column A (window right − 12) ↑
```

**[Liam] Every line runs between column L and column A, never over the grey frame**
(2026-11-25). The gutter, the waveform's pads, the scroll column and the status bar are one
continuous grey with no line across them.

- **Column L** (`WAVE_PAD` = 12 px from the window's left): the search box's left side, the end of the Name gutter
  and the start of the waveform. One black line runs down it from the top of the headings to the
  bottom of the waveform.
- **Column A** (`list_border_x` = window right − `LIST_BAR_WIDTH` 12): the search box's right
  side, the last column's border and the waveform's right line. The scroll bar fills the 12 px
  to its right, and the waveform's right pad is the same 12 px.

- **Window:** opens 1100×680, minimum 420×240. Title is `query - Totems`, or `Totems` when
  the query is empty.
- **Columns:** Name 320 (left), Path 420 (left), Length 84 (right), Size 90 (right),
  Date Modified (rest, left). All resizable and clipped, never wrapped. Numbers are right-aligned.
- **Padding:** Name text starts 16 px in (`NAME_PAD`). The first 12 px are the grey gutter (up
  to column L), which holds the ▶ marker, so the marker never moves the name. Other cells have 6 px (`CELL_PAD`). Headings use the same padding as their
  column, so heading and cell text line up exactly.
- **Spacing:** item spacing 6×0, interact height 18. Tight on purpose.
- **Margins, all in px.** Menu bar 2×1. Menus and pop-outs 3×2. Buttons 4×1. Secondary entry
  boxes 4×1. Search panel L12 (= column L) R12 (= scroll bar width) T3 B4; search box L20 R6 T3 B3. Dialog windows L6 R8 T6 B6. Status bar 0×3. If you need a
  number that isn't here, start from the smallest one that still keeps text off the border.
- **Sort chevron:** a 1 px `dim` line chevron (8×3 px) centred on the top edge of the active
  heading, like Explorer. Clicking a heading sorts by it; clicking again reverses. Name and Path
  start ascending; Size and Date start descending.

## 7. Components

### Text entry boxes

**[Liam]** This is the component agents get wrong most often. A text entry box must look like a
box, and the box must sit tight round its text.

There are two kinds. Both follow the same rules. Only the size and fill differ.

| | Main search box | Secondary entry (Folders "…" box, any dialog field) |
|---|---|---|
| Text | 13.5 px Regular | **12 px Regular**, one size below body |
| Inner margin | L20 (room for `>`), R6, T3, B3 | **4×1** |
| Fill | `bg`, on the grey (`status_bg`) search panel | `status_bg`, so it reads as a box against the dialog |
| Outline, idle | **[Liam]** 1 physical px `header_border` (black), drawn by us, snapped | 1 px `dim` at 60 % |
| Outline, focused | the same black (the search box holds the caret almost always) | 1 px `sel_border` |
| Right edge | on column A, the list border's pixel column | wherever the dialog needs it |
| Corner radius | 0 | 1 |
| Width | Fills the panel | Fixed (74 px in Folders), sized so the window ends where Liam wants it |
| Hint | none (the `>` says "type here") | "…" only, never a sentence |

Rules for both:

1. **Text one size smaller than the content it filters or names.** 12 px in every entry box
   except the main search box. If you're unsure, go smaller.
2. **The border hugs the text.** Box height = text row + 2 px margin + 2 px outline, about the
   height of a list row (19 px). If there's visible empty space above or below the text, the
   margin is too big.
3. **A little room on the left.** Text never touches the left border: 4 px minimum (20 px in
   the search box, for the prompt).
4. **The caret fits inside.** 1 *physical* pixel wide, as tall as the text (13 px in the search
   box), never full line height. It must clear the top and bottom border.
5. **Focus shows on the border only** (secondary boxes): `sel_border` while typing. No glow, no
   thicker stroke, no fill change. The main search box stays black: it holds the caret nearly
   all the time, and the caret shows where typing goes.
6. **Fixed width for secondary boxes.** Don't let them stretch to fill the row. They set the
   window's width, so size them last, to the edge Liam marks.
7. **Put the padding on the `Frame`**, not on `TextEdit::margin()`: egui ignores the margin when
   you give it a custom frame (§9 trap 5), and the text ends up touching the border.

Code for a secondary entry box:

```rust
let focused = ui.memory(|m| m.focused()) == Some(id);
ui.add(
    egui::TextEdit::singleline(&mut text)
        .id(id)
        .hint_text("…")
        .font(FontId::proportional(12.0))
        .frame(
            egui::Frame::new()
                .fill(p.status_bg)
                .stroke(Stroke::new(1.0, if focused { p.sel_border } else { p.dim.gamma_multiply(0.6) }))
                .corner_radius(1)
                .inner_margin(egui::Margin::symmetric(4, 1)),
        )
        .desired_width(74.0),
);
```

### Buttons and icon squares

- **[Liam]** Text buttons: Semilight 12 px, padding **4×1** (`spacing.button_padding`). Use a
  text button only when no symbol says it (e.g. "Browse…").
- **[Liam]** Add and remove are **13 px squares** with a drawn **+** or **×** (`icon_square`):
  thin outline (`dim` at 60 %), no fill; on hover a `hover` fill, `dim` outline and `text`-coloured
  mark; pointing-hand cursor; a tooltip names the action ("Remove", "Add this folder"). Snap the
  square and the + bars to physical pixels so every line is one crisp pixel.
- Enter in an entry box does the same as its + square.

### Search box

Has the caret whenever nothing else needs the keyboard, like a launcher. Blue `>` prompt painted
7 px in, 13.5 px. Text starts 20 px in. **[Liam]** Caret: 1 *physical* pixel wide, 13 px tall,
blinks 0.5 s on / 0.5 s off, solid right after any move. egui's own caret is hidden and not
blinking (§9 trap 2). A pasted full path finds that file. The box sits on the grey search panel.
We paint its outline after the `TextEdit` (the frame keeps a transparent 1 pt stroke so the box
keeps its size), so the outline is one physical pixel and ends exactly on column A.

### List

Whole-row scrolling (§3.4). Selected row: `sel` fill across every cell, or `sel_inactive` when
the list doesn't have focus. The playing file shows a 9 px ▶ in `accent`, 2 px in, on the grey gutter
left of column L (painted after the gutter, so the gutter doesn't cover it). Mouse wheel moves 3 rows per notch. One blank spacer row after the last result lets the
last real row sit fully in view.

### Scroll bar

**[Liam]** Custom and always visible. **12 px** (`LIST_BAR_WIDTH` = `WAVE_PAD`): Liam widened it
from 8 on 2026-11-21 so its left edge meets the waveform's right line (column A). Still slim,
never a hover-only bar. It fills the window's last 12 px: flush against the black list border on
its left and the window edge on its right, no gaps. Other egui scroll areas (Folders) stay 8 px.
Track
`status_bg`, and **[Liam]** the scroll column beside the headings is the same grey, with no
line under it (2026-11-25). Handle `scroll_handle` with a pencil-thin `header_text` outline,
radius 1. **[Liam]** While the handle rests against either end (top or bottom of the list) its
outline turns a subtle red (`scroll_end`: `#C24A3E` dark, `#C42B1C` light) instead of blue:
that, not a line, shows where it stops. Handle at least 1.3× egui's minimum length so it stays
grabbable over a million rows. Pressing the track jumps the handle's centre to the pointer. It
moves by whole rows.

### Headings

`header_text` blue, 22 px tall, a one-physical-pixel `header_border` outline round each one,
drawn after the table (cells clip their own rect).

### Lines and borders

**[Liam]** One kind of line: **pencil thin (one physical pixel) and black** (`header_border`),
snapped to the pixel grid. If a line separates two parts of the window, it's this line.

- **Column dividers** carry the heading outlines straight down the body, in the **same pixel
  column** as the heading divider above. A divider that's off by one pixel next to its heading
  is a bug.
- **[Liam] Lines never light up.** Hovering or dragging a column divider or the waveform's top
  edge only changes the cursor (resize arrows); the line stays black. "The mouse icon change
  when hovering over the dividers is enough." egui's own resize lines highlight and are
  unsnapped; switch them off (§9 trap 13) and draw ours.
- **[Liam] Straight columns.** Column A (`list_border_x`): search box right side, last column
  border, waveform right line. Column L (`WAVE_PAD`): end of the Name gutter, start of the
  waveform. Anything new that ends near one of these ends *on* it, to the pixel.
- **Borders Liam asked for:** the search box outline; a column divider between the list and
  the scroll bar (the last heading's right edge carried down); and the waveform frame
  (`wave_frame`): its top edge (also the resize handle) and bottom edge, between columns L and A, a
  line between the wave and each pad, and the left one carried up to the top of the headings,
  between the gutter's ▶ marker and the name.
- **[Liam] One line between the search panel and the headings:** the headings' own top
  outline. egui's panel separator under the search panel is off (it doubled that line, and ran
  on over the grey gutter and scroll column, where grey meets grey and no line belongs).
- **[Liam] No line over the grey frame** (2026-11-25). The Name heading's outline starts on
  column L: its top and bottom lines don't cross the gutter, and the headings' lines don't
  cross the scroll column. The gutter is one unbroken grey strip from the search panel down.
- **[Liam] Edges are crisp at every scale** (checked at 100, 125 and 150 %). egui panel and
  frame fills end on fractional pixels, which blends a row (at 100 % the search box's and the
  headings' top lines read as two smudged pixels). So: the search box's fill is painted by us,
  in whole pixels exactly inside its outline (a `Shape::Noop` slot added before the
  `TextEdit`, filled after); the two rows above the headings' top line are repainted grey; and
  the rows either side of the menu bar / search panel edge are repainted `bg` / `status_bg`.
  A colour edge with no line must still change in one row. Check by sampling a pixel column of
  a screenshot, at 100 % as well as 150 %.
- **How to draw one:** `painter.vline` or `hline` with `Stroke::new(1.0 / ppp, p.header_border)`
  at a pixel centre: `((v * ppp).round() + 0.5) / ppp` for the pixel starting at `v`, `- 0.5`
  for the pixel ending at it. Draw it after the content it borders, so nothing covers it.
- No gridlines between rows. Vertical dividers only.

### Waveform strip

**[Liam]** 56 px by default; **drag its top edge** to make it 32–400 px tall (egui keeps the
height between runs). **[Liam]** 12 px pads on both sides (`WAVE_PAD`), `status_bg`, the same
grey as the status bar, so they read as part of the frame. A click in the left pad goes to 0:00,
in the right one to the end. Black 1 px lines frame the wave (see *Lines and borders*).
Min/max peak columns, one *point* wide each (at 150 % that's 1.5 physical pixels, so they
blur slightly; snapping them to physical pixels is an open item). **[Liam]** Only
the selection is coloured: inside it `accent` not yet played and `wave_played` grey once played, outside `wave_out`. Selection
edges get small white downward triangles ("crowns"). Playhead: 1 px line with a small orange
square crown. **[Liam]** The crown sits flush under the strip's top line: its fill starts on the
pixel row below the line, and the top side of its one-physical-pixel black outline *is* the line. Hidden while loading and once the sound has ended. Hover shows a
faint `dim` line. Cursor: text beam to select, grab inside a selection, resize while scrubbing.
The selection label (`0:01.200 – 0:02.500 (1.300 s)`, 11 px) sits top left on a translucent
`bg` chip.

### Status bar

`status_bg`, margin 0×3. Left: count ("12,345 objects"), or progress, or a notice (info 2 s in
`dim`, error 5 s in red). Right: player status (`▶ 0:12 / 0:30    48 kHz · 24-bit · Stereo ·
PCM`) then the volume. **[Liam]** When the window is too narrow for all of it, the format
goes first, then the times (the ▶ stays), and a long notice on the left ends in "…" before it
reaches the volume: status text never overlaps.

**Volume** (`volume_ui`):

- **Label**, in a 60 px slot right of the fader: `-∞ dB`, `0.0 dB`, `+3.5 dB`, 0.1 dB steps.
  **[Liam]** Above 0 dB it turns `BOOST` orange inside a one-physical-pixel `header_border`
  box hugging the text (4 px either side).
- **Fader**, 160 px, −∞ to **+24 dB** ([Liam]: for quiet files). Fader law `FADER`: 0 dB at
  70 % of the travel, finest round 0 dB, the bottom fifth −60 to −30. Audio glides 30 ms
  (`VOL_GLIDE`) so a drag flows.
- **[Liam] Cap:** 11×16 pt, a little bigger than egui's round handle, radius 1, egui's handle
  fill, one-physical-pixel outline in egui's handle stroke colour, and three pencil-thin vertical
  gripper lines 2 pt apart, like a fader cap. Laid out in physical pixels with an odd width
  so the middle gripper is centred. egui's own rail and handle are made transparent; we paint
  the groove (dark: egui's rail colour; light: `groove` with an outline) and the cap.
- Mouse wheel 0.5 dB a notch, double-click 0 dB, right-click a snug menu: a `dim` line "When
  boosting above 0 dB", then Soft Clip / Hard Clip.

**[Liam]** The ▶ ‖ ■ icon is painted (`paint_transport`), not a font glyph, so the three
match: all `TRANSPORT_H` (9 pt) tall, about the digits' height, the square a little smaller;
bars and square on whole pixels. The glyph ‖ was thin and short next to ▶. Clicking the player
status jumps the list to the playing file. While a new file loads, the old status stays up, so
nothing flashes.

### Menus and pop-outs

**[Liam]** The border sits snug round the text: `menu_margin` 3×2 px (egui's default 6 was too
roomy). Radius 2, widgets radius 1, `hover` fill. **[Liam]** Help > Search Syntax is a thin
pop-out, not a window: an `Area` hung under the Help menu in the menu frame, no title bar, two
side-by-side columns (Search | Keys and mouse) of 12 px rows: blue section label, key in `text`,
a 2–4 word description in `dim`. Esc or any click outside closes it. Keep its wording terse; it
must stay a small box. **[Liam]** "Reset Folders to Defaults" lives in the Help menu.

The menus, in order:

| Menu | Items |
|---|---|
| File | Folders…, Bookmarks…, Rebuild Index, Exit |
| Search | Match Case, Match Path, Enable Regex (checkboxes) |
| Playback | Autoplay on Select, Autoplay Next, Loop, Crossfade Loop (checkboxes) |
| View | Dark |
| Help | Search Syntax, Reset Folders to Defaults |

An item ending in "…" opens a window. A checkbox whose effect isn't obvious carries hover text
that says what it does and what "off" means ("Off: the loop repeats sample-exact"). When two
options can conflict, the hover text says which wins ("Loop wins").

### Dialogs (Folders, Bookmarks)

Plain egui window, not collapsible, not resizable, sized to its content (`auto_sized()`).
**[Liam]** Semilight 13 px title, centred over the whole window. One short `dim` 12 px line under
it ("Indexed"), then 4 px space. List entries in 12 px with a × square in front of each, rows
2 px apart, scrolling after 220 px. 6 px space, then one row: "Browse…" button, the "…" entry box
(74 px), the + square. **[Liam]** The window ends 9 px past the + square: a right inner margin
of 8 plus the 1 px border. No action buttons at the bottom; changes apply at once.

**[Liam]** Bookmarks is the Folders window's twin (same frame, header line "Ctrl + key", same
last row, `folder_entry_row`). Each row has a second 13 px square after the ×: the bookmark's
key digit (1–9, 0 for Ctrl+0; "·" in `dim` for none). Clicking it opens a snug menu of
Ctrl+1 … Ctrl+0 and None.

## 8. Interaction conventions

| Input | Behaviour |
|---|---|
| Type | Search updates on every keystroke; latest query wins. Typing (or paste, Backspace, Delete) while the list has focus goes back to the search box |
| ↑ ↓ PgUp PgDn, Ctrl+Home/End | Move selection; plays it if Autoplay is on. Home/End without Ctrl too, while the list has focus |
| Ctrl+↑ / Ctrl+↓ | Top of list / next file with a different name |
| Enter, double-click | Play from start (or the selection) |
| Space | Play / pause |
| ← → (Shift) | Seek 1 s (5 s) |
| Ctrl+← | Back to the start of the selected file (stays paused if paused), from the list or the search box |
| Esc | Closes Help, else stops, else clears the waveform selection, else back to search |
| Ctrl+1 … Ctrl+0 | Bookmarked folder into the search box, caret at the end |
| Tab | Toggle search ↔ list |
| Ctrl+F, Ctrl+L, F3 | Back to search, query selected |
| Ctrl+C | Copy full path (status bar confirms) |
| Ctrl+click Path cell | Open Explorer with the file selected and bring it to the front (pointing-hand cursor while Ctrl is held over it) |
| Right-click Path cell | Copy the folder path (status bar confirms) |
| Click status ▶ | Select and scroll to the playing file |
| Drag a row out | Copy the file to the drop target |
| Click waveform / its pads | Seek there / to 0:00 or the end. **[Liam]** Inside the selection it plays from there and keeps the selection (still stops or loops at its end); outside, it clears it |
| Drag on waveform | Select; plays the selection on release (under 3 px counts as a click) |
| Drag from inside a selection | Drag it out as a WAV, named `Name [0.5-1.7].wav` (one decimal, **[Liam]**) |
| Right-drag waveform | Scrub |
| Drag waveform's top edge | Resize the strip, 32–400 px |

Conventions behind the table:
- Confirm silent actions in the status bar (copied path, export errors). Never use a modal for
  information.
- Cursors tell you what a click will do (pointing hand, text beam, grab, resize).
- A toggle that changes how clicks behave (Autoplay) has hover text that names both modes.
- An action that opens another app (Explorer) must bring that app's window to the front. Liam
  flagged one that opened behind Totems.

## 9. Building it in egui 0.37 (traps we hit)

```rust
ctx.all_styles_mut(|s| {
    s.spacing.item_spacing = Vec2::new(6.0, 0.0);
    s.spacing.interact_size.y = 18.0;
    s.spacing.menu_margin = egui::Margin::symmetric(3, 2);   // egui: 6
    s.spacing.button_padding = Vec2::new(4.0, 1.0);          // egui: 8×2
    let light = FontFamily::Name("light".into());             // Segoe UI Semilight
    s.text_styles.insert(TextStyle::Button, FontId::new(12.0, light.clone()));
    s.text_styles.insert(TextStyle::Heading, FontId::new(13.0, light));
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
9. **egui centres a window title between the left edge and the × button**, so it sits left of
   centre. Put an invisible square spacer atom (as wide as the ×, one heading row high) before
   the title to balance it.
10. **Never touch the style inside `ctx.fonts_mut`.** Calling `ctx.global_style()` (or anything
    else that locks the context) inside the closure deadlocks the UI thread. Opening Folders
    froze the app this way. Resolve the `FontId` first, then call `fonts_mut`.
11. **`resizable(false)` doesn't size a window to its content.** It keeps egui's 320 px
    default width. Use `auto_sized()`.
12. **egui remembers window sizes by id.** After you change a window's layout, give it a new id
    (`"folders-v2"`), or the size saved from the old version comes back.
13. **egui_extras column resize lines and egui panel resize edges** use
    `widgets.{noninteractive,hovered,active}.bg_stroke` (table) and
    `noninteractive.bg_stroke` / `hovered.fg_stroke` / `active.fg_stroke` (panel), unsnapped,
    at 1 pt (a 2 px blur at 150 %), and the table's sit a pixel off the heading outlines. Set
    them to `Stroke::NONE` on the table's child `Ui` (or around the panel's `show`, then restore
    the style), and paint snapped lines yourself. (If a line ever needs hover/drag state, read it with
    `ctx.read_response(id)`: table `scope_id().with(IdSalt::new(salt)).with("resize_column").with(i)`,
    panel `scope_id().with_salt(IdSalt::new(salt)).with("__resize")`.)

## 10. Things Liam rejected (don't bring back)

- MS Shell Dlg / Tahoma / Microsoft Sans Serif: "too blobby and thick".
- A scroll bar that only appears on hover, or one wider than it needs to be to line up. (It
  was 8 px; Liam asked for 12 on 2026-11-21 so it meets the waveform's right line.)
- egui's default 2 px, full-line-height caret, and any caret that touches the box border.
- Text entry boxes with egui's default frame: roomy padding, body-size text, or no visible box.
- Prose in an entry box hint (use "…").
- A big titled Help window with long descriptions (2026-11-20: "not working for me visually").
- "Remove" and "Add" text buttons, wide button padding, and bold or oversized button and title
  text (2026-11-21).
- Bold search matches ("too bold and thick, too bright").
- Dialogs wider than their content, or with a "Reset to Defaults" button inside them.
- Rows that look different from each other (the fade, stripes).
- Divider and edge lines that light up on hover or drag (2026-11-21): the cursor is enough.
- Grey or soft column dividers, and hover lines that are thicker than, or off by a pixel from,
  the heading dividers (2026-11-21).
- A lighter grey pad left of the waveform (it now matches the status bar).
- A blue or grey search box outline (2026-11-21: "make the border around the search bar pencil
  thin black").
- Lines that nearly line up: a waveform edge 4 px off the list border, a search box one pixel
  short of it, a scroll bar with a gap beside it.
- A playhead crown floating below the waveform's top line.
- ▶ flashing for a file that ends or fails at once.

## 11. How to work with Liam on UI

1. Change one visible thing at a time and show it in the running app. On Windows, run
   `cargo run --release` and look at it. Screenshots beat descriptions.
2. Check the screenshot against §2 before sending it. If any item fails, fix it first.
3. When he marks a screenshot (red arrows, a red line, a green box for "I like this"), match the
   mark to the pixel. A red line is an edge to hit; green means don't change it.
4. Before changing anything marked **[Liam]**, ask. Those are his decisions, not defaults.
5. When he reports a visual glitch, find the cause in the framework source before working
   around it. The dim top row was egui's fade, not our colours.
6. Keep the reply short: what changed, how to see it, one next step. He reads in ADHD mode.
   Give full absolute paths for any file he should open.
7. Record new decisions here, marked **[Liam]**, in the same commit as the code.
