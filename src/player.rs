//! Low-latency preview engine.
//!
//! A decoder thread streams PCM into a lock-free ring buffer that the audio
//! callback drains. Switching sounds never waits on a handshake: the decoder
//! appends new audio right after the old and posts a marker saying where the
//! new sound begins. The callback fades the old sound out over ~5 ms, skips
//! to the marker and starts the new sound on the very next sample, so the
//! time from keypress to sound is one audio buffer.

use crate::decode::{Info, Source};
use crate::dsp::{Resampler, map_channels};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
enum Marker {
    /// New sound `generation` starts once `at` samples have been consumed.
    Start { generation: u64, at: u64, base: f64, fade_in: bool },
    /// Sound `generation` ends once `at` samples have been consumed.
    End { generation: u64, at: u64 },
}

/// State published by the audio callback for the UI.
#[derive(Default)]
pub struct Shared {
    pub cur_gen: AtomicU64,
    pub frames_out: AtomicU64,
    pub base_bits: AtomicU64,
    pub finished_gen: AtomicU64,
    /// Latest generation the decoder has queued at least `SHOWN_AFTER` of
    /// audio for. Set by the decoder thread, read by the UI only (the
    /// callback never touches it).
    pub fed_gen: AtomicU64,
    pub paused: AtomicBool,
    pub dead: AtomicBool,
    /// Which output stream is current; an error from an older one (its
    /// callback can still fire after it was replaced) is ignored.
    pub stream_id: AtomicU64,
    pub error: Mutex<Option<(u64, String)>>,
    pub info: Mutex<Option<(u64, Info)>>,
    /// Output volume as linear gain (f32 bits).
    pub volume: Volume,
    /// Soft-clip (true) or hard-clip (false) when boosting above 0 dB.
    pub soft_clip: AtomicBool,
}

pub struct Volume(AtomicU32);

impl Default for Volume {
    fn default() -> Self {
        Self(AtomicU32::new(1f32.to_bits()))
    }
}

impl Volume {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    pub fn set(&self, g: f32) {
        self.0.store(g.to_bits(), Relaxed)
    }
}

/// Lowest slider position; treated as silence (-inf dB).
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 12.0;

pub fn db_to_gain(db: f32) -> f32 {
    if db.is_nan() {
        return 1.0; // a NaN gain would stick in the callback's glide
    }
    let db = db.min(MAX_DB);
    if db <= MIN_DB { 0.0 } else if db == 0.0 { 1.0 } else { 10f32.powf(db / 20.0) }
}

/// Soft knee starting at -1 dBFS: identical to the input below the knee,
/// smooth (C1-continuous) above it, and never exceeding full scale.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const T: f32 = 0.891_250_9; // -1 dBFS
    let a = x.abs();
    if a <= T {
        x
    } else {
        (T + (1.0 - T) * ((a - T) / (1.0 - T)).tanh()).copysign(x)
    }
}

enum Cmd {
    /// Play `path` from `start`; with `end`, stop there (with a short fade).
    Load { path: PathBuf, generation: u64, start: f64, end: Option<f64> },
    Seek { generation: u64, secs: f64 },
    Stop { generation: u64 },
    Output(Box<Output>),
}

struct Output {
    data: rtrb::Producer<f32>,
    markers: rtrb::Producer<Marker>,
    rate: u32,
    ch: usize,
}

pub struct Player {
    stream: Option<cpal::Stream>,
    tx: Sender<Cmd>,
    pub shared: Arc<Shared>,
    pub out_rate: u32,
    next_gen: u64,
    /// Generation the UI last requested, with the file and its duration.
    pub current: Option<Current>,
    pub device_error: Option<String>,
    reopened_at: Option<std::time::Instant>,
}

#[derive(Clone)]
pub struct Current {
    pub generation: u64,
    pub path: PathBuf,
    pub requested_pos: f64,
    /// Generation of the Load that opened this file (seeks keep it).
    pub load_gen: u64,
    /// Range end in seconds while auditioning a selection (a seek clears it).
    pub end: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle,
    Playing,
    Paused,
    Ended,
}

