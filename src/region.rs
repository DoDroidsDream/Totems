//! Export a time range of an audio file as a new WAV, for dragging a
//! waveform selection out of the window.
//!
//! The file keeps the source's sample rate and channel count. PCM and
//! lossless sources keep their bit depth (16/24-bit round-trip bit-exactly),
//! float sources stay 32-bit float, lossy sources become 24-bit.
//!
//! Exported files are temporary: `cleanup` removes them when the app exits
//! and again at the next launch (catching crashes). Drop targets that keep
//! the audio copy it into their own project; anything referenced in place
//! is the user's to copy across.

use crate::decode::Source;
use std::hash::{Hash, Hasher};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fmt {
    I16,
    I24,
    I32,
    F32,
}

impl Fmt {
    fn bytes(self) -> u16 {
        match self {
            Fmt::I16 => 2,
            Fmt::I24 => 3,
            Fmt::I32 | Fmt::F32 => 4,
        }
    }
}

pub fn regions_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("Totems").join("Regions")
}

/// Exported files younger than this are left alone: Explorer finishes
/// copying a dropped file in the background after the drop returns.
pub const GRACE: std::time::Duration = std::time::Duration::from_secs(120);

/// Delete exported regions (and partial writes) older than `grace`, then
/// any folders left empty. Files a program still holds open are skipped and
/// retried next time. Returns how many files were removed.
pub fn cleanup(grace: std::time::Duration) -> usize {
    cleanup_in(&regions_dir(), grace)
}

