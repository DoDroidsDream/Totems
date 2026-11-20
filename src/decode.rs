//! Thin wrapper over symphonia: open any supported file, decode packets to
//! interleaved f32, seek sample-accurately.

use std::path::Path;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::common::Limit;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
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
            if self.rate.is_multiple_of(1000) {
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

fn rate_ok(rate: u32) -> bool {
    (MIN_RATE..=MAX_RATE).contains(&rate)
}

fn probe(src: Box<dyn MediaSource>, hint: &Hint) -> Result<Box<dyn FormatReader>, String> {
    let mss = MediaSourceStream::new(src, MediaSourceStreamOptions { buffer_len: 64 * 1024 });
    let mut meta = MetadataOptions::default();
    meta.limit_tag_bytes = Limit::Maximum(256 * 1024);
    meta.limit_visual_bytes = Limit::Maximum(0);
    symphonia::default::get_probe().probe(hint, mss, FormatOptions::default(), meta).map_err(|e| match e {
        Error::Unsupported(_) => "Unsupported format".to_string(),
        e => e.to_string(),
    })
}

/// Read `buf.len()` bytes at `at` without moving a shared file cursor.
fn read_at(file: &std::fs::File, buf: &mut [u8], at: u64) -> std::io::Result<usize> {
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(file, buf, at);
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_at(file, buf, at);
}

/// The data chunk of a RIFF/WAVE file holding MPEG audio (format tag 0x55
/// Layer 3, or 0x50 Layer 1/2), as (offset, length).
fn mpeg_in_wav(file: &std::fs::File) -> Option<(u64, u64)> {
    let file_len = file.metadata().ok()?.len();
    let mut head = [0u8; 12];
    read_at(file, &mut head, 0).ok().filter(|&n| n == 12)?;
    if &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return None;
    }
    let (mut at, mut mpeg) = (12u64, false);
    for _ in 0..64 {
        let mut ch = [0u8; 10];
        if read_at(file, &mut ch, at).ok()? < 8 {
            return None;
        }
        let size = u32::from_le_bytes(ch[4..8].try_into().ok()?) as u64;
        match &ch[..4] {
            b"fmt " => mpeg = matches!(u16::from_le_bytes([ch[8], ch[9]]), 0x55 | 0x50),
            b"data" if mpeg => {
                let start = at + 8;
                return Some((start, size.min(file_len.saturating_sub(start))));
            }
            b"data" => return None,
            _ => {}
        }
        at = at.checked_add(8 + size + (size & 1))?;
    }
    None
}

/// Byte overrides for a WAV header symphonia can't read as is (offset,
/// bytes), or None if there's nothing to fix:
/// - RF64 / BW64 (recorders switch to it past 4 GB): magic becomes RIFF and
///   the data size comes from the ds64 chunk;
/// - RIFF or data sizes left 0 or 0xFFFFFFFF by a crashed or streaming
///   recorder, or claiming more than the file holds: the data runs to the
///   end of the file.
///
/// Sizes stay 32-bit, so past 4 GB of data only the first 4 GB plays.
fn wav_repair(file: &std::fs::File) -> Option<Vec<(u64, Vec<u8>)>> {
    let file_len = file.metadata().ok()?.len();
    let mut head = [0u8; 12];
    read_at(file, &mut head, 0).ok().filter(|&n| n == 12)?;
    let rf64 = matches!(&head[..4], b"RF64" | b"BW64");
    if !(rf64 || &head[..4] == b"RIFF") || &head[8..] != b"WAVE" {
        return None;
    }
    let mut patches = Vec::new();
    if rf64 {
        patches.push((0, b"RIFF".to_vec()));
    }
    let (mut at, mut ds64_data) = (12u64, None);
    for _ in 0..64 {
        let mut ch = [0u8; 24];
        if read_at(file, &mut ch, at).ok()? < 8 {
            return None;
        }
        let size = u32::from_le_bytes(ch[4..8].try_into().ok()?);
        match &ch[..4] {
            b"ds64" => ds64_data = Some(u64::from_le_bytes(ch[16..24].try_into().ok()?)),
            b"data" => {
                let start = at + 8;
                let avail = file_len.saturating_sub(start);
                let real = match (size, ds64_data) {
                    (u32::MAX, Some(d)) => d.min(avail),
                    (0 | u32::MAX, _) => avail,
                    (n, _) => (n as u64).min(avail),
                };
                let real = real.min(u32::MAX as u64 - 1) as u32;
                if real != size {
                    patches.push((at + 4, real.to_le_bytes().to_vec()));
                }
                let riff = u32::from_le_bytes(head[4..8].try_into().ok()?) as u64;
                let need = start + real as u64 - 8;
                if rf64 || riff < need || riff > file_len - 8 {
                    let fixed = (file_len - 8).min(u32::MAX as u64) as u32;
                    patches.push((4, fixed.to_le_bytes().to_vec()));
                }
                return (!patches.is_empty()).then_some(patches);
            }
            _ => {}
        }
        at = at.checked_add(8 + size as u64 + (size as u64 & 1)).filter(|&a| a < file_len)?;
    }
    None
}