impl Player {
    pub fn new(repaint: Arc<dyn Fn() + Send + Sync>) -> Player {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared::default());
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("decoder".into())
                .spawn(move || decoder_thread(rx, shared, repaint))
                .expect("spawn decoder");
        }
        let mut p = Player {
            stream: None,
            tx,
            shared,
            out_rate: 48000,
            next_gen: 1,
            current: None,
            device_error: None,
            reopened_at: None,
        };
        p.open_output();
        p
    }

    fn open_output(&mut self) {
        self.stream = None;
        self.shared.stream_id.fetch_add(1, SeqCst);
        self.shared.dead.store(false, SeqCst);
        match build_output(self.shared.clone()) {
            Ok((stream, out)) => {
                self.out_rate = out.rate;
                self.stream = Some(stream);
                self.device_error = None;
                let _ = self.tx.send(Cmd::Output(Box::new(out)));
            }
            Err(e) => self.device_error = Some(e),
        }
    }

    /// Call once per UI frame. If the output device died (unplugged, or the
    /// driver reset), reopen the default device and carry on from the same
    /// spot, rather than showing ▶ with a frozen playhead forever.
    pub fn poll(&mut self) {
        if self.stream.is_none() || !self.shared.dead.load(SeqCst) {
            return;
        }
        // A device that keeps failing is retried once a second, not per frame.
        if self.reopened_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.reopened_at = Some(std::time::Instant::now());
        let state = self.state();
        let resume = matches!(state, State::Playing | State::Paused).then(|| self.current.clone()).flatten();
        let pos = self.position();
        self.open_output();
        if let Some(c) = resume
            && self.stream.is_some()
        {
            let end = c.end.filter(|e| *e > pos);
            self.play_range(c.path, pos, end);
            // Paused stays paused, ready to go on from the same spot.
            self.shared.paused.store(state == State::Paused, SeqCst);
        }
    }

    fn ensure_output(&mut self) {
        if self.stream.is_none() || self.shared.dead.load(SeqCst) {
            self.open_output();
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.next_gen += 1;
        self.next_gen
    }

    pub fn play(&mut self, path: PathBuf, start: f64) {
        self.play_range(path, start, None);
    }

    /// Play from `start`, stopping at `end` if given (auditioning a selection).
    pub fn play_range(&mut self, path: PathBuf, start: f64, end: Option<f64>) {
        self.ensure_output();
        let generation = self.next_generation();
        self.shared.paused.store(false, SeqCst);
        self.current = Some(Current { generation, path: path.clone(), requested_pos: start, load_gen: generation, end });
        let _ = self.tx.send(Cmd::Load { path, generation, start, end });
    }

    pub fn seek(&mut self, secs: f64) {
        let Some(cur) = self.current.clone() else { return };
        let secs = secs.max(0.0);
        if self.state() == State::Ended || self.state() == State::Idle {
            self.play(cur.path, secs);
            return;
        }
        let generation = self.next_generation();
        self.current = Some(Current { generation, path: cur.path, requested_pos: secs, load_gen: cur.load_gen, end: None });
        let _ = self.tx.send(Cmd::Seek { generation, secs });
    }

    pub fn stop(&mut self) {
        let generation = self.next_generation();
        if let Some(c) = &mut self.current {
            c.generation = generation;
            c.requested_pos = 0.0;
        }
        self.shared.paused.store(false, SeqCst);
        let _ = self.tx.send(Cmd::Stop { generation });
        self.current = None;
    }

    /// Volume in dB; `MIN_DB` or below is silence.
    pub fn set_volume_db(&self, db: f32) {
        self.shared.volume.set(db_to_gain(db));
    }

    pub fn set_soft_clip(&self, soft: bool) {
        self.shared.soft_clip.store(soft, Relaxed);
    }

    pub fn toggle_pause(&mut self) {
        match self.state() {
            State::Playing => self.shared.paused.store(true, SeqCst),
            State::Paused => self.shared.paused.store(false, SeqCst),
            State::Ended => {
                if let Some(c) = self.current.clone() {
                    self.play(c.path, 0.0);
                }
            }
            State::Idle => {}
        }
    }

    pub fn state(&self) -> State {
        state_of(&self.shared, self.current.as_ref(), self.stream.is_some())
    }

    /// A file was asked to play but its audio hasn't started yet (the
    /// decoder is opening it and has queued no audio yet, or the callback
    /// hasn't reached its Start marker). `state()` already says Playing; displays wait for this to
    /// clear so a file that ends or fails at once never flashes ▶.
    pub fn loading(&self) -> bool {
        loading_of(&self.shared, self.current.as_ref(), self.stream.is_some())
    }

    /// Current playback position in seconds.
    pub fn position(&self) -> f64 {
        let Some(c) = &self.current else { return 0.0 };
        if self.shared.cur_gen.load(Acquire) != c.generation {
            return c.requested_pos;
        }
        let base = f64::from_bits(self.shared.base_bits.load(Acquire));
        base + self.shared.frames_out.load(Acquire) as f64 / self.out_rate as f64
    }

    /// Seconds until the current sound (or range) ends, if its length is
    /// known. The UI wakes itself then: the audio callback never asks for a
    /// repaint (that takes egui's context lock and posts a window message).
    pub fn time_left(&self) -> Option<f64> {
        let end = self.current.as_ref()?.end.or(self.info()?.duration)?;
        Some(time_left(self.position(), end))
    }

    pub fn info(&self) -> Option<Info> {
        let c = self.current.as_ref()?;
        let g = self.shared.info.lock();
        g.as_ref().filter(|(generation, _)| *generation == c.generation).map(|(_, i)| i.clone())
    }

    pub fn error(&self) -> Option<String> {
        let c = self.current.as_ref()?;
        let g = self.shared.error.lock();
        g.as_ref().filter(|(generation, _)| *generation == c.generation).map(|(_, e)| e.clone())
    }
}

fn time_left(pos: f64, end: f64) -> f64 {
    (end - pos).max(0.0)
}

#[cfg(test)]
#[test]
fn time_left_counts_down_to_zero() {
    assert_eq!(time_left(1.0, 3.5), 2.5);
    assert_eq!(time_left(4.0, 3.5), 0.0);
}

fn state_of(shared: &Shared, cur: Option<&Current>, has_output: bool) -> State {
    let Some(c) = cur else { return State::Idle };
    if !has_output {
        return State::Idle;
    }
    if shared.finished_gen.load(Acquire) == c.generation
        || shared.error.lock().as_ref().is_some_and(|(g, _)| *g == c.generation)
    {
        return State::Ended;
    }
    if shared.paused.load(Relaxed) { State::Paused } else { State::Playing }
}

/// A sound is shown as playing only once this much of it is queued: anything
/// shorter is over before the UI could show it, and would only flash ▶.
const SHOWN_AFTER: f64 = 0.1;

fn loading_of(shared: &Shared, cur: Option<&Current>, has_output: bool) -> bool {
    let Some(c) = cur else { return false };
    // The Start marker is queued before the file is even opened, so also
    // wait for real audio: an empty, broken or very short file never gets
    // enough.
    state_of(shared, cur, has_output) == State::Playing
        && (shared.cur_gen.load(Acquire) < c.load_gen || shared.fed_gen.load(Acquire) < c.load_gen)
}

// --------------------------------------------------------------- output

