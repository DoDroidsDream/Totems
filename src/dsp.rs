//! Sample-level DSP used by the preview engine: channel mapping and a
//! high-quality streaming windowed-sinc sample-rate converter.

use std::f64::consts::PI;
use std::sync::{Arc, Mutex};

/// Map interleaved `src` with `src_ch` channels onto `dst_ch` channels,
/// appending to `dst`.
pub fn map_channels(src: &[f32], src_ch: usize, dst_ch: usize, dst: &mut Vec<f32>) {
    let frames = src.len() / src_ch.max(1);
    dst.reserve(frames * dst_ch);
    if src_ch == dst_ch {
        dst.extend_from_slice(&src[..frames * src_ch]);
        return;
    }
    match (src_ch, dst_ch) {
        (1, _) => {
            for &s in &src[..frames] {
                for _ in 0..dst_ch {
                    dst.push(s);
                }
            }
        }
        (_, 1) => {
            let g = 1.0 / src_ch as f32;
            for f in src.chunks_exact(src_ch) {
                dst.push(f.iter().sum::<f32>() * g);
            }
        }
        _ => {
            // Build a stereo pair from the source, then spread over dst channels.
            const M3: f32 = std::f32::consts::FRAC_1_SQRT_2;
            for f in src.chunks_exact(src_ch) {
                let (l, r) = if src_ch == 2 {
                    (f[0], f[1])
                } else if src_ch == 6 {
                    // 5.1: L R C LFE Ls Rs (WAVE / SMPTE order).
                    let c = f[2] * M3;
                    (
                        (f[0] + c + f[4] * M3) / (1.0 + M3 + M3),
                        (f[1] + c + f[5] * M3) / (1.0 + M3 + M3),
                    )
                } else {
                    // Generic: first two channels hard L/R, the rest centred at -3 dB.
                    let rest: f32 = f[2..].iter().sum::<f32>() * M3;
                    let norm = 1.0 + (src_ch - 2) as f32 * M3;
                    ((f[0] + rest) / norm, (f[1] + rest) / norm)
                };
                if dst_ch == 2 {
                    dst.push(l);
                    dst.push(r);
                } else {
                    dst.push(l);
                    dst.push(r);
                    for _ in 2..dst_ch {
                        dst.push(0.0);
                    }
                }
            }
        }
    }
}

/// Phases of the interpolated table, used when a rate pair has no small
/// exact polyphase table.
const PHASES: usize = 512;
/// Rate pairs whose exact polyphase table needs at most this many phases use
/// it: every common pair does (44.1 -> 48 kHz has 160, 11.025 -> 48 kHz 640).
const MAX_EXACT_PHASES: usize = 1024;
const ZERO_CROSSINGS: f64 = 24.0;
const KAISER_BETA: f64 = 9.0;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= half / k as f64;
        let t2 = term * term;
        sum += t2;
        if t2 < sum * 1e-18 {
            break;
        }
    }
    sum
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[derive(Clone, Copy)]
enum Step {
    /// Output `n` sits exactly at input `n * adv / phases`: one table row per
    /// phase, no coefficient interpolation.
    Exact { adv: usize, phases: usize },
    /// Any other ratio: interpolate between `PHASES + 1` rows.
    Interp { step: f64 },
}

/// Windowed-sinc coefficients for one rate pair. Building one takes up to a
/// few milliseconds, too slow for every Load, so they're cached (`kernel`).
struct Kernel {
    half: usize,
    taps: usize,
    /// One row of `taps` coefficients per phase.
    table: Vec<f32>,
    step: Step,
}

impl Kernel {
    fn build(in_rate: u32, out_rate: u32) -> Kernel {
        Self::build_with(in_rate, out_rate, MAX_EXACT_PHASES)
    }