/// A byte range of a file, optionally with some bytes replaced, as a media
/// source of its own.
struct Window {
    file: std::fs::File,
    start: u64,
    len: u64,
    pos: u64,
    /// (offset within the window, replacement bytes)
    patches: Vec<(u64, Vec<u8>)>,
}

impl Window {
    fn new(file: std::fs::File, start: u64, len: u64, patches: Vec<(u64, Vec<u8>)>) -> Self {
        Self { file, start, len, pos: 0, patches }
    }
}

impl std::io::Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        if n == 0 {
            return Ok(0);
        }
        let got = read_at(&self.file, &mut buf[..n], self.start + self.pos)?;
        let (a, b) = (self.pos, self.pos + got as u64);
        for (off, bytes) in &self.patches {
            let (pa, pb) = (*off, off + bytes.len() as u64);
            if pa < b && pb > a {
                let (from, to) = (pa.max(a), pb.min(b));
                buf[(from - a) as usize..(to - a) as usize].copy_from_slice(&bytes[(from - pa) as usize..(to - pa) as usize]);
            }
        }
        self.pos = b;
        Ok(got)
    }
}

impl std::io::Seek for Window {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let p = match to {
            std::io::SeekFrom::Start(p) => p as i128,
            std::io::SeekFrom::Current(d) => self.pos as i128 + d as i128,
            std::io::SeekFrom::End(d) => self.len as i128 + d as i128,
        };
        if p < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

impl MediaSource for Window {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

impl Source {
    pub fn open(path: &Path) -> Result<Source, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let first = probe(Box::new(file), &hint);
        // The checks below and their readers get handles of their own: on
        // Windows a cloned handle shares one file cursor, so their reads
        // would move the first reader's position (a lengthless FLAC then
        // repeated audio, a piped Matroska decoded to silence).
        let own = || std::fs::File::open(path).map_err(|e| e.to_string());
        // Opened, but with no length (or none) it may be a WAV with an
        // unfinished header that `wav_repair` can fix.
        let lengthless = first.as_ref().is_ok_and(|f| {
            f.default_track(TrackType::Audio).is_some_and(|t| matches!(t.num_frames, None | Some(0)))
        });
        let format = match first {
            Ok(f) if !lengthless => f,
            first => {
                let file = own()?;
                if let Some((start, len)) = mpeg_in_wav(&file) {
                    // MP3 inside a WAV (format tag 0x55: old samplers, some
                    // break packs): play the data chunk as the MP3 it is.
                    let mut hint = Hint::new();
                    hint.with_extension("mp3");
                    probe(Box::new(Window::new(file, start, len, Vec::new())), &hint)?
                } else if let Some(patches) = wav_repair(&file) {
                    // RF64/BW64, or sizes a crashed or streaming recorder
                    // left as 0 / 0xFFFFFFFF: read it through a fixed header.
                    let len = file.metadata().map_err(|e| e.to_string())?.len();
                    probe(Box::new(Window::new(file, 0, len, patches)), &hint)?
                } else {
                    first?
                }
            }
        };
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
        // falls) explodes. No rate at all means the first packet tells
        // (checked in `next`); a stated rate of 0 is just broken.
        if params.sample_rate.is_some() && !rate_ok(rate) {
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
                // A chained Ogg moved on to its next stream: carry on with a
                // fresh decoder if it's the same shape of audio.
                Err(Error::ResetRequired) => {
                    if self.next_link() {
                        continue;
                    }
                    return None;
                }
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
            // Decoders drop `trim_start` frames (encoder delay) themselves,
            // so the decoded audio starts there.
            let start_ts = packet.pts.saturating_add(packet.trim_start);
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let ch = buf.spec().channels().count().max(1);
                    if self.info.rate == 0 {
                        self.info.rate = buf.spec().rate();
                        if !rate_ok(self.info.rate) {
                            return None;
                        }
                    }
                    if self.info.channels == 0 {
                        self.info.channels = ch;
                    }
                    buf.copy_to_vec_interleaved(out);
                    if let Some(t) = self.skip_until {
                        // Readers land up to a few packets early (MP3 preroll),
                        // and a packet may decode to nothing (Vorbis after a
                        // reset): drop audio until the target, however many
                        // packets that takes.
                        match self.time_base.and_then(|tb| tb.calc_time(start_ts)) {
                            Some(start) => {
                                let skip = ((t - start.as_secs_f64()) * self.info.rate as f64).round().max(0.0);
                                if skip >= (out.len() / ch) as f64 {
                                    out.clear();
                                    continue;
                                }
                                self.skip_until = None;
                                out.drain(..skip as usize * ch);
                            }
                            None => self.skip_until = None,
                        }
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

    /// Switch to the stream that follows (chained Ogg). False if there is
    /// none, or it has a different rate or channel count.
    fn next_link(&mut self) -> bool {
        let Some(track) = self.format.default_track(TrackType::Audio) else { return false };
        let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()).cloned() else { return false };
        let same_rate = params.sample_rate.is_none_or(|r| r == self.info.rate);
        let same_ch = params.channels.as_ref().is_none_or(|c| c.count() == self.info.channels);
        if !(same_rate && same_ch) {
            return false;
        }
        let Ok(decoder) = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default())
        else {
            return false;
        };
        (self.track_id, self.time_base, self.decoder) = (track.id, track.time_base, decoder);
        true
    }