fn build_output(shared: Arc<Shared>) -> Result<(cpal::Stream, Output), String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("No audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let rate = supported.sample_rate();
    let ch = supported.channels() as usize;
    let fmt = supported.sample_format();
    // Small fixed buffer for low latency where the device allows it.
    let want = (rate / 100).next_power_of_two().clamp(256, 1024); // ~10 ms
    let buffer_size = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } if want >= *min && want <= *max => {
            cpal::BufferSize::Fixed(want)
        }
        _ => cpal::BufferSize::Default,
    };
    let cap = rate as usize * ch * 2;
    let make = |buffer_size| -> Result<(cpal::Stream, Output), String> {
        let (dp, dc) = rtrb::RingBuffer::new(cap);
        let (mp, mc) = rtrb::RingBuffer::new(256);
        let renderer = Renderer {
            data: dc,
            markers: mc,
            ch,
            gain: 0.0,
            step: 1.0 / (rate as f32 * 0.005),
            popped: 0,
            active: false,
            shared: shared.clone(),
            scratch: vec![0.0; scratch_len(rate, ch)],
            vol: shared.volume.get(),
            vol_coef: 1.0 - (-1.0 / (rate as f32 * 0.01)).exp(), // ~10 ms glide
        };
        let config = cpal::StreamConfig { channels: ch as u16, sample_rate: rate, buffer_size };
        let stream = match fmt {
            cpal::SampleFormat::F32 => run::<f32>(&device, config, renderer, shared.clone()),
            cpal::SampleFormat::I16 => run::<i16>(&device, config, renderer, shared.clone()),
            cpal::SampleFormat::I32 => run::<i32>(&device, config, renderer, shared.clone()),
            cpal::SampleFormat::U16 => run::<u16>(&device, config, renderer, shared.clone()),
            cpal::SampleFormat::F64 => run::<f64>(&device, config, renderer, shared.clone()),
            cpal::SampleFormat::I24 => run::<cpal::I24>(&device, config, renderer, shared.clone()),
            f => Err(format!("Unsupported device format {f}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        Ok((stream, Output { data: dp, markers: mp, rate, ch }))
    };
    match make(buffer_size) {
        Ok(v) => Ok(v),
        Err(_) if buffer_size != cpal::BufferSize::Default => make(cpal::BufferSize::Default),
        Err(e) => Err(e),
    }
}

/// Callback scratch size in samples: half a second, far above the 1024-frame
/// fixed buffer we ask for and any driver default period.
fn scratch_len(rate: u32, ch: usize) -> usize {
    (rate as usize / 2).max(4096) * ch
}

#[cfg(test)]
#[test]
fn scratch_covers_every_buffer_we_request() {
    for rate in [8000, 22050, 44100, 48000, 96000, 192000, 384000] {
        for ch in 1..=8 {
            assert!(scratch_len(rate, ch) >= 1024 * ch, "{rate} Hz x {ch}");
        }
    }
}

/// The stream `id` reported a fatal error: mark the output dead, unless a
/// newer stream has replaced it (then a second, needless restart would
/// follow).
fn stream_failed(shared: &Shared, id: u64) {
    if shared.stream_id.load(SeqCst) == id {
        shared.dead.store(true, SeqCst);
    }
}

#[cfg(test)]
#[test]
fn stale_stream_errors_are_ignored() {
    let shared = Shared::default();
    shared.stream_id.store(5, SeqCst);
    stream_failed(&shared, 4);
    assert!(!shared.dead.load(SeqCst), "an old stream's error killed the new one");
    stream_failed(&shared, 5);
    assert!(shared.dead.load(SeqCst));
}

fn run<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut r: Renderer,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    let id = shared.stream_id.load(SeqCst);
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                // Preallocated for 0.5 s; growing (an allocation on the
                // real-time thread) is only a fallback for odd drivers.
                if r.scratch.len() < out.len() {
                    r.scratch.resize(out.len(), 0.0);
                }
                let mut buf = std::mem::take(&mut r.scratch);
                r.render(&mut buf[..out.len()]);
                for (o, s) in out.iter_mut().zip(&buf) {
                    *o = T::from_sample(*s);
                }
                r.scratch = buf;
            },
            move |e: cpal::Error| {
                use cpal::ErrorKind::*;
                if !matches!(e.kind(), Xrun | RealtimeDenied) {
                    stream_failed(&shared, id);
                }
            },
            None,
        )
        .map_err(|e| e.to_string())
}

struct Renderer {
    data: rtrb::Consumer<f32>,
    markers: rtrb::Consumer<Marker>,
    ch: usize,
    gain: f32,
    step: f32,
    popped: u64,
    active: bool,
    shared: Arc<Shared>,
    scratch: Vec<f32>,
    /// Smoothed output volume and its one-pole coefficient.
    vol: f32,
    vol_coef: f32,
}

impl Renderer {
    /// Volume and clipping, after all other processing. At exactly 0 dB this
    /// leaves samples untouched (bit-exact output).
    fn apply_volume(&mut self, out: &mut [f32]) {
        let target = self.shared.volume.get();
        if self.vol == 1.0 && target == 1.0 {
            return; // 0 dB: untouched
        }
        let soft = self.shared.soft_clip.load(Relaxed);
        let ch = self.ch;
        for frame in out.chunks_exact_mut(ch) {
            if self.vol != target {
                self.vol += (target - self.vol) * self.vol_coef;
                if (self.vol - target).abs() < 1e-5 {
                    self.vol = target;
                }
            }
            let v = self.vol;
            if v == 1.0 {
                continue;
            }
            if v > 1.0 && soft {
                for s in frame.iter_mut() {
                    *s = soft_clip(*s * v);
                }
            } else {
                for s in frame.iter_mut() {
                    *s = (*s * v).clamp(-1.0, 1.0);
                }
            }
        }
    }

    fn render(&mut self, out: &mut [f32]) {
        let ch = self.ch;
        let paused = self.shared.paused.load(Relaxed);
        let mut frames_out = 0u64;
        let frames = out.len() / ch;
        let mut f = 0;
        while f < frames {
            // Steady playback at full gain: copy whole runs (bit-exact; the
            // gain is exactly 1). The fill level is read before the markers
            // are peeked: the decoder pushes a marker before the audio after
            // it, so with none visible, none applies within these frames.
            if self.active && !paused && self.gain == 1.0 {
                let avail = self.data.slots() / ch;
                if avail > 0
                    && self.markers.peek().is_err()
                    && let Ok(chunk) = self.data.read_chunk(avail.min(frames - f) * ch)
                {
                    let n = chunk.len() / ch;
                    let (a, b) = chunk.as_slices();
                    let dst = &mut out[f * ch..(f + n) * ch];
                    dst[..a.len()].copy_from_slice(a);
                    dst[a.len()..].copy_from_slice(b);
                    chunk.commit_all();
                    self.popped += (n * ch) as u64;
                    frames_out += n as u64;
                    f += n;
                    continue;
                }
            }
            self.render_frame(&mut out[f * ch..(f + 1) * ch], paused, &mut frames_out);
            f += 1;
        }
        self.shared.frames_out.fetch_add(frames_out, Release);
        self.apply_volume(out);
    }

