//! Thin wrapper over symphonia: open any supported file, decode packets to
//! interleaved f32, seek sample-accurately.

use std::path::Path;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::common::Limit;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

#[derive(Clone, Debug, Default)]
pub struct Info {
    pub duration: Option<f64>,
    pub frames: Option<u64>,
    pub rate: u32,
    pub channels: usize,
    pub bits: Option<u32>,
    pub codec: &'static str,
}

impl Info {
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if self.rate > 0 {
            let k = self.rate as f64 / 1000.0;
            if self.rate % 1000 == 0 {
                s += &format!("{} kHz", self.rate / 1000);
            } else {
                s += &format!("{k:.1} kHz");
            }
        }
        if let Some(b) = self.bits {
            s += &format!(" · {b}-bit");
        }
        s += match self.channels {
            0 => "",
            1 => " · Mono",
            2 => " · Stereo",
            6 => " · 5.1",
            8 => " · 7.1",
            _ => "",
        };
        if self.channels > 2 && self.channels != 6 && self.channels != 8 {
            s += &format!(" · {} ch", self.channels);
        }
        if !self.codec.is_empty() {
            let c = if self.codec.starts_with("pcm") { "PCM" } else { self.codec };
            s += &format!(" · {}", c.to_uppercase());
        }
        // No rate: drop the separator the next part brought with it.
        s.strip_prefix(" · ").map(str::to_string).unwrap_or(s)
    }
}

pub struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    pub info: Info,
    /// After a seek: drop decoded audio before this time (seconds).
    skip_until: Option<f64>,
}

/// Sample rates Totems will play: 1 kHz up to twice 768 kHz, the highest
/// real PCM rate.
const MIN_RATE: u32 = 1_000;
const MAX_RATE: u32 = 1_536_000;

impl Source {
    pub fn open(path: &Path) -> Result<Source, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions { buffer_len: 64 * 1024 });
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let mut meta = MetadataOptions::default();
        meta.limit_tag_bytes = Limit::Maximum(256 * 1024);
        meta.limit_visual_bytes = Limit::Maximum(0);
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), meta)
            .map_err(|e| match e {
                Error::Unsupported(_) => "Unsupported format".to_string(),
                e => e.to_string(),
            })?;
        let track = format.default_track(TrackType::Audio).ok_or("No audio track")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("No audio track")?
            .clone();
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|_| "Unsupported codec".to_string())?;
        let rate = params.sample_rate.unwrap_or(0);
        // A header can claim any rate. Past these bounds the resampler's
        // table (grows with the input rate) or output (grows as the rate
        // falls) explodes; 0 means unknown and plays unresampled.
        if rate != 0 && !(MIN_RATE..=MAX_RATE).contains(&rate) {
            return Err(format!("Unsupported sample rate ({rate} Hz)"));
        }
        let frames = track.num_frames;
        let duration = match (track.time_base, track.duration) {
            (Some(tb), Some(d)) => tb.calc_duration(d).map(|t| t.as_secs_f64()),
            _ => None,
        }
        .or_else(|| frames.filter(|_| rate > 0).map(|f| f as f64 / rate as f64));
        let info = Info {
            duration,
            frames,
            rate,
            channels: params.channels.as_ref().map(|c| c.count()).unwrap_or(0),
            bits: params.bits_per_sample,
            codec: decoder.codec_info().short_name,
        };
        Ok(Source {
            track_id: track.id,
            time_base: track.time_base,
            format,
            decoder,
            info,
            skip_until: None,
        })
    }

    /// Decode the next chunk into `out` (cleared first). Returns the channel
    /// count, or None at end of stream.
    pub fn next(&mut self, out: &mut Vec<f32>) -> Option<usize> {
        out.clear();
        let mut errors = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return None,
                Err(Error::ResetRequired) => return None,
                Err(Error::IoError(_)) => return None,
                Err(_) => {
                    errors += 1;
                    if errors > 64 {
                        return None;
                    }
                    continue;
                }
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let pts = packet.pts;
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let ch = buf.spec().channels().count().max(1);
                    if self.info.rate == 0 {
                        self.info.rate = buf.spec().rate();
                    }
                    if self.info.channels == 0 {
                        self.info.channels = ch;
                    }
                    buf.copy_to_vec_interleaved(out);
                    if let Some(t) = self.skip_until {
                        let start = self
                            .time_base
                            .and_then(|tb| tb.calc_time(pts))
                            .map(|t| t.as_secs_f64())
                            .unwrap_or(t);
                        let skip = ((t - start) * self.info.rate as f64).round().max(0.0) as usize;
                        self.skip_until = None;
                        let skip = (skip * ch).min(out.len());
                        out.drain(..skip);
                    }
                    if out.is_empty() {
                        continue;
                    }
                    return Some(ch);
                }
                Err(Error::DecodeError(_)) | Err(Error::IoError(_)) => {
                    errors += 1;
                    if errors > 64 {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Seek to `secs`. Subsequent `next` output starts exactly there.
    pub fn seek(&mut self, secs: f64) -> Result<(), String> {
        let time = Time::try_from_secs_f64(secs.max(0.0)).ok_or("bad time")?;
        self.format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) })
            .map_err(|e| e.to_string())?;
        self.decoder.reset();
        self.skip_until = Some(secs);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 16-bit mono PCM WAV whose header claims `rate`.
    fn wav_with_rate(rate: u32) -> Vec<u8> {
        let data = [0u8; 4000];
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&rate.wrapping_mul(2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    /// B6: with no sample rate the readout started with a stray separator
    /// ("· 16-bit · Mono · PCM").
    #[test]
    fn describe_has_no_leading_separator() {
        let i = Info { rate: 0, bits: Some(16), channels: 1, codec: "pcm_s16le", ..Default::default() };
        assert_eq!(i.describe(), "16-bit · Mono · PCM");
        let i = Info { rate: 44_100, bits: Some(24), channels: 2, codec: "pcm_s24le", ..Default::default() };
        assert_eq!(i.describe(), "44.1 kHz · 24-bit · Stereo · PCM");
        let i = Info { rate: 0, bits: None, channels: 0, codec: "mp3", ..Default::default() };
        assert_eq!(i.describe(), "MP3");
        assert_eq!(Info::default().describe(), "");
    }

    /// B1: a 44-byte header claiming 4.29 GHz made the player build a ~9 GB
    /// resampler table (out-of-memory abort); 10 Hz turned one packet into
    /// 156 MB and stalled the decoder for 3 s. Absurd rates are rejected.
    #[test]
    fn absurd_sample_rates_are_rejected() {
        let dir = std::env::temp_dir().join(format!("totems-rates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (rate, ok) in [(u32::MAX, false), (20_000_000, false), (10, false), (8_000, true), (768_000, true)] {
            let p = dir.join(format!("r{rate}.wav"));
            std::fs::write(&p, wav_with_rate(rate)).unwrap();
            let r = Source::open(&p);
            assert_eq!(r.is_ok(), ok, "{rate} Hz: {:?}", r.as_ref().err());
            if !ok {
                assert!(r.err().unwrap().contains("sample rate"));
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
