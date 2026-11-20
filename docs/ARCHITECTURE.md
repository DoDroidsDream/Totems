# Totems architecture

Shared reference for anyone (human or agent) changing, reviewing or testing Totems.
Keep this file current: when an invariant or a performance budget changes, update it in the
same commit.

Totems does two things: **search** every audio file on the machine, and **play** the selected
one, both as fast as possible. Windows is the shipping target. Linux builds exist for
development and CI. Drag-out is Windows-only.

## Modules

| File | Role |
|---|---|
| `src/index.rs` | In-memory database: columnar, name-sorted entries; rank tables; cache file (`TOTEMIX6`); filesystem scan (`jwalk`; size/mtime come from a per-directory `std::fs::read_dir` re-list, free on Windows, never a per-file stat) |
| `src/indexer.rs` | Index lifecycle: cache load → background rescan → live updates via `notify`, batched and debounced |
| `src/query.rs` | Query language (lexer, parser, AST) and the parallel search engine |
| `src/decode.rs` | `Source`: symphonia wrapper: open, decode to interleaved f32, sample-accurate seek |
| `src/dsp.rs` | Channel mapping/downmix and the streaming Kaiser-windowed sinc resampler |
| `src/player.rs` | Preview engine: decoder thread, lock-free ring buffer, markers, audio callback, volume/clip |
| `src/workers.rs` | Background header probes (Length column) and waveform peak computation |
| `src/region.rs` | Export a waveform selection to WAV; cleanup of exported files |
| `src/app.rs` | egui UI: search box, table, waveform, status bar, keyboard, drag-out |

## Threads

| Thread | Work | Rules |
|---|---|---|
| UI (main) | egui frame; sends requests, never waits on them | No disk I/O or decoding per frame. Exceptions: drag-out (OS modal loop) and region export at drag start |
| `search` | parse + search + sort | Latest request wins; stale queries are dropped |
| `indexer` | cache load, full scan, apply file-system change batches | Publishes a whole new `Arc<Index>`; never mutates a published one. The watcher is armed before a scan starts, so changes during it are applied afterwards. Batches apply after 200 ms of quiet, or 2 s after their first change at the latest. A root that disappears is polled every 2 s and rescanned (and watched again) when it comes back |
| `decoder` | decode → map channels → resample → push to ring | Keeps ~250 ms queued; coalesces commands (only the last Load matters) |
| audio callback (cpal) | `Renderer::render` + `apply_volume` | **Real-time**: no allocation (scratch is preallocated for 0.5 s; growing is only a fallback), no locks, no syscalls, no logging, no unbounded loops. Never wakes the UI: while playing, the UI schedules its next frame no later than the sound's end (`Player::time_left`) |
| `info0..3` | header probes for visible rows | LIFO, queue capped at 256 |
| `waveform` | full decode to min/max peaks | Latest wins; cancels mid-file when a newer request arrives |

## Data layout (index)

- Entries are sorted by `(lowercase name, lowercase dir, name)`. Row order = Name sort order.
- `names` and `lower` are concatenated name buffers, each name terminated by `\0`;
  `name_off` / `lower_off` have `n + 1` entries.
- `dir[i]` indexes `dirs` / `dirs_lower` (`dirs_lower` uses `/` separators everywhere).
- `rank_path`, `rank_size` and `rank_mtime` are permutation ranks, so sorting results is an
  integer sort.
- `lower` / `dirs_lower` are case-folded one character at a time (`index::lower`; final ς folds
  to σ), never with context-dependent `str::to_lowercase`; queries fold the same way.
- Queries nest at most 64 `< >` deep and runs of `!` fold without recursion: parsing, evaluating
  and dropping a query recurse per level on the search thread's 2 MiB stack.
- The search prefilter scans `lower` with `memmem` over 16k-entry chunks in parallel, then
  evaluates the full AST on candidates.
- Cache: column-wise binary with magic `TOTEMIX6`. **Bump the magic whenever the layout
  changes, or when the rules for what gets indexed change** (so a stale cache can't bring
  excluded files back).