    /// One frame through markers, fades and switches.
    fn render_frame(&mut self, frame: &mut [f32], paused: bool, frames_out: &mut u64) {
        let ch = self.ch;
        // Apply any markers we've reached; note if a switch is pending.
        let mut switch_at = None;
        loop {
            match self.markers.peek().ok().copied() {
                Some(Marker::Start { generation, at, base, fade_in }) => {
                    if self.popped >= at || (!self.active && self.gain == 0.0) {
                        // Discard whatever old audio precedes the marker.
                        self.skip_to(at);
                        let _ = self.markers.pop();
                        self.shared.frames_out.fetch_add(*frames_out, Release);
                        *frames_out = 0;
                        self.shared.base_bits.store(base.to_bits(), Release);
                        self.shared.frames_out.store(0, Release);
                        self.shared.cur_gen.store(generation, Release);
                        self.active = true;
                        // A sound queued while paused (device reopened) waits silently.
                        self.gain = if fade_in || paused { 0.0 } else { 1.0 };
                        continue;
                    }
                    switch_at = Some(at);
                }
                Some(Marker::End { generation, at }) if self.popped >= at => {
                    let _ = self.markers.pop();
                    if self.shared.cur_gen.load(Relaxed) == generation {
                        self.active = false;
                        self.shared.finished_gen.store(generation, Release);
                    }
                    continue;
                }
                Some(Marker::End { .. }) | None => {}
            }
            break;
        }
        let target = if switch_at.is_some() || paused || !self.active { 0.0 } else { 1.0 };
        if let Some(at) = switch_at
            && self.gain <= 0.0
        {
            // Faded out: drop the rest of the old sound, the marker
            // fires on the next frame.
            self.skip_to(at);
            frame.fill(0.0);
            return;
        }
        if target == 0.0 && self.gain <= 0.0 {
            frame.fill(0.0);
            return;
        }
        let limit = switch_at.unwrap_or(u64::MAX);
        if self.data.slots() >= ch && self.popped + ch as u64 <= limit {
            for s in frame.iter_mut() {
                *s = self.data.pop().unwrap_or(0.0) * self.gain;
            }
            self.popped += ch as u64;
            if switch_at.is_none() {
                *frames_out += 1;
            }
        } else {
            frame.fill(0.0); // underrun
        }
        if self.gain < target {
            self.gain = (self.gain + self.step).min(1.0);
        } else if self.gain > target {
            self.gain = (self.gain - self.step).max(0.0);
        }
    }

    fn skip_to(&mut self, at: u64) {
        let n = (at.saturating_sub(self.popped) as usize).min(self.data.slots());
        if n > 0 {
            if let Ok(c) = self.data.read_chunk(n) {
                c.commit_all();
            }
            self.popped += n as u64;
        }
    }
}

// -------------------------------------------------------------- decoder

struct Playing {
    generation: u64,
    src: Source,
    resampler: Option<Resampler>,
    done: bool,
    end_posted: bool,
    /// Source frames left before the range end, when playing a range.
    remaining: Option<u64>,
}

