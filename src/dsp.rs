//! Sample-level DSP used by the preview engine: channel mapping and a
//! high-quality streaming windowed-sinc sample-rate converter.

use std::f64::consts::PI;

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

const PHASES: usize = 512;
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

/// Streaming band-limited resampler (Kaiser-windowed sinc, polyphase table
/// with linear phase interpolation). Roughly -120 dB stop band, transparent
/// pass band up to ~0.92 of the lower Nyquist.
pub struct Resampler {
    ch: usize,
    step: f64,
    half: usize,
    taps: usize,
    table: Vec<f32>, // (PHASES + 1) * taps
    buf: Vec<f32>,   // interleaved input history
    pos: f64,        // read position in frames relative to buf start
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, ch: usize) -> Self {
        let step = in_rate as f64 / out_rate as f64;
        let cutoff = (out_rate as f64 / in_rate as f64).min(1.0) * 0.94;
        let half = (ZERO_CROSSINGS / cutoff).ceil() as usize;
        let taps = 2 * half;
        let i0b = bessel_i0(KAISER_BETA);
        let mut table = vec![0f32; (PHASES + 1) * taps];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut sum = 0.0;
            let mut vals = vec![0f64; taps];
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
            for (o, v) in row.iter_mut().zip(vals) {
                *o = (v / sum) as f32;
            }
        }
        let mut r = Self { ch, step, half, taps, table, buf: Vec::new(), pos: 0.0 };
        r.reset();
        r
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        // Pre-pad so the first output sample is centred on input sample 0.
        self.buf.resize((self.half - 1) * self.ch, 0.0);
        self.pos = (self.half - 1) as f64;
    }

    /// Feed interleaved input, append interleaved output.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        self.run(out);
    }

    /// Flush the filter tail at end of stream.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let pad = vec![0f32; (self.half + 1) * self.ch];
        self.buf.extend_from_slice(&pad);
        self.run(out);
    }

    fn run(&mut self, out: &mut Vec<f32>) {
        let ch = self.ch;
        let frames = self.buf.len() / ch;
        let taps = self.taps;
        let mut acc = [0f32; 16];
        loop {
            let i = self.pos.floor() as usize;
            if i + self.half >= frames {
                break;
            }
            let fp = (self.pos - i as f64) * PHASES as f64;
            let p = fp.floor() as usize;
            let a = (fp - p as f64) as f32;
            let c0 = &self.table[p * taps..(p + 1) * taps];
            let c1 = &self.table[(p + 1) * taps..(p + 2) * taps];
            let start = i + 1 - self.half; // first input frame used
            if ch <= 16 {
                acc[..ch].fill(0.0);
                let src = &self.buf[start * ch..(start + taps) * ch];
                for j in 0..taps {
                    let c = c0[j] + (c1[j] - c0[j]) * a;
                    let f = &src[j * ch..j * ch + ch];
                    for k in 0..ch {
                        acc[k] += f[k] * c;
                    }
                }
                out.extend_from_slice(&acc[..ch]);
            } else {
                for k in 0..ch {
                    let mut s = 0f32;
                    for j in 0..taps {
                        let c = c0[j] + (c1[j] - c0[j]) * a;
                        s += self.buf[(start + j) * ch + k] * c;
                    }
                    out.push(s);
                }
            }
            self.pos += self.step;
        }
        // Drop consumed history, keep what the next output needs.
        let keep_from = (self.pos.floor() as usize).saturating_sub(self.half - 1);
        if keep_from > 0 {
            let keep_from = keep_from.min(frames);
            self.buf.drain(..keep_from * ch);
            self.pos -= keep_from as f64;
        }
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

    #[test]
    fn downmix_51() {
        let mut out = Vec::new();
        map_channels(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0] > 0.0 && out[1] == 0.0);
    }
}