- **What gets indexed:** `audio_ext_id` is the single gate for full scans, folder rescans and
  live updates. It rejects macOS AppleDouble files (`._name.wav`: Finder metadata with an
  audio extension). `skip_dir` skips system folders and `__MACOSX`. `Index::load` validates
  structure, so a corrupt cache can never panic; it is rejected instead (including name offsets
  that don't start at 0: `name()` trusts them to fall on UTF-8 boundaries).
- **Case:** paths compare case-insensitively (Windows), but `Index::find` prefers an exact-case
  match, because case-sensitive folders can hold `kick.wav` and `Kick.wav`. Live updates
  resolve each event path to its on-disk spelling (Windows reports a case-only rename with
  both spellings, and both exist) and remove entries by id.
- Tests use a per-process cache file in the temp folder (`cache_path` under `cfg(test)`), never
  the user's.

## Playback engine invariants

- **Generations:** every Load, Seek and Stop gets a new, strictly increasing generation number.
  The UI and the callback compare generations; they never compare paths.
- **Shown as playing only once audio has started:** `Player::state()` says Playing from the
  moment `play()` is called, which drives logic and repaints. Displays (list ▶, playhead,
  status text) also wait for `Player::loading()` to clear: the callback has reached the Load's
  `Start` (`cur_gen`) *and* the decoder has queued at least 0.1 s of audio for it (`fed_gen`,
  set by the decoder thread only, `SHOWN_AFTER`). An empty, unreadable or very short file
  therefore never flashes ▶ (`instant_end_never_shows_playing`). Seeks keep the Load's generation, so they never blank
  the display.
- **Markers:** `Start { at }` / `End { at }` are posted in-band after the samples they refer
  to. A switch never waits on a handshake. The callback fades the old sound out over 5 ms,
  skips to `at`, and starts the new sound on the next sample.
- **No fade on a sound's own start:** a Load at 0 starts at full gain, preserving the attack.
  Seeks fade in over 5 ms.
- `End` is posted only after every sample of the sound has been queued.
- **Bit-exact:** when the source rate equals the device rate and the volume is exactly 0 dB,
  output samples equal the decoded samples (`bit_exact_playback_and_end` test).
- **Volume:** 10 ms one-pole glide toward the target. Soft clip (knee at −1 dBFS, never above
  1.0) or hard clip, applied only when gain > 1.0.
- **Range playback:** stops sample-accurately at the end with a 5 ms fade-out.
- **Resampler:** Kaiser β = 9, 24 zero crossings, cutoff 0.94 × the lower Nyquist. Rate pairs
  whose exact polyphase table needs ≤ 1024 phases (every common pair: 44.1→48 kHz has 160) use
  it directly; others interpolate linearly between 512 phases. Kernels are built once per rate
  pair and cached; the decoder prebuilds 44.1/48/88.2/96 kHz → device rate when the output
  opens, so no Load pays for a table. History is kept per channel and the dot product is
  vectorised (AVX2 when the CPU has it).

## Region export invariants

- The output is sample-exact and keeps the source's rate and channel count.
  16/24-bit sources round-trip bit-exactly; float sources stay float; lossy sources become
  24-bit. `WAVE_FORMAT_EXTENSIBLE` is used for more than 2 channels or more than 16 bits.
- Files go to `%LOCALAPPDATA%\Totems\Regions\<hash>\`. They're deleted on exit and at the
  next launch, with a 2-minute grace period. Files held open are skipped and retried.

## Performance budgets (measured; don't regress without a reason)

Measured on 4 cores in Linux CI; Windows desktops are typically faster.

| Operation | Budget | How to measure |
|---|---|---|
| Search, 1M files, typical query | ≤ 5 ms | `cargo test --release bench_1m -- --ignored --nocapture` |
| Search, 1M files, regex worst case | ≤ 40 ms | same |
| Re-sort 1M results by size | ≤ 25 ms | same |
| Cache load, 1M files | ≈ 100 ms | same |
| Index build, 1M files (background) | ≈ 1.3 s | same |
| Live update, 1M files, one file deleted | ≈ 150 ms (was 300 ms) | `cargo test --release bench_apply_changes -- --ignored --nocapture` |
| Scan 200k files / 32k dirs (warm) | ≈ 100 ms | `TOTEMS_SCAN=/ cargo test --release scan_root -- --ignored --nocapture` |
| Scan 79k audio files / 3.3k dirs (warm, Windows, SATA SSD) | ≈ 45 ms | `TOTEMS_SCAN='F:\Samples' cargo test --release scan_root -- --ignored --nocapture` |
| Open to first decoded audio | ≤ 3 ms | `TOTEMS_AUDIO_DIR=… cargo test --release decode_dir -- --ignored --nocapture` |
| Keypress to sound | 1 device buffer (~10 ms) | `switching_is_click_free_and_immediate` |
| Resampler, 44.1→48 kHz stereo | ≤ 1 ms per second of audio (was 10 ms) | `cargo test --release bench_resampler -- --ignored --nocapture` |
| Resampler setup per Load | ≤ 1 µs (cached kernel; cold 44.1→48 kHz build 0.36 ms, was 1.1 ms on every Load) | `cargo test --release bench_resampler_new -- --ignored --nocapture` |
| Audio callback, 480-frame stereo buffer | ≈ 5 µs | `cargo test --release bench_render -- --ignored --nocapture` |
| Region export (warm) | ≤ 3 ms for SFX-length ranges | `TOTEMS_AUDIO_DIR=… TOTEMS_OUT=… cargo test --release export_dir -- --ignored --nocapture` |
| Idle CPU | ~0 (UI is reactive) | |
| Decoder thread while playing | ~1 % of a core | |

## Verify every change

```sh
cargo test --release      # unit + engine tests
cargo clippy --release    # lints
cargo build --release     # on Windows: the shipping build itself
# on Linux, also check that the shipping target compiles:
cargo build --release --target x86_64-pc-windows-gnu
```

The engine tests drive the decoder thread and the `Renderer` directly, with no sound card.
GUI checks: on Windows, run the app (`cargo run --release`). On Linux, run under Xvfb with a
window manager (openbox) and a PulseAudio null sink.