fn decoder_thread(rx: Receiver<Cmd>, shared: Arc<Shared>, repaint: Arc<dyn Fn() + Send + Sync>) {
    let mut out: Option<Output> = None;
    let mut playing: Option<Playing> = None;
    let mut pushed: u64 = 0;
    // Samples queued for the current Load (and its seeks), up to `SHOWN_AFTER`.
    let mut fed = 0usize;
    // Generation of the last Load, Seek or Stop handled.
    let mut last_gen = 0u64;
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_off = 0usize;
    let mut decoded = Vec::new();
    let mut mapped = Vec::new();

    loop {
        // Collect commands: block when idle, poll when there's work.
        let mut cmds = Vec::new();
        let busy = out.is_some()
            && (playing.as_ref().is_some_and(|p| !p.end_posted) || pending_off < pending.len());
        // Seconds of audio queued for the callback.
        let queued = out.as_ref().map_or(0.0, |o| {
            (o.data.buffer().capacity() - o.data.slots()) as f64 / (o.rate as usize * o.ch) as f64
        });
        let room = queued < 0.25; // keep ~250 ms queued
        if busy && room {
            cmds.extend(rx.try_iter());
        } else if busy {
            // Full: sleep until 150 ms are left (a command still wakes us
            // at once), then top up in one burst: ~10 wake-ups a second
            // instead of one per 3 ms, with margin for drivers that pull
            // big blocks.
            let wait = (queued - 0.15).clamp(0.003, 0.1);
            #[cfg(test)]
            tests::DECODER_SLEEPS.fetch_add(1, Relaxed);
            match rx.recv_timeout(Duration::from_secs_f64(wait)) {
                Ok(c) => {
                    cmds.push(c);
                    cmds.extend(rx.try_iter());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => {
                    cmds.push(c);
                    cmds.extend(rx.try_iter());
                }
                Err(_) => return,
            }
        }

        // Only the last Load matters; everything queued before it is stale.
        if let Some(last_load) = cmds.iter().rposition(|c| matches!(c, Cmd::Load { .. })) {
            let mut i = 0;
            cmds.retain(|c| {
                let keep = i >= last_load || matches!(c, Cmd::Output(_));
                i += 1;
                keep
            });
        }

        for cmd in cmds {
            match cmd {
                Cmd::Output(o) => {
                    crate::dsp::prewarm(o.rate);
                    out = Some(*o);
                    pushed = 0;
                    pending.clear();
                    pending_off = 0;
                    playing = None;
                }
                Cmd::Stop { generation } => {
                    last_gen = generation;
                    pending.clear();
                    pending_off = 0;
                    playing = None;
                    if let Some(o) = &mut out {
                        let _ = o.markers.push(Marker::Start { generation, at: pushed, base: 0.0, fade_in: false });
                        let _ = o.markers.push(Marker::End { generation, at: pushed });
                    }
                }
                Cmd::Load { path, generation, start, end } => {
                    pending.clear();
                    pending_off = 0;
                    playing = None;
                    fed = 0;
                    last_gen = generation;
                    let Some(o) = &mut out else {
                        *shared.error.lock() = Some((generation, "No audio output device".into()));
                        continue;
                    };
                    let _ = o.markers.push(Marker::Start { generation, at: pushed, base: start, fade_in: start > 0.0 });
                    match Source::open(&path) {
                        Ok(mut src) => {
                            // Starting at or past the end plays nothing (a
                            // failed seek would otherwise leave it at 0).
                            let past_end = start > 0.0
                                && (src.info.duration.is_some_and(|d| start >= d) || src.seek(start).is_err());
                            *shared.info.lock() = Some((generation, src.info.clone()));
                            let remaining = end
                                .filter(|e| *e > start && src.info.rate > 0)
                                .map(|e| ((e - start) * src.info.rate as f64).round() as u64);
                            playing = Some(Playing {
                                generation,
                                src,
                                resampler: None,
                                done: past_end,
                                end_posted: false,
                                remaining,
                            });
                        }
                        Err(e) => {
                            *shared.error.lock() = Some((generation, e));
                            let _ = o.markers.push(Marker::End { generation, at: pushed });
                        }
                    }
                    repaint();
                }
                Cmd::Seek { generation, secs } => {
                    pending.clear();
                    pending_off = 0;
                    let Some(o) = &mut out else { continue };
                    let _ = o.markers.push(Marker::Start { generation, at: pushed, base: secs, fade_in: true });
                    if playing.is_none() {
                        // Nothing to seek in (the file failed to open, sent
                        // before the UI knew): end at once and keep the error.
                        let _ = o.markers.push(Marker::End { generation, at: pushed });
                        let mut e = shared.error.lock();
                        if let Some((g, msg)) = e.take() {
                            *e = Some((if g == last_gen { generation } else { g }, msg));
                        }
                    }
                    last_gen = generation;
                    if let Some(p) = &mut playing {
                        p.generation = generation;
                        p.done = false;
                        p.end_posted = false;
                        p.remaining = None; // seeking leaves range playback
                        if let Some(r) = &mut p.resampler {
                            r.reset();
                        }
                        if p.src.seek(secs).is_err() {
                            p.done = true;
                        }
                        let mut g = shared.info.lock();
                        if let Some((_, info)) = g.clone() {
                            *g = Some((generation, info));
                        }
                    }
                }
            }
        }

        let Some(o) = &mut out else { continue };

        // Push pending output.
        if pending_off < pending.len() {
            let (done, _) = o.data.push_partial_slice(&pending[pending_off..]);
            pending_off += done.len();
            pushed += done.len() as u64;
            if pending_off < pending.len() {
                continue;
            }
            pending.clear();
            pending_off = 0;
        }

        let Some(p) = &mut playing else { continue };
        if p.done {
            if !p.end_posted {
                // Posted only once every sample of the sound is queued.
                let _ = o.markers.push(Marker::End { generation: p.generation, at: pushed });
                p.end_posted = true;
            }
            continue;
        }
        let cap = o.data.buffer().capacity();
        if cap - o.data.slots() >= (o.rate as usize * o.ch) / 4 {
            continue;
        }
        let next = if p.remaining == Some(0) { None } else { p.src.next(&mut decoded) };
        match next {
            Some(src_ch) => {
                if let Some(rem) = &mut p.remaining {
                    // Range playback: cut at the end, with a 5 ms fade so it
                    // never clicks.
                    let frames = decoded.len() / src_ch;
                    let keep = frames.min(*rem as usize);
                    decoded.truncate(keep * src_ch);
                    let fade = (p.src.info.rate as u64 / 200).max(1);
                    for (k, f) in decoded.chunks_exact_mut(src_ch).enumerate() {
                        let left = *rem - k as u64; // frames left including this one
                        if left <= fade {
                            let g = (left - 1) as f32 / fade as f32;
                            f.iter_mut().for_each(|s| *s *= g);
                        }
                    }
                    *rem -= keep as u64;
                }
                mapped.clear();
                map_channels(&decoded, src_ch, o.ch, &mut mapped);
                let rate = p.src.info.rate;
                if rate != o.rate && rate > 0 {
                    let r = p.resampler.get_or_insert_with(|| Resampler::new(rate, o.rate, o.ch));
                    r.process(&mapped, &mut pending);
                } else {
                    std::mem::swap(&mut pending, &mut mapped);
                }
                fed += pending.len();
                if fed >= (o.rate as f64 * SHOWN_AFTER) as usize * o.ch {
                    shared.fed_gen.store(p.generation, Release);
                }
            }
            None => {
                if let Some(r) = &mut p.resampler {
                    r.flush(&mut pending);
                }
                p.done = true;
            }
        }
        let (done, _) = o.data.push_partial_slice(&pending);
        pending_off = done.len();
        pushed += done.len() as u64;
        if pending_off >= pending.len() {
            pending.clear();
            pending_off = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timed decoder sleeps while busy (all harnesses in this process).
    pub static DECODER_SLEEPS: AtomicU64 = AtomicU64::new(0);

    /// Decoder wake-ups while playing, with the callback consuming in real
    /// time: `cargo test --release bench_decoder_wakeups -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_decoder_wakeups() {
        let p = tmp("wake.wav");
        write_wav(&p, 44100, &tone(44100, 10.0, 440.0));
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let t = std::time::Instant::now();
        let before = DECODER_SLEEPS.load(Relaxed);
        let mut underruns = 0;
        for i in 0..300 {
            // 10 ms buffers, paced like a device.
            let out = render(&mut h, 480);
            if i > 5 && out.iter().all(|s| *s == 0.0) {
                underruns += 1;
            }
            let due = Duration::from_millis(10 * (i + 1));
            if let Some(d) = due.checked_sub(t.elapsed()) {
                std::thread::sleep(d);
            }
        }
        let secs = t.elapsed().as_secs_f64();
        println!(
            "decoder: {:.0} timed sleeps per second of playback, {underruns} silent buffers",
            (DECODER_SLEEPS.load(Relaxed) - before) as f64 / secs
        );
    }

    fn write_wav(path: &std::path::Path, rate: u32, samples: &[i16]) {
        let mut b = Vec::new();
        let data_len = (samples.len() * 2) as u32;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&1u16.to_le_bytes()); // mono
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    fn tone(rate: u32, secs: f64, freq: f64) -> Vec<i16> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| ((2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() * 16000.0) as i16)
            .collect()
    }

    struct Harness {
        tx: Sender<Cmd>,
        r: Renderer,
        shared: Arc<Shared>,
    }

    fn harness(rate: u32) -> Harness {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        let s2 = shared.clone();
        std::thread::spawn(move || decoder_thread(rx, s2, Arc::new(|| {})));
        let ch = 2;
        let (dp, dc) = rtrb::RingBuffer::new(rate as usize * ch * 2);
        let (mp, mc) = rtrb::RingBuffer::new(256);
        tx.send(Cmd::Output(Box::new(Output { data: dp, markers: mp, rate, ch }))).unwrap();
        let r = Renderer {
            data: dc,
            markers: mc,
            ch,
            gain: 0.0,
            step: 1.0 / (rate as f32 * 0.005),
            popped: 0,
            active: false,
            shared: shared.clone(),
            scratch: Vec::new(),
            vol: 1.0,
            vol_coef: 1.0 - (-1.0 / (rate as f32 * 0.01)).exp(),
        };
        Harness { tx, r, shared }
    }

    fn render(h: &mut Harness, frames: usize) -> Vec<f32> {
        let mut out = vec![0f32; frames * 2];
        h.r.render(&mut out);
        out.chunks(2).map(|f| f[0]).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("totems-player-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join(name)
    }

    #[test]
    fn bit_exact_playback_and_end() {
        let src = tone(48000, 0.5, 440.0);
        let p = tmp("a.wav");
        write_wav(&p, 48000, &src);
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(render(&mut h, 480));
            if h.shared.finished_gen.load(SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10)); // real time, like a device
        }
        assert_eq!(h.shared.finished_gen.load(SeqCst), 2, "end detected");
        // Audio starts on the very first frame of the first callback.
        assert!(got[1].abs() > 0.0, "no start latency");
        let n = src.len();
        for i in 0..n {
            let want = src[i] as f32 / 32768.0;
            assert!((got[i] - want).abs() < 1e-6, "sample {i}: {} vs {want}", got[i]);
        }
        assert!(got[n..].iter().all(|s| *s == 0.0));
    }

    /// B5: clicking a file that ends at once (0 samples) or fails to open
    /// showed ▶ for a frame, because `state()` says Playing from the moment
    /// `play()` is called. Displays must only show playing once the audio
    /// has really started; a seek within a playing file must not blank it.
    #[test]
    fn instant_end_never_shows_playing() {
        let empty = tmp("empty.wav");
        write_wav(&empty, 48000, &[]);
        let junk = tmp("junk.wav");
        std::fs::write(&junk, b"RIFF....WAVEnot really a wav file").unwrap();
        // B7: a sound shorter than a few UI frames flashed ▶ too.
        let blip = tmp("blip.wav");
        write_wav(&blip, 48000, &tone(48000, 0.05, 440.0));
        let one = tmp("one.wav");
        write_wav(&one, 48000, &[1000]);
        let tone_p = tmp("shown.wav");
        write_wav(&tone_p, 48000, &tone(48000, 0.3, 440.0));
        let mut h = harness(48000);
        let mut generation = 1;
        for (path, plays) in [(empty, false), (junk, false), (blip, false), (one, false), (tone_p, true)] {
            generation += 1;
            let cur = Current { generation, path: path.clone(), requested_pos: 0.0, load_gen: generation, end: None };
            h.tx.send(Cmd::Load { path, generation, start: 0.0, end: None }).unwrap();
            let shown = |h: &Harness| {
                state_of(&h.shared, Some(&cur), true) == State::Playing && !loading_of(&h.shared, Some(&cur), true)
            };
            let mut ever_shown = false;
            let mut seek_checked = false;
            for _ in 0..2000 {
                ever_shown |= shown(&h);
                if state_of(&h.shared, Some(&cur), true) == State::Ended {
                    break;
                }
                if plays && shown(&h) && !seek_checked {
                    // A seek is a new generation of the same load: no gap.
                    let seek = Current { generation: generation + 100, ..cur.clone() };
                    assert!(!loading_of(&h.shared, Some(&seek), true), "seek blanks the playing display");
                    seek_checked = true;
                }
                render(&mut h, 480);
                std::thread::sleep(Duration::from_micros(200));
            }
            assert_eq!(state_of(&h.shared, Some(&cur), true), State::Ended, "gen {generation} ends");
            assert_eq!(ever_shown, plays, "gen {generation}: shown as playing");
            assert_eq!(seek_checked, plays);
        }
    }

    /// B8: a seek sent before the decoder found the file unreadable (the
    /// UI still said Playing) queued a Start with no End: the player stayed
    /// Playing forever, repainting at 60 Hz, and the error was lost.
    #[test]
    fn seek_after_failed_load_ends() {
        let junk = tmp("junk-seek.wav");
        std::fs::write(&junk, b"RIFF....WAVEnot really a wav file").unwrap();
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: junk.clone(), generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        h.tx.send(Cmd::Seek { generation: 3, secs: 1.0 }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let cur = Current { generation: 3, path: junk, requested_pos: 1.0, load_gen: 2, end: None };
        for _ in 0..20 {
            render(&mut h, 480);
        }
        assert_eq!(state_of(&h.shared, Some(&cur), true), State::Ended);
        assert!(h.shared.error.lock().as_ref().is_some_and(|(g, _)| *g == 3), "error kept for the seek");
    }

    /// A header claiming 200 MHz once built a 436 MB resampler table and
    /// stalled the decoder for 4 s; it must be refused at once.
    #[test]
    fn absurd_sample_rate_does_not_stall_decoder() {
        let bad = tmp("rate200M.wav");
        write_wav(&bad, 200_000_000, &tone(48000, 1.0, 440.0));
        let good = tmp("after-bad.wav");
        write_wav(&good, 48000, &vec![8000i16; 48000]);
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: bad, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        render(&mut h, 480);
        h.tx.send(Cmd::Load { path: good, generation: 3, start: 0.0, end: None }).unwrap();
        let t = std::time::Instant::now();
        let mut heard = false;
        while !heard && t.elapsed() < Duration::from_millis(500) {
            heard = render(&mut h, 480).iter().any(|s| (*s - 8000.0 / 32768.0).abs() < 1e-6);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(heard, "next file still silent after {:?}: decoder stuck", t.elapsed());
    }

    /// B19: a Load starting past the end (→ after a sound ended, or a click
    /// at the right edge of the waveform) replayed the whole file from 0.
    #[test]
    fn load_past_end_plays_nothing() {
        let p = tmp("one-second.wav");
        write_wav(&p, 48000, &vec![8000i16; 48000]);
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 2.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let mut got = Vec::new();
        for _ in 0..300 {
            got.extend(render(&mut h, 480));
            if h.shared.finished_gen.load(SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(h.shared.finished_gen.load(SeqCst), 2, "ends");
        let audible = got.iter().filter(|s| **s != 0.0).count();
        assert_eq!(audible, 0, "start past the end replayed {audible} frames from 0");
    }

    /// When the device dies mid-sound (unplugged), playback moves to the
    /// new default device from the same spot instead of freezing on ▶.
    /// Uses the real output device, silently; skipped without one.
    #[test]
    fn dead_device_resumes_on_the_new_one() {
        let p = tmp("device.wav");
        write_wav(&p, 48000, &tone(48000, 5.0, 440.0));
        let mut player = Player::new(Arc::new(|| {}));
        if player.device_error.is_some() {
            return;
        }
        player.set_volume_db(MIN_DB);
        player.play(p.clone(), 1.0);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(player.state(), State::Playing);
        let before = player.position();
        let generation = player.current.as_ref().unwrap().generation;
        player.shared.dead.store(true, SeqCst);
        player.poll();
        let c = player.current.clone().unwrap();
        assert!(c.generation > generation, "restarted");
        assert_eq!(c.path, p);
        assert!((c.requested_pos - before).abs() < 0.05, "from {before}, got {}", c.requested_pos);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(player.state(), State::Playing);
        assert!(player.position() > before, "plays on");
        player.stop();
    }

    /// B32: a device lost while paused dropped the sound but kept the
    /// pause: Space then showed ▶ with a frozen playhead and no sound.
    #[test]
    fn device_lost_while_paused_resumes() {
        let p = tmp("paused-device.wav");
        write_wav(&p, 48000, &tone(48000, 5.0, 440.0));
        let mut player = Player::new(Arc::new(|| {}));
        if player.device_error.is_some() {
            return;
        }
        player.set_volume_db(MIN_DB);
        player.play(p.clone(), 1.0);
        std::thread::sleep(Duration::from_millis(300));
        player.toggle_pause();
        assert_eq!(player.state(), State::Paused);
        let at = player.position();
        player.shared.dead.store(true, SeqCst);
        player.poll();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(player.state(), State::Paused, "still paused after the device came back");
        assert!((player.position() - at).abs() < 0.05, "same spot: {} vs {at}", player.position());
        player.toggle_pause(); // Space
        std::thread::sleep(Duration::from_millis(500));
        let (state, pos) = (player.state(), player.position());
        player.stop();
        assert!(state == State::Playing && pos > at + 0.2, "{state:?} but stuck at {pos:.3} s (paused at {at:.3} s)");
    }

    /// B42: a NaN volume (from a corrupt config) stuck in the callback's
    /// glide and muted everything for the session, even after 0 dB.
    #[test]
    fn nan_volume_recovers() {
        assert_eq!(db_to_gain(f32::NAN), 1.0);
        assert!((db_to_gain(1000.0) - db_to_gain(MAX_DB)).abs() < 1e-6);
        let p = tmp("nan.wav");
        write_wav(&p, 48000, &vec![16384i16; 48000]);
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        h.shared.volume.set(db_to_gain(f32::NAN));
        render(&mut h, 480);
        h.shared.volume.set(db_to_gain(0.0));
        render(&mut h, 9600);
        assert_eq!(render(&mut h, 480)[100], 0.5);
    }

    #[test]
    fn switching_is_click_free_and_immediate() {
        let a = tmp("long.wav");
        let b = tmp("b.wav");
        write_wav(&a, 48000, &tone(48000, 3.0, 300.0));
        write_wav(&b, 48000, &vec![8000i16; 48000]);
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: a, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        for _ in 0..10 {
            render(&mut h, 480);
        }
        h.tx.send(Cmd::Load { path: b, generation: 3, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let out = render(&mut h, 960);
        // Old sound fades over ~5 ms (240 frames), then the new one starts.
        let start = out.iter().position(|s| (*s - 8000.0 / 32768.0).abs() < 1e-6).expect("new sound plays");
        assert!(start <= 260, "switch took {start} frames");
        let max_step = out[..start].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(max_step < 0.05, "click during fade: {max_step}");
        assert_eq!(h.shared.cur_gen.load(SeqCst), 3);
    }

    #[test]
    fn resampled_seek_position() {
        let a = tmp("c.wav");
        write_wav(&a, 44100, &tone(44100, 4.0, 1000.0));
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: a, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        render(&mut h, 4800);
        h.tx.send(Cmd::Seek { generation: 3, secs: 2.0 }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let out = render(&mut h, 4800);
        assert_eq!(h.shared.cur_gen.load(SeqCst), 3);
        let base = f64::from_bits(h.shared.base_bits.load(SeqCst));
        let frames = h.shared.frames_out.load(SeqCst);
        assert_eq!(base, 2.0);
        assert!(frames > 4000 && frames <= 4800, "{frames}");
        // Resampled 1 kHz tone keeps its level.
        let peak = out[2400..].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((peak - 16000.0 / 32768.0).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn soft_clip_curve() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.891), -0.891);
        let mut prev = 0.0;
        for i in 0..4000 {
            let x = i as f32 / 1000.0;
            let y = soft_clip(x);
            assert!(y <= 1.0 && y >= prev, "monotonic and bounded at {x}");
            prev = y;
        }
        // Smooth knee: slope just above the threshold is ~1.
        let d = (soft_clip(0.8915) - soft_clip(0.8914)) / 0.0001;
        assert!((d - 1.0).abs() < 0.05, "slope {d}");
        assert_eq!(db_to_gain(MIN_DB), 0.0);
        assert_eq!(db_to_gain(0.0), 1.0);
        assert!((db_to_gain(12.0) - 3.981).abs() < 1e-3);
    }

    #[test]
    fn volume_glides_and_clips() {
        let p = tmp("v.wav");
        write_wav(&p, 48000, &vec![16384i16; 48000]); // constant 0.5 (-6 dBFS)
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 0.0, end: None }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let a = render(&mut h, 480);
        assert_eq!(a[100], 0.5, "0 dB is bit-exact");
        // +12 dB: 0.5 * 3.98 = 2.0 -> clipped.
        h.shared.volume.set(db_to_gain(12.0));
        h.shared.soft_clip.store(false, SeqCst);
        let b = render(&mut h, 4800);
        assert!(b[1] < 0.6, "volume glides rather than jumps: {}", b[1]);
        assert_eq!(*b.last().unwrap(), 1.0, "hard clip at full scale");
        // Soft knee: 0.5 * 1.9 = 0.95 lands in the knee and is gently reduced.
        h.shared.soft_clip.store(true, SeqCst);
        h.shared.volume.set(1.9);
        let c = render(&mut h, 4800);
        let y = *c.last().unwrap();
        assert!(y > 0.93 && y < 0.95, "soft knee: {y}");
        h.shared.volume.set(db_to_gain(MIN_DB));
        let d = render(&mut h, 9600);
        assert_eq!(*d.last().unwrap(), 0.0, "-inf is silence");
    }

    #[test]
    fn range_playback_stops_at_end_with_fade() {
        let p = tmp("r.wav");
        write_wav(&p, 48000, &vec![16384i16; 96000]); // 2 s of constant 0.5
        let mut h = harness(48000);
        h.tx.send(Cmd::Load { path: p, generation: 2, start: 0.5, end: Some(1.0) }).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let mut got = Vec::new();
        for _ in 0..100 {
            got.extend(render(&mut h, 480));
            if h.shared.finished_gen.load(SeqCst) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10)); // real time, like a device
        }
        assert_eq!(h.shared.finished_gen.load(SeqCst), 2, "range ends");
        let audible = got.iter().rposition(|s| *s != 0.0).unwrap() + 1;
        assert!((audible as i64 - 24000).abs() <= 2, "plays exactly 0.5 s: {audible}");
        assert!(got[audible - 1] < 0.01, "fades out at the end: {}", got[audible - 1]);
        assert_eq!(got[12000], 0.5);
    }

    /// Audio callback cost per device buffer (480 stereo frames at 48 kHz)
    /// with the ring kept full: `cargo test --release bench_render -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_render() {
        let rate = 48000;
        let ch = 2;
        for (name, vol) in [("0 dB", 1.0f32), ("-6 dB", 0.5), ("+6 dB soft", 2.0)] {
            let shared = Arc::new(Shared::default());
            shared.volume.set(vol);
            shared.soft_clip.store(true, SeqCst);
            let (mut dp, dc) = rtrb::RingBuffer::new(rate * ch * 2);
            let (mut mp, mc) = rtrb::RingBuffer::new(256);
            let mut r = Renderer {
                data: dc,
                markers: mc,
                ch,
                gain: 0.0,
                step: 1.0 / (rate as f32 * 0.005),
                popped: 0,
                active: false,
                shared: shared.clone(),
                scratch: Vec::new(),
                vol,
                vol_coef: 1.0 - (-1.0 / (rate as f32 * 0.01)).exp(),
            };
            mp.push(Marker::Start { generation: 2, at: 0, base: 0.0, fade_in: false }).unwrap();
            let chunk: Vec<f32> = (0..480 * ch).map(|i| ((i as f32) * 0.01).sin() * 0.4).collect();
            let mut out = vec![0f32; 480 * ch];
            let iters = 20_000;
            let mut total = Duration::ZERO;
            let mut worst = Duration::ZERO;
            for _ in 0..iters {
                let _ = dp.push_partial_slice(&chunk);
                let t = std::time::Instant::now();
                r.render(&mut out);
                let e = t.elapsed();
                total += e;
                worst = worst.max(e);
            }
            std::hint::black_box(&out);
            println!(
                "render {name:>11}: {:>6.0} ns per 480-frame buffer ({:.2} ns/frame), worst {:?}",
                total.as_nanos() as f64 / iters as f64,
                total.as_nanos() as f64 / (iters * 480) as f64,
                worst
            );
        }
    }

    /// Open to first decoded audio (the keypress-to-sound path), warm cache,
    /// per format: `TOTEMS_AUDIO_LIST=files.txt cargo test --release
    /// bench_open_first -- --ignored --nocapture` (one path per line).
    #[test]
    #[ignore]
    fn bench_open_first() {
        let list = std::fs::read_to_string(std::env::var("TOTEMS_AUDIO_LIST").expect("set TOTEMS_AUDIO_LIST")).unwrap();
        let mut by_ext: std::collections::BTreeMap<String, Vec<(Duration, Duration)>> = Default::default();
        let mut buf = Vec::new();
        for line in list.lines().filter(|l| !l.trim().is_empty()) {
            let p = PathBuf::from(line.trim());
            let _ = std::fs::read(&p); // warm the OS cache
            let t = std::time::Instant::now();
            let mut s = match Source::open(&p) {
                Ok(s) => s,
                Err(e) => {
                    println!("FAIL {e}: {}", p.display());
                    continue;
                }
            };
            let open = t.elapsed();
            let _ = s.next(&mut buf);
            let first = t.elapsed();
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            by_ext.entry(ext).or_default().push((open, first));
        }
        for (ext, mut v) in by_ext {
            v.sort_by_key(|x| x.1);
            let pct = |q: f64| v[((v.len() - 1) as f64 * q) as usize].1;
            let open_med = {
                let mut o: Vec<_> = v.iter().map(|x| x.0).collect();
                o.sort();
                o[o.len() / 2]
            };
            println!(
                "{ext:>5} n={:>4}  open median {:>8.2?}  first audio median {:>8.2?}  p90 {:>8.2?}  max {:>8.2?}",
                v.len(),
                open_med,
                pct(0.5),
                pct(0.9),
                pct(1.0)
            );
        }
    }

    /// TOTEMS_AUDIO_DIR=... cargo test decode_dir -- --ignored --nocapture
    #[test]
    #[ignore]
    fn decode_dir() {
        let dir = std::env::var("TOTEMS_AUDIO_DIR").expect("set TOTEMS_AUDIO_DIR");
        for p in crate::index::files_under(std::path::Path::new(&dir)) {
            let t = std::time::Instant::now();
            match Source::open(&p) {
                Ok(mut s) => {
                    let open = t.elapsed();
                    let mut buf = Vec::new();
                    let first = s.next(&mut buf).map(|_| t.elapsed());
                    let mut frames = buf.len() / s.info.channels.max(1);
                    while s.next(&mut buf).is_some() {
                        frames += buf.len() / s.info.channels.max(1);
                    }
                    println!(
                        "{:<32} open {:>6.2?} first {:>6.2?} all {:>7.2?}  {} frames  {:?}  [{}]",
                        p.file_name().unwrap().to_string_lossy(),
                        open,
                        first.unwrap_or_default(),
                        t.elapsed(),
                        frames,
                        s.info.duration,
                        s.info.describe()
                    );
                }
                Err(e) => println!("{:<32} ERROR {e}", p.file_name().unwrap().to_string_lossy()),
            }
        }
    }
}