    fn build_with(in_rate: u32, out_rate: u32, max_exact: usize) -> Kernel {
        let cutoff = (out_rate as f64 / in_rate as f64).min(1.0) * 0.94;
        let half = (ZERO_CROSSINGS / cutoff).ceil() as usize;
        let taps = 2 * half;
        let g = gcd(in_rate, out_rate).max(1);
        let exact = (out_rate / g) as usize;
        let (step, rows, denom) = if exact <= max_exact {
            (Step::Exact { adv: (in_rate / g) as usize, phases: exact }, exact, exact)
        } else {
            (Step::Interp { step: in_rate as f64 / out_rate as f64 }, PHASES + 1, PHASES)
        };
        let i0b = bessel_i0(KAISER_BETA);
        let mut table = vec![0f32; rows * taps];
        let mut vals = vec![0f64; taps];
        for (p, row) in table.chunks_exact_mut(taps).enumerate() {
            let frac = p as f64 / denom as f64;
            let mut sum = 0.0;
            for (j, v) in vals.iter_mut().enumerate() {
                let k = j as f64 - (half as f64 - 1.0);
                let x = k - frac; // distance (input samples) from output instant
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (PI * cutoff * x).sin() / (PI * cutoff * x)
                };
                let r = x / half as f64;
                let w = if r.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0b
                };
                *v = cutoff * sinc * w;
                sum += *v;
            }
            for (o, v) in row.iter_mut().zip(&vals) {
                *o = (v / sum) as f32;
            }
        }
        Kernel { half, taps, table, step }
    }
}

/// The kernel for a rate pair, built once and shared.
fn kernel(in_rate: u32, out_rate: u32) -> Arc<Kernel> {
    static CACHE: Mutex<Vec<(u32, u32, Arc<Kernel>)>> = Mutex::new(Vec::new());
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = c.iter().position(|(a, b, _)| (*a, *b) == (in_rate, out_rate)) {
        let item = c.remove(i);
        let k = item.2.clone();
        c.push(item);
        return k;
    }
    let k = Arc::new(Kernel::build(in_rate, out_rate));
    c.push((in_rate, out_rate, k.clone()));
    if c.len() > 8 {
        c.remove(0);
    }
    k
}

/// Build the kernels for the usual file rates ahead of time, so the first
/// file at each rate starts as fast as the rest.
pub fn prewarm(out_rate: u32) {
    for r in [44_100, 48_000, 88_200, 96_000] {
        if r != out_rate {
            kernel(r, out_rate);
        }
    }
}

/// Dot product written so the compiler can vectorise it (16 independent
/// partial sums; plain `f32` addition order would forbid that).
#[inline(always)]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, ar) = a[..n].as_chunks::<16>();
    let (b, br) = b[..n].as_chunks::<16>();
    let mut acc = [0f32; 16];
    for (x, y) in a.iter().zip(b) {
        for j in 0..16 {
            acc[j] += x[j] * y[j];
        }
    }
    let mut tail = 0f32;
    for (x, y) in ar.iter().zip(br) {
        tail += x * y;
    }
    for j in 0..8 {
        acc[j] += acc[j + 8];
    }
    for j in 0..4 {
        acc[j] += acc[j + 4];
    }
    (acc[0] + acc[2]) + (acc[1] + acc[3]) + tail
}

