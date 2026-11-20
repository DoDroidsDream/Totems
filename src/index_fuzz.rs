//! Cache-file fuzzing for `Index::load` (from the 2026-11-21 bug hunt).
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("totems-hunt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sample() -> Index {
    let dirs = vec!["/a/sfx".to_string(), "/b/Müsic 🎵".to_string()];
    let e = |dir, name: &str, size, mtime| RawEntry { dir, name: name.into(), size, mtime };
    Index::build(
        dirs,
        vec![
            e(0, "Whoosh_01.wav", 500, 3),
            e(1, "Sóng ΟΔΟΣ.mp3", 9000, 1),
            e(0, "door_slam.flac", 100, 2),
            e(1, "e\u{301}clair.flac", 100, 2),
            e(0, "🎹.wav", 7, 9),
        ],
    )
}

/// Touch everything a loaded index exposes, the way search and the UI do.
fn exercise(ix: &Index) {
    for i in 0..ix.len() {
        let n = ix.name(i);
        assert!(std::str::from_utf8(n.as_bytes()).is_ok(), "name({i}) is not valid UTF-8: {:?}", n.as_bytes());
        let _ = ix.name_lower(i);
        let _ = ix.full_path(i);
        let _ = ix.find(&ix.full_path(i));
        let _ = ix.size[i] ^ ix.mtime[i] as u64 ^ ix.ext[i] as u64;
    }
    for q in ["o", "wav", "path:sfx", "ext:wav", "o wav"] {
        if let Ok(q) = crate::query::Query::parse(q, &Default::default()) {
            for s in [crate::query::SortKey::Name, crate::query::SortKey::Path, crate::query::SortKey::Size, crate::query::SortKey::Modified] {
                let _ = crate::query::search(ix, &q, s, true);
            }
        }
    }
}

fn try_load(roots: &[PathBuf], bytes: &[u8]) -> Result<bool, String> {
    catch_unwind(AssertUnwindSafe(|| match Index::parse(bytes, bytes.len() as u64, roots) {
        Some(ix) => {
            exercise(&ix);
            true
        }
        None => false,
    }))
    .map_err(|e| e.downcast_ref::<String>().cloned().or(e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default())
}

/// Every truncation, sentinel lengths and random byte flips of a cache with
/// Unicode names: `Index::load` rejects or loads, never panics, and a
/// loaded index is safe to search and display.
#[test]
fn corrupt_caches_are_rejected_never_panic() {
    let d = tmp("cache");
    let p = d.join("ix.bin");
    let roots = vec![PathBuf::from("/a")];
    sample().save(&p, &roots).unwrap();
    let good = std::fs::read(&p).unwrap();
    let mut fails = Vec::new();
    let mut accepted = 0usize;
    let t = std::time::Instant::now();
    for cut in 0..good.len() {
        match try_load(&roots, &good[..cut]) {
            Ok(true) => fails.push(format!("truncated at {cut} accepted")),
            Ok(false) => {}
            Err(e) => fails.push(format!("truncate {cut}: panic {e}")),
        }
    }
    for off in 0..good.len().saturating_sub(4) {
        for v in [0u32, 1, 2, 3, 0x7FFF_FFFF, 0xFFFF_FFFF, 0x1000_0000] {
            let mut m = good.clone();
            m[off..off + 4].copy_from_slice(&v.to_le_bytes());
            match try_load(&roots, &m) {
                Err(e) => fails.push(format!("u32 {v:#x} at {off}: panic {e}")),
                Ok(true) => accepted += 1,
                _ => {}
            }
        }
    }
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for _ in 0..20_000 {
        let mut m = good.clone();
        for _ in 0..(1 + rnd() % 4) {
            let i = (rnd() as usize) % m.len();
            if rnd() % 2 == 0 {
                m[i] ^= 1 << (rnd() % 8);
            } else {
                m[i] = rnd() as u8;
            }
        }
        match try_load(&roots, &m) {
            Err(e) => fails.push(format!("flip: panic {e}")),
            Ok(true) => accepted += 1,
            _ => {}
        }
    }
    println!("cache fuzz: {} bytes, {accepted} mutants accepted, {:?}", good.len(), t.elapsed());
    let _ = std::fs::remove_dir_all(d);
    fails.dedup();
    assert!(fails.is_empty(), "{} failures, first: {:#?}", fails.len(), &fails[..fails.len().min(10)]);
}