    /// Seek to `secs`. Subsequent `next` output starts exactly there.
    pub fn seek(&mut self, secs: f64) -> Result<(), String> {
        let secs = secs.max(0.0);
        // An AAC decoder needs the previous packet's overlap to get the
        // first one right after a reset: land a little early and skip.
        let preroll = if self.info.codec == "aac" { 2048.0 / self.info.rate.max(1) as f64 } else { 0.0 };
        let time = Time::try_from_secs_f64((secs - preroll).max(0.0)).ok_or("bad time")?;
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
        // B7: rate 0 played one sample at the device rate and flashed ▶.
        for (rate, ok) in [(u32::MAX, false), (20_000_000, false), (10, false), (0, false), (8_000, true), (768_000, true)] {
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


#[cfg(test)]
mod header_tests {
    use super::*;

    /// 16-bit stereo PCM: (plain WAV bytes, the samples).
    fn plain() -> (Vec<u8>, Vec<i16>) {
        let samples: Vec<i16> = (0..48000 * 2).map(|i| ((i * 37) % 20000) as i16 - 10000).collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&48000u32.to_le_bytes());
        v.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        (v, samples)
    }

    /// The same audio as RF64 (or BW64): sizes 0xFFFFFFFF, real ones in ds64.
    fn rf64(magic: &[u8; 4]) -> Vec<u8> {
        let (p, _) = plain();
        let data_len = (p.len() - 44) as u64;
        let mut v = Vec::new();
        v.extend_from_slice(magic);
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        v.extend_from_slice(b"WAVEds64");
        v.extend_from_slice(&28u32.to_le_bytes());
        v.extend_from_slice(&((p.len() + 36 - 8) as u64).to_le_bytes()); // riff size
        v.extend_from_slice(&data_len.to_le_bytes()); // data size
        v.extend_from_slice(&(data_len / 4).to_le_bytes()); // sample count
        v.extend_from_slice(&0u32.to_le_bytes()); // table length
        v.extend_from_slice(&p[12..36]); // fmt chunk
        v.extend_from_slice(b"data");
        v.extend_from_slice(&u32::MAX.to_le_bytes());
        v.extend_from_slice(&p[44..]);
        v
    }

    fn decode_bytes(name: &str, bytes: &[u8]) -> Result<(Info, Vec<f32>), String> {
        let p = std::env::temp_dir().join(format!("totems-hdr-{}-{name}.wav", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        let r = Source::open(&p).map(|mut s| {
            let (mut all, mut buf) = (Vec::new(), Vec::new());
            while s.next(&mut buf).is_some() {
                all.extend_from_slice(&buf);
            }
            (s.info.clone(), all)
        });
        let _ = std::fs::remove_file(&p);
        r
    }

    /// B30: RF64/BW64 (field recorders past 4 GB), and WAVs whose sizes a
    /// crashed or streaming recorder left as 0 or 0xFFFFFFFF, didn't open.
    #[test]
    fn rf64_and_unfinished_headers_play() {
        let (p, samples) = plain();
        let want: Vec<f32> = samples.iter().map(|s| *s as f32 / 32768.0).collect();
        let mut crashed = p.clone();
        crashed[4..8].copy_from_slice(&0u32.to_le_bytes());
        crashed[40..44].copy_from_slice(&0u32.to_le_bytes());
        let mut data_zero = p.clone();
        data_zero[40..44].copy_from_slice(&0u32.to_le_bytes());
        let mut streaming = p.clone();
        streaming[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        streaming[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut too_long = p.clone();
        too_long[40..44].copy_from_slice(&(p.len() as u32 * 3).to_le_bytes());
        for (name, bytes) in [
            ("plain", p.clone()),
            ("rf64", rf64(b"RF64")),
            ("bw64", rf64(b"BW64")),
            ("crashed", crashed),
            ("data-zero", data_zero),
            ("streaming", streaming),
            ("too-long", too_long),
        ] {
            let (info, got) = decode_bytes(name, &bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((info.rate, info.channels), (48000, 2), "{name}");
            assert_eq!(got.len(), want.len(), "{name}: sample count");
            assert!(got == want, "{name}: samples differ");
            assert!(info.duration.is_some_and(|d| (d - 1.0).abs() < 1e-6), "{name}: duration {:?}", info.duration);
        }
    }
}

#[cfg(test)]
mod lengthless_tests {
    use super::*;

    fn piped(args: &[&str], out: &Path) -> bool {
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=2", "-ac", "1"])
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success() && !o.stdout.is_empty())
            .is_some_and(|o| std::fs::write(out, o.stdout).is_ok())
    }

    fn all(p: &Path) -> Vec<f32> {
        let mut s = Source::open(p).unwrap();
        let (mut v, mut buf) = (Vec::new(), Vec::new());
        while s.next(&mut buf).is_some() {
            v.extend_from_slice(&buf);
        }
        v
    }

    /// Files with no length in their header (FLAC or Matroska written to a
    /// pipe) went through the header-repair checks, whose reads moved the
    /// file cursor the first reader shares on Windows: the FLAC repeated
    /// audio, the Matroska decoded to silence. Needs ffmpeg.
    #[test]
    fn lengthless_files_decode_whole() {
        let d = std::env::temp_dir().join(format!("totems-lengthless-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (wav, flac, mka) = (d.join("ref.wav"), d.join("streamed.flac"), d.join("piped.mka"));
        if !piped(&["-f", "wav", "pipe:1"], &wav)
            || !piped(&["-f", "flac", "pipe:1"], &flac)
            || !piped(&["-c:a", "flac", "-f", "matroska", "pipe:1"], &mka)
        {
            eprintln!("ffmpeg not found; skipping");
            return;
        }
        let want = all(&wav);
        assert_eq!(want.len(), 96000, "reference");
        for f in [&flac, &mka] {
            let got = all(f);
            assert!(got == want, "{}: {} samples, first difference at {:?}", f.display(), got.len(), got.iter().zip(&want).position(|(a, b)| a != b));
        }
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod seek_tests {
    /// B17: WAVs holding MP3 (format tag 0x55; 152 of 982 files in one
    /// break pack on Liam's drive) were "Unsupported format". They play as
    /// the MP3 stream they contain. Needs ffmpeg.
    #[test]
    fn mp3_inside_wav_plays() {
        let dir = std::env::temp_dir();
        let wav = dir.join(format!("totems-mp3in-{}.wav", std::process::id()));
        let mp3 = dir.join(format!("totems-mp3in-{}.mp3", std::process::id()));
        let mk = |out: &Path, fmt: &str| {
            std::process::Command::new("ffmpeg")
                .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "sine=f=440:r=44100:d=2", "-ac", "2", "-c:a", "libmp3lame", "-f", fmt])
                .arg(out)
                .status()
                .is_ok_and(|s| s.success())
        };
        if !mk(&wav, "wav") || !mk(&mp3, "mp3") {
            eprintln!("ffmpeg not found; skipping");
            return;
        }
        assert_eq!(&std::fs::read(&wav).unwrap()[20..22], &[0x55, 0], "ffmpeg wrote an MPEG WAV");
        let s = Source::open(&wav).unwrap();
        assert_eq!((s.info.rate, s.info.channels, s.info.codec), (44100, 2, "mp3"));
        let (a, b) = (decode(&wav, None), decode(&mp3, None));
        assert!(a.len() > 44100 * 2 * 19 / 10, "{} samples", a.len());
        // Same stream as the plain MP3 (allowing for the wrapper's lack of
        // a gapless header), and seeks work inside the wrapper.
        let off = b.windows(64).position(|w| w == &a[20000..20064]).expect("same audio as the .mp3");
        assert_eq!(off % 2, 0);
        let got = decode(&wav, Some(1.0));
        assert!((got.len() as i64 - (a.len() as i64 - 88200)).abs() <= 2 * 1152 * 2, "seek length");
        for p in [wav, mp3] {
            let _ = std::fs::remove_file(p);
        }
    }

    use super::*;
    use std::path::PathBuf;

    fn encode(ext: &str, codec: &str) -> Option<PathBuf> {
        let p = std::env::temp_dir().join(format!("totems-seek-{}.{ext}", std::process::id()));
        // AAC codes noise as noise (PNS: the decoder makes up its own), so
        // it gets tones instead.
        let src = if codec == "aac" {
            "aevalsrc=0.3*sin(2*PI*440*t)+0.2*sin(2*PI*3001*t)|0.3*sin(2*PI*660*t):s=44100:d=4"
        } else {
            "anoisesrc=color=pink:sample_rate=44100:duration=4:seed=7"
        };
        std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg(src)
            .args(["-ac", "2", "-c:a", codec])
            .arg(&p)
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| p)
    }

    fn decode(p: &Path, seek: Option<f64>) -> Vec<f32> {
        let mut s = Source::open(p).unwrap();
        if let Some(t) = seek {
            s.seek(t).unwrap();
        }
        let (mut all, mut buf) = (Vec::new(), Vec::new());
        while s.next(&mut buf).is_some() {
            all.extend_from_slice(&buf);
        }
        all
    }

    /// B22: a chained Ogg (streams back to back) stopped after the first.
    #[test]
    fn chained_ogg_plays_every_link() {
        let Some(p) = encode("ogg", "libvorbis") else { return };
        let one = decode(&p, None).len();
        let bytes = std::fs::read(&p).unwrap();
        let chained = p.with_file_name(format!("totems-chained-{}.ogg", std::process::id()));
        std::fs::write(&chained, [bytes.as_slice(), bytes.as_slice()].concat()).unwrap();
        let both = decode(&chained, None);
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(&chained);
        assert_eq!(both.len(), 2 * one, "second link dropped");
        assert!(both[..one] == both[one..], "links decode the same");
    }

    /// B18: MP3 and Ogg seeks started up to 47 ms early (the skip was
    /// applied to the first packet only, and ignored the encoder-delay
    /// trim), which shifted waveform clicks, range audition and region
    /// export. Needs ffmpeg; skipped without it.
    #[test]
    fn seek_is_sample_accurate_in_every_format() {
        // (steady-state tolerance, tolerance for the first 2048 frames: MP3
        // decoders need a few frames to settle after a seek, about -46 dB)
        for (ext, codec, tol, head_tol) in [
            ("wav", "pcm_s16le", 0.0, 0.0),
            ("flac", "flac", 0.0, 0.0),
            ("mp3", "libmp3lame", 1e-4, 5e-3),
            ("ogg", "libvorbis", 1e-4, 1e-4),
            ("m4a", "aac", 1e-3, 1e-4),
        ] {
            let Some(p) = encode(ext, codec) else {
                eprintln!("ffmpeg not found; skipping");
                return;
            };
            let full = decode(&p, None);
            for t in [0.0, 0.01, 1.0, 2.5] {
                let got = decode(&p, Some(t));
                let s = (t * 44100.0).round() as usize * 2;
                assert_eq!(got.len(), full.len() - s, "{ext} seek {t}: length");
                let (a, b) = (&got[8192..16384], &full[s + 8192..s + 16384]);
                let err = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
                assert!(err <= tol, "{ext} seek {t}: max diff {err}");
                // Right after the seek too (no decoder warm-up glitch).
                let head = got[..4096].iter().zip(&full[s..s + 4096]).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
                assert!(head <= head_tol, "{ext} seek {t}: first 2048 frames differ by {head}");
            }
            let _ = std::fs::remove_file(p);
        }
    }
}
