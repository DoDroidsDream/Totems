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
    pub paused: AtomicBool,
    pub dead: AtomicBool,
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
    repaint: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Clone)]
pub struct Current {
    pub generation: u64,
    pub path: PathBuf,
    pub requested_pos: f64,
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
            let repaint = repaint.clone();
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
            repaint,
        };
        p.open_output();
        p
    }

    fn open_output(&mut self) {
        self.stream = None;
        self.shared.dead.store(false, SeqCst);
        match build_output(self.shared.clone(), self.repaint.clone()) {
            Ok((stream, out)) => {
                self.out_rate = out.rate;
                self.stream = Some(stream);
                self.device_error = None;
                let _ = self.tx.send(Cmd::Output(Box::new(out)));
            }
            Err(e) => self.device_error = Some(e),
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
        self.current = Some(Current { generation, path: path.clone(), requested_pos: start });
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
        self.current = Some(Current { generation, path: cur.path, requested_pos: secs });
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
        let Some(c) = &self.current else { return State::Idle };
        if self.stream.is_none() {
            return State::Idle;
        }
        if self.shared.finished_gen.load(Acquire) == c.generation
            || self.shared.error.lock().as_ref().is_some_and(|(g, _)| *g == c.generation)
        {
            return State::Ended;
        }
        if self.shared.paused.load(Relaxed) {
            State::Paused
        } else {
            State::Playing
        }
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

// --------------------------------------------------------------- output

fn build_output(
    shared: Arc<Shared>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Result<(cpal::Stream, Output), String> {
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
            scratch: Vec::new(),
            repaint: repaint.clone(),
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

fn run<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut r: Renderer,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
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
                    shared.dead.store(true, SeqCst);
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
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// Smoothed output volume and its one-pole coefficient.
    vol: f32,
    vol_coef: f32,
}

impl Renderer {
    /// Volume and clipping, after all other processing. At exactly 0 dB this
    /// leaves samples untouched (bit-exact output).
    fn apply_volume(&mut self, out: &mut [f32]) {
        let target = self.shared.volume.get();
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
        let mut ended = false;
        for frame in out.chunks_exact_mut(ch) {
            // Apply any markers we've reached; note if a switch is pending.
            let mut switch_at = None;
            loop {
                match self.markers.peek().ok().copied() {
                    Some(Marker::Start { generation, at, base, fade_in }) => {
                        if self.popped >= at || (!self.active && self.gain == 0.0) {
                            // Discard whatever old audio precedes the marker.
                            self.skip_to(at);
                            let _ = self.markers.pop();
                            self.shared.frames_out.fetch_add(frames_out, Release);
                            frames_out = 0;
                            self.shared.base_bits.store(base.to_bits(), Release);
                            self.shared.frames_out.store(0, Release);
                            self.shared.cur_gen.store(generation, Release);
                            self.active = true;
                            self.gain = if fade_in { 0.0 } else { 1.0 };
                            continue;
                        }
                        switch_at = Some(at);
                    }
                    Some(Marker::End { generation, at }) => {
                        if self.popped >= at {
                            let _ = self.markers.pop();
                            if self.shared.cur_gen.load(Relaxed) == generation {
                                self.active = false;
                                self.shared.finished_gen.store(generation, Release);
                                ended = true;
                            }
                            continue;
                        }
                    }
                    None => {}
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
                continue;
            }
            if target == 0.0 && self.gain <= 0.0 {
                frame.fill(0.0);
                continue;
            }
            let limit = switch_at.unwrap_or(u64::MAX);
            if self.data.slots() >= ch && self.popped + ch as u64 <= limit {
                for s in frame.iter_mut() {
                    *s = self.data.pop().unwrap_or(0.0) * self.gain;
                }
                self.popped += ch as u64;
                if switch_at.is_none() {
                    frames_out += 1;
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
        self.shared.frames_out.fetch_add(frames_out, Release);
        self.apply_volume(out);
        if ended {
            (self.repaint)();
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
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_off = 0usize;
    let mut decoded = Vec::new();
    let mut mapped = Vec::new();

    loop {
        // Collect commands: block when idle, poll when there's work.
        let mut cmds = Vec::new();
        let busy = out.is_some()
            && (playing.as_ref().is_some_and(|p| !p.end_posted) || pending_off < pending.len());
        let room = out.as_ref().is_some_and(|o| {
            let cap = o.data.buffer().capacity();
            cap - o.data.slots() < (o.rate as usize * o.ch) / 4 // keep ~250 ms queued
        });
        if busy && room {
            cmds.extend(rx.try_iter());
        } else if busy {
            match rx.recv_timeout(Duration::from_millis(3)) {
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
                    out = Some(*o);
                    pushed = 0;
                    pending.clear();
                    pending_off = 0;
                    playing = None;
                }
                Cmd::Stop { generation } => {
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
                    let Some(o) = &mut out else {
                        *shared.error.lock() = Some((generation, "No audio output device".into()));
                        continue;
                    };
                    let _ = o.markers.push(Marker::Start { generation, at: pushed, base: start, fade_in: start > 0.0 });
                    match Source::open(&path) {
                        Ok(mut src) => {
                            if start > 0.0 {
                                let _ = src.seek(start);
                            }
                            *shared.info.lock() = Some((generation, src.info.clone()));
                            let remaining = end
                                .filter(|e| *e > start && src.info.rate > 0)
                                .map(|e| ((e - start) * src.info.rate as f64).round() as u64);
                            playing = Some(Playing {
                                generation,
                                src,
                                resampler: None,
                                done: false,
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
            repaint: Arc::new(|| {}),
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
            std::thread::sleep(Duration::from_millis(1));
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
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(h.shared.finished_gen.load(SeqCst), 2, "range ends");
        let audible = got.iter().rposition(|s| *s != 0.0).unwrap() + 1;
        assert!((audible as i64 - 24000).abs() <= 2, "plays exactly 0.5 s: {audible}");
        assert!(got[audible - 1] < 0.01, "fades out at the end: {}", got[audible - 1]);
        assert_eq!(got[12000], 0.5);
    }

    /// TOTEMS_AUDIO_DIR=... cargo test decode_dir -- --ignored --nocapture
    #[test]
    #[ignore]
    fn decode_dir() {
        let dir = std::env::var("TOTEMS_AUDIO_DIR").expect("set TOTEMS_AUDIO_DIR");
        for e in jwalk::WalkDir::new(dir).into_iter().flatten() {
            if !e.file_type.is_file() {
                continue;
            }
            let p = e.path();
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
