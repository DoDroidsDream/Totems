# Totems

Instant search and preview for every audio file on the machine (Rust, egui, symphonia, cpal).
Two features only: **search** and **playback**. Both must be as fast as possible. Windows is
the shipping target.

Read `docs/ARCHITECTURE.md` before changing anything. It has the module map, the thread model,
the invariants that must never break (bit-exact playback at 0 dB, the click-free no-handshake
switch, nothing in the audio callback that can cause a dropout, the cache format magic) and the
measured performance budgets.

## Commands

```sh
cargo build --release              # target/release/totems(.exe)
cargo run --release                # build and launch
cargo test --release               # unit + engine tests (no sound card needed)
cargo clippy --release             # lints
```

Benchmarks are `#[ignore]` tests; see the budgets table in `docs/ARCHITECTURE.md`, for example
`cargo test --release bench_1m -- --ignored --nocapture`.

Always build and measure with `--release`. Dependencies are optimised even in debug, but the
app's own code isn't.

## Environment notes

- **On Windows (the normal case):** verify UI changes by running the real app
  (`cargo run --release`): drag-out, audio device and file dialogs are only fully testable
  here. Optional tools for stress testing: `ffmpeg` and Python with numpy, to generate test
  audio.
- **On Linux:** needs `libasound2-dev` to build. Check the Windows build with
  `cargo build --release --target x86_64-pc-windows-gnu` (mingw-w64), and run the GUI under
  Xvfb + openbox with a PulseAudio null sink.

## Project skills (`.claude/skills/`)

- `audio-bug-hunter`: stress-tests and reports verified bugs; fixes only trivial ones.
- `audio-engine-builder`: implements features and fixes, with a definition of done.
- `audio-perf-reviewer`: measured performance review, plus correctness and style notes.

The usual flow is hunter → builder → reviewer: every bug gets a regression test, every change
gets numbers.

## Conventions

- Keep `docs/ARCHITECTURE.md` and `README.md` current in the same commit when invariants,
  budgets or user-visible behaviour change.
- Commit messages say what changed and include measured numbers where relevant.