fn cleanup_in(base: &Path, grace: std::time::Duration) -> usize {
    let Ok(dirs) = std::fs::read_dir(base) else { return 0 };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for d in dirs.flatten() {
        let dir = d.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let old = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .is_none_or(|age| age >= grace);
                if old && f.path().is_file() && std::fs::remove_file(f.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        let _ = std::fs::remove_dir(&dir); // only succeeds when empty
    }
    removed
}

/// Write `[start, end)` seconds of `src` to a WAV and return its path.
/// The same range of the same file is only written once.
pub fn export(src: &Path, start: f64, end: f64) -> Result<PathBuf, String> {
    export_to(&regions_dir(), src, start, end)
}

fn export_to(base: &Path, src: &Path, start: f64, end: f64) -> Result<PathBuf, String> {
    let (start, end) = (start.max(0.0), end.max(start));
    if end <= start {
        return Err("The selection is empty".into());
    }
    let mut source = Source::open(src)?;
    let info = source.info.clone();
    if info.rate == 0 || info.channels == 0 {
        return Err("Unknown sample format".into());
    }
    let fmt = if info.codec.starts_with("pcm_f") {
        Fmt::F32
    } else {
        match info.bits {
            Some(b) if b <= 16 => Fmt::I16,
            Some(b) if b <= 24 => Fmt::I24,
            Some(_) => Fmt::I32,
            None => Fmt::I24, // lossy
        }
    };

    // One folder per source file keeps names readable and collision-free.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    let dir = base.join(format!("{:016x}", h.finish()));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("Region");
    let out = dir.join(format!("{stem} [{start:.3}-{end:.3}].wav"));
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let ch = info.channels;
    let mut remaining = ((end - start) * info.rate as f64).round() as u64;
    if start > 0.0 {
        source.seek(start)?;
    }
    let tmp = out.with_extension("part");
    let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    write_header(&mut w, fmt, ch as u16, info.rate, 0).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut frames = 0u64;
    let mut bytes = Vec::with_capacity(1 << 16);
    while remaining > 0 {
        let Some(c) = source.next(&mut buf) else { break };
        if c != ch {
            return Err("Channel layout changed mid-stream".into());
        }
        let n = (buf.len() / ch).min(remaining as usize);
        bytes.clear();
        for &s in &buf[..n * ch] {
            match fmt {
                Fmt::F32 => bytes.extend_from_slice(&s.to_le_bytes()),
                Fmt::I16 => bytes.extend_from_slice(&quant(s, 16).to_le_bytes()[..2]),
                Fmt::I24 => bytes.extend_from_slice(&quant(s, 24).to_le_bytes()[..3]),
                Fmt::I32 => bytes.extend_from_slice(&quant(s, 32).to_le_bytes()),
            }
        }
        w.write_all(&bytes).map_err(|e| e.to_string())?;
        remaining -= n as u64;
        frames += n as u64;
    }
    if frames == 0 {
        drop(w);
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_dir(&dir); // only if empty
        return Err("The selection is past the end of the audio".into());
    }
    // Patch the sizes now that the length is known.
    let mut file = w.into_inner().map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    write_header(&mut file, fmt, ch as u16, info.rate, frames).map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// f32 in [-1, 1) back to a signed integer of `bits`, exact for values that
/// came from a `bits`-deep integer.
fn quant(s: f32, bits: u32) -> i32 {
    let scale = (1u64 << (bits - 1)) as f64;
    ((s as f64) * scale).round().clamp(-scale, scale - 1.0) as i32
}

/// RIFF/WAVE header. Uses WAVE_FORMAT_EXTENSIBLE for anything beyond plain
/// 16-bit mono/stereo, as the spec requires, up to the 18 channels its
/// speaker mask can name; more channels get a plain header, which readers
/// (symphonia included) accept where they reject mask bits past 18.
fn write_header(w: &mut impl Write, fmt: Fmt, ch: u16, rate: u32, frames: u64) -> std::io::Result<()> {
    let bps = fmt.bytes();
    let block = ch * bps;
    let data = frames * block as u64;
    let extensible = (ch > 2 || fmt != Fmt::I16) && ch <= 18;
    let fmt_len: u32 = if extensible { 40 } else { 16 };
    let riff = (4 + 8 + fmt_len as u64 + 8 + data).min(u32::MAX as u64) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&riff.to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&fmt_len.to_le_bytes())?;
    let tag: u16 = if extensible {
        0xFFFE
    } else if fmt == Fmt::F32 {
        3
    } else {
        1
    };
    w.write_all(&tag.to_le_bytes())?;
    w.write_all(&ch.to_le_bytes())?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * block as u32).to_le_bytes())?;
    w.write_all(&block.to_le_bytes())?;
    w.write_all(&(bps * 8).to_le_bytes())?;
    if extensible {
        w.write_all(&22u16.to_le_bytes())?;
        w.write_all(&(bps * 8).to_le_bytes())?; // valid bits
        let mask: u32 = match ch {
            1 => 0x4,
            2 => 0x3,
            4 => 0x33,
            6 => 0x3F,
            8 => 0x63F,
            n => (1u32 << n) - 1, // n <= 18
        };
        w.write_all(&mask.to_le_bytes())?;
        let sub: u16 = if fmt == Fmt::F32 { 3 } else { 1 };
        w.write_all(&sub.to_le_bytes())?;
        w.write_all(&[0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71])?;
    }
    w.write_all(b"data")?;
    w.write_all(&(data.min(u32::MAX as u64) as u32).to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(p: &Path) -> (crate::decode::Info, Vec<f32>) {
        let mut s = Source::open(p).unwrap();
        let mut all = Vec::new();
        let mut buf = Vec::new();
        while s.next(&mut buf).is_some() {
            all.extend_from_slice(&buf);
        }
        (s.info.clone(), all)
    }

    fn write_pcm(path: &Path, rate: u32, ch: u16, fmt: Fmt, samples: &[f32]) {
        let mut f = std::fs::File::create(path).unwrap();
        let frames = samples.len() as u64 / ch as u64;
        write_header(&mut f, fmt, ch, rate, frames).unwrap();
        for &s in samples {
            match fmt {
                Fmt::I16 => f.write_all(&quant(s, 16).to_le_bytes()[..2]).unwrap(),
                Fmt::I24 => f.write_all(&quant(s, 24).to_le_bytes()[..3]).unwrap(),
                Fmt::I32 => f.write_all(&quant(s, 32).to_le_bytes()).unwrap(),
                Fmt::F32 => f.write_all(&s.to_le_bytes()).unwrap(),
            }
        }
    }

    #[test]
    fn region_is_sample_exact_and_keeps_format() {
        let dir = std::env::temp_dir().join(format!("totems-region-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rate = 48000;
        for (fmt, ch) in [(Fmt::I16, 2u16), (Fmt::I24, 1), (Fmt::F32, 2), (Fmt::I24, 6)] {
            let n = rate as usize * 2;
            let src: Vec<f32> = (0..n * ch as usize)
                .map(|i| ((i as f32 * 0.37).sin() * 0.8 * (1u32 << 15) as f32).round() / (1u32 << 15) as f32)
                .collect();
            let p = dir.join(format!("src-{fmt:?}-{ch}.wav"));
            write_pcm(&p, rate, ch, fmt, &src);
            let (_, original) = decode_all(&p);

            let out = export_to(&dir.join("out"), &p, 0.25, 1.0).unwrap();
            assert!(out.file_name().unwrap().to_str().unwrap().ends_with("[0.250-1.000].wav"));
            let (info, region) = decode_all(&out);
            assert_eq!(info.rate, rate);
            assert_eq!(info.channels, ch as usize);
            let want_bits = match fmt {
                Fmt::I16 => 16,
                Fmt::I24 => 24,
                _ => 32,
            };
            assert_eq!(info.bits, Some(want_bits), "{fmt:?}");
            let c = ch as usize;
            assert_eq!(region.len(), 36000 * c, "{fmt:?} length");
            assert_eq!(&region[..], &original[12000 * c..48000 * c], "{fmt:?} bit-exact");
            // Second export of the same range reuses the file.
            assert_eq!(export_to(&dir.join("out"), &p, 0.25, 1.0).unwrap(), out);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// B21: exports of 19-26 channels set channel-mask bits symphonia (and
    /// other readers) reject, so Totems couldn't reopen its own file.
    #[test]
    fn many_channel_export_reopens() {
        let dir = std::env::temp_dir().join(format!("totems-region-20ch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("twenty.wav");
        let samples: Vec<f32> = (0..48000 * 20).map(|i| ((i % 2000) as f32 * 7.0) / 32768.0).collect();
        write_pcm(&p, 48000, 20, Fmt::I16, &samples);
        let (_, original) = decode_all(&p);
        let out = export_to(&dir.join("out"), &p, 0.25, 0.5).unwrap();
        assert!(Source::open(&out).is_ok(), "exported 20-ch WAV can't be reopened: {:?}", Source::open(&out).err());
        let (_, region) = decode_all(&out);
        assert_eq!(&region[..], &original[12000 * 20..24000 * 20]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// B23: an empty selection, or one past the real end of a truncated
    /// file, wrote an empty WAV and reported success.
    #[test]
    fn empty_or_out_of_data_range_is_an_error() {
        let dir = std::env::temp_dir().join(format!("totems-region-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("one-second.wav");
        write_pcm(&p, 48000, 1, Fmt::I16, &vec![0.25; 48000]);
        assert!(export_to(&dir.join("out"), &p, 0.5, 0.5).is_err(), "start == end wrote an empty WAV");
        let bytes = std::fs::read(&p).unwrap();
        let cut = dir.join("cut.wav");
        std::fs::write(&cut, &bytes[..bytes.len() - 96000 + 960]).unwrap(); // header says 1 s, holds 10 ms
        assert!(export_to(&dir.join("out"), &cut, 0.1, 0.6).is_err(), "range past the real data wrote an empty WAV");
        let leftovers: Vec<_> = walk(&dir.join("out"));
        assert!(leftovers.is_empty(), "files left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn walk(d: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(d) else { return vec![] };
        rd.flatten().flat_map(|e| if e.path().is_dir() { walk(&e.path()) } else { vec![e.path()] }).collect()
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn removes_old_keeps_recent() {
        let base = std::env::temp_dir().join(format!("totems-cleanup-{}", std::process::id()));
        let (a, b) = (base.join("aaaa"), base.join("bbbb"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        for p in [a.join("old.wav"), a.join("old.part"), b.join("old.wav")] {
            std::fs::File::create(&p).unwrap().set_modified(old).unwrap();
        }
        std::fs::write(b.join("fresh.wav"), b"x").unwrap();

        assert_eq!(cleanup_in(&base, GRACE), 3);
        assert!(!a.exists(), "emptied folder removed");
        assert!(b.join("fresh.wav").exists(), "recent drop kept for its copy");
        assert_eq!(cleanup_in(&base, Duration::ZERO), 1);
        assert!(!b.exists());
        let _ = std::fs::remove_dir_all(base);
    }
}

#[cfg(test)]
mod tool {
    /// TOTEMS_AUDIO_DIR=... TOTEMS_OUT=... cargo test export_dir -- --ignored
    #[test]
    #[ignore]
    fn export_dir() {
        let src = std::env::var("TOTEMS_AUDIO_DIR").unwrap();
        let out = std::path::PathBuf::from(std::env::var("TOTEMS_OUT").unwrap());
        for e in jwalk::WalkDir::new(src).into_iter().flatten().filter(|e| e.file_type.is_file()) {
            let t = std::time::Instant::now();
            let r = super::export_to(&out, &e.path(), 0.1, 0.6);
            println!("{:?} -> {:?} in {:?}", e.file_name, r.map(|p| p.file_name().unwrap().to_owned()), t.elapsed());
        }
    }
}