/// Streaming band-limited resampler (Kaiser-windowed sinc). Common rate
/// pairs use an exact polyphase table; others interpolate linearly between
/// 512 phases. Roughly -120 dB stop band, transparent pass band up to ~0.92
/// of the lower Nyquist.
pub struct Resampler {
    k: Arc<Kernel>,
    /// Input history, one buffer per channel.
    hist: Vec<Vec<f32>>,
    /// Input frame (index into `hist`) at or just before the next output.
    ipos: usize,
    /// Where the next output sits past `ipos`: a phase for exact tables, a
    /// fraction for interpolated ones.
    phase: usize,
    frac: f64,
    /// Interpolated coefficient row (scratch).
    coef: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, ch: usize) -> Self {
        Self::with_kernel(kernel(in_rate, out_rate), ch)
    }

    fn with_kernel(k: Arc<Kernel>, ch: usize) -> Self {
        let coef = vec![0f32; k.taps];
        let mut r = Self { k, hist: vec![Vec::new(); ch.max(1)], ipos: 0, phase: 0, frac: 0.0, coef };
        r.reset();
        r
    }

    pub fn reset(&mut self) {
        // Pre-pad so the first output sample is centred on input sample 0.
        let half = self.k.half;
        for h in &mut self.hist {
            h.clear();
            h.resize(half - 1, 0.0);
        }
        self.ipos = half - 1;
        self.phase = 0;
        self.frac = 0.0;
    }

    /// Feed interleaved input, append interleaved output.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.hist.len();
        if ch == 1 {
            self.hist[0].extend_from_slice(input);
        } else {
            let frames = input.len() / ch;
            for (k, h) in self.hist.iter_mut().enumerate() {
                h.extend(input.iter().skip(k).step_by(ch).take(frames));
            }
        }
        self.run(out);
    }

    /// Flush the filter tail at end of stream.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let pad = self.k.half + 1;
        for h in &mut self.hist {
            h.resize(h.len() + pad, 0.0);
        }
        self.run(out);
    }

    fn run(&mut self, out: &mut Vec<f32>) {
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU supports AVX2, checked just above.
            return unsafe { self.run_avx2(out) };
        }
        self.run_impl(out)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    fn run_avx2(&mut self, out: &mut Vec<f32>) {
        self.run_impl(out)
    }

    #[inline(always)]
    fn run_impl(&mut self, out: &mut Vec<f32>) {
        let k = &*self.k;
        let (half, taps) = (k.half, k.taps);
        let frames = self.hist[0].len();
        let (mut i, mut phase, mut frac) = (self.ipos, self.phase, self.frac);
        while i + half < frames {
            let start = i + 1 - half; // first input frame used
            let c: &[f32] = match k.step {
                Step::Exact { adv, phases } => {
                    let row = &k.table[phase * taps..(phase + 1) * taps];
                    phase += adv;
                    i += phase / phases;
                    phase %= phases;
                    row
                }
                Step::Interp { step } => {
                    let fp = frac * PHASES as f64;
                    let p = fp as usize;
                    let a = (fp - p as f64) as f32;
                    let c0 = &k.table[p * taps..(p + 1) * taps];
                    let c1 = &k.table[(p + 1) * taps..(p + 2) * taps];
                    for ((o, x), y) in self.coef.iter_mut().zip(c0).zip(c1) {
                        *o = x + (y - x) * a;
                    }
                    frac += step;
                    let whole = frac.floor();
                    i += whole as usize;
                    frac -= whole;
                    &self.coef
                }
            };
            for h in &self.hist {
                out.push(dot(&h[start..start + taps], c));
            }
        }
        // Drop consumed history, keep what the next output needs.
        let keep_from = i.saturating_sub(half - 1).min(frames);
        if keep_from > 0 {
            for h in &mut self.hist {
                h.drain(..keep_from);
            }
            i -= keep_from;
        }
        (self.ipos, self.phase, self.frac) = (i, phase, frac);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5).collect()
    }

    #[test]
    fn length_and_amplitude_preserved() {
        for (a, b) in [(44100, 48000), (96000, 48000), (48000, 44100), (22050, 48000)] {
            let input = sine(a, 1000.0, 1.0);
            let mut r = Resampler::new(a, b, 1);
            let mut out = Vec::new();
            for chunk in input.chunks(1000) {
                r.process(chunk, &mut out);
            }
            r.flush(&mut out);
            let expect = b as f64;
            assert!((out.len() as f64 - expect).abs() < 40.0, "{a}->{b}: {}", out.len());
            // Compare against an ideal sine in the steady-state middle.
            let mut err = 0f64;
            for i in 2000..(b as usize - 2000) {
                let ideal = (2.0 * PI * 1000.0 * i as f64 / b as f64).sin() * 0.5;
                err = err.max((out[i] as f64 - ideal).abs());
            }
            assert!(err < 2e-3, "{a}->{b} max err {err}");
        }
    }

    /// Run `input` (mono) through a fresh resampler in odd-sized chunks.
    fn resample(a: u32, b: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(a, b, 1);
        let mut out = Vec::new();
        for chunk in input.chunks(997) {
            r.process(chunk, &mut out);
        }
        r.flush(&mut out);
        out
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    #[ignore]
    fn quality_report() {
        // Pass band: a 20 kHz tone keeps its level; error vs ideal.
        for (a, b, f) in [(44100u32, 48000u32, 20000.0), (48000, 44100, 20000.0), (96000, 48000, 20000.0), (44056, 48000, 20000.0), (22050, 48000, 10000.0)] {
            let out = resample(a, b, &sine(a, f, 1.0));
            let mut err = 0f64;
            for i in 4000..(b as usize - 4000) {
                let ideal = (2.0 * PI * f * i as f64 / b as f64).sin() * 0.5;
                err = err.max((out[i] as f64 - ideal).abs());
            }
            println!("{a}->{b} {f} Hz: max err {err:.2e} ({:.1} dB)", 20.0 * (err / 0.5).log10());
        }
        // Stop band: tones above the output Nyquist must vanish.
        for (a, b, f) in [(96000u32, 48000u32, 30000.0), (48000, 44100, 23000.0), (192000, 44100, 60000.0)] {
            let out = resample(a, b, &sine(a, f, 1.0));
            let r = rms(&out[4000..b as usize - 4000]);
            println!("{a}->{b} {f} Hz alias: {:.1} dB", 20.0 * (r / (0.5 / 2f64.sqrt())).log10());
        }
    }

    /// Table build cost on the keypress-to-sound path:
    /// `cargo test --release bench_resampler_new -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_resampler_new() {
        for (a, b) in [(44100, 48000), (96000, 48000), (192000, 48000), (22050, 48000), (44056, 48000)] {
            let t = std::time::Instant::now();
            std::hint::black_box(Kernel::build(a, b));
            let cold = t.elapsed();
            kernel(a, b);
            let t = std::time::Instant::now();
            let n = 100;
            for _ in 0..n {
                std::hint::black_box(Resampler::new(a, b, 2));
            }
            println!("Resampler::new {a:>6} -> {b}: cold {cold:?}, cached {:?}", t.elapsed() / n);
        }
    }

    /// Resampler throughput: `cargo test --release bench_resampler -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_resampler() {
        for (a, b, ch) in [(44100, 48000, 2), (96000, 48000, 2), (48000, 44100, 2), (44100, 48000, 1), (48000, 96000, 6)] {
            let secs = 20.0;
            let mono = sine(a, 1000.0, secs);
            let input: Vec<f32> = mono.iter().flat_map(|s| std::iter::repeat_n(*s, ch)).collect();
            let mut r = Resampler::new(a, b, ch);
            let mut out = Vec::with_capacity((b as f64 * secs * 1.1) as usize * ch);
            let t = std::time::Instant::now();
            for chunk in input.chunks(1152 * ch) {
                r.process(chunk, &mut out);
            }
            r.flush(&mut out);
            let e = t.elapsed();
            std::hint::black_box(&out);
            println!(
                "resample {a:>6} -> {b:>6} x{ch}: {:>7.1} x realtime ({:.2} ms per second of audio), {} taps",
                secs / e.as_secs_f64(),
                e.as_secs_f64() * 1000.0 / secs,
                r.k.taps
            );
        }
    }

    /// Exact polyphase tables and the interpolated fallback agree, and
    /// channels never leak into each other (history is kept per channel).
    #[test]
    fn exact_and_interpolated_tables_agree() {
        for (a, b) in [(44100, 48000), (48000, 44100), (96000, 48000), (22050, 96000)] {
            let exact = Arc::new(Kernel::build_with(a, b, MAX_EXACT_PHASES));
            let interp = Arc::new(Kernel::build_with(a, b, 0));
            assert!(matches!(exact.step, Step::Exact { .. }) && matches!(interp.step, Step::Interp { .. }));
            // Left: a tone; middle: silence; right: a different tone.
            let l = sine(a, 1000.0, 0.5);
            let r = sine(a, 3000.0, 0.5);
            let input: Vec<f32> = l.iter().zip(&r).flat_map(|(x, y)| [*x, 0.0, *y]).collect();
            let mut outs = Vec::new();
            for k in [exact, interp] {
                let mut rs = Resampler::with_kernel(k, 3);
                let mut out = Vec::new();
                for c in input.chunks(3 * 777) {
                    rs.process(c, &mut out);
                }
                rs.flush(&mut out);
                outs.push(out);
            }
            assert_eq!(outs[0].len(), outs[1].len(), "{a}->{b}");
            let d = outs[0].iter().zip(&outs[1]).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
            assert!(d < 1e-5, "{a}->{b}: tables differ by {d}");
            let out = &outs[0];
            assert!(out.chunks(3).all(|f| f[1] == 0.0), "{a}->{b}: silent channel stays silent");
            let n = out.len() / 3;
            for i in n / 4..3 * n / 4 {
                let t = i as f64 / b as f64;
                let want_l = (2.0 * PI * 1000.0 * t).sin() * 0.5;
                let want_r = (2.0 * PI * 3000.0 * t).sin() * 0.5;
                assert!((out[3 * i] as f64 - want_l).abs() < 2e-3, "{a}->{b} L at {i}");
                assert!((out[3 * i + 2] as f64 - want_r).abs() < 2e-3, "{a}->{b} R at {i}");
            }
        }
    }

    #[test]
    fn downmix_51() {
        let mut out = Vec::new();
        map_channels(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0] > 0.0 && out[1] == 0.0);
    }
}

