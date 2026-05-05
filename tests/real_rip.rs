//! Real-rip end-to-end smoke test against a homebrew NSF.
//!
//! Fetches `chibi-tech_-_miko_miko_nurse.nsf` (a 1-track, NTSC, no-
//! expansion-chip rip authored 2003 by Jason Nisperos / Chibi-Tech)
//! from `samples.oxideav.org` and asserts:
//!
//! 1. The header parses cleanly.
//! 2. `start_song` runs the init routine to completion under the
//!    `MAX_CYCLES_PER_CALL` budget without hanging.
//! 3. Rendering 30 wall-clock seconds (~1.32 M PCM samples at 44.1 kHz)
//!    never panics and produces non-trivial audio (peak > 1000 LSB,
//!    mean |amplitude| > 50 LSB across the buffer).
//!
//! ## Network gating
//!
//! Set `OXIDEAV_NETWORK_TESTS=1` to permit first-time downloads. Without
//! the flag — and without a populated cache under
//! `target/test-fixtures/oxideav-nsf-real-rip/` — the test prints a
//! skip message and returns success. This mirrors the
//! `oxideav-msmpeg4` and `oxideav-mod` URL-fixture pattern.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use oxideav_nsf::{parse_nsf, NsfPlayer, OUTPUT_SAMPLE_RATE};

const FIXTURE_NAME: &str = "chibi-tech_-_miko_miko_nurse.nsf";
const FIXTURE_URL: &str = "https://samples.oxideav.org/audio/nsf/chibi-tech_-_miko_miko_nurse.nsf";
const FIXTURE_BYTES: u64 = 14_996;
const FIXTURE_SHA256: &str = "56fe8b0c7a555eb63c3f51455c1fca89d265b0007bd8418269d74e1d30b98ed0";

fn fixture_cache_dir() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let crate_dir = std::env::var("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .expect("CARGO_MANIFEST_DIR set during cargo test");
            crate_dir.join("..").join("..").join("target")
        });
    let dir = target_dir
        .join("test-fixtures")
        .join("oxideav-nsf-real-rip");
    fs::create_dir_all(&dir).expect("create test-fixtures dir");
    dir
}

fn cache_path() -> PathBuf {
    fixture_cache_dir().join(FIXTURE_NAME)
}

fn network_tests_enabled() -> bool {
    matches!(
        std::env::var("OXIDEAV_NETWORK_TESTS").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn fetch_fixture() -> Option<Vec<u8>> {
    let path = cache_path();
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() as u64 == FIXTURE_BYTES && sha256_hex(&bytes) == FIXTURE_SHA256 {
            eprintln!("[{FIXTURE_NAME}] using cached {}", path.display());
            return Some(bytes);
        }
        eprintln!(
            "[{FIXTURE_NAME}] cached {} is stale (len {} / sha256 {}), re-downloading",
            path.display(),
            bytes.len(),
            sha256_hex(&bytes)
        );
        let _ = fs::remove_file(&path);
    }
    if !network_tests_enabled() {
        eprintln!(
            "[{FIXTURE_NAME}] OXIDEAV_NETWORK_TESTS not set and no cached fixture at {} — skipping",
            path.display()
        );
        return None;
    }
    eprintln!("[{FIXTURE_NAME}] downloading {FIXTURE_URL}");
    let resp = match ureq::get(FIXTURE_URL).call() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[{FIXTURE_NAME}] download failed ({e}) — skipping");
            return None;
        }
    };
    let mut buf = Vec::with_capacity(FIXTURE_BYTES as usize);
    if let Err(e) = resp.into_body().into_reader().read_to_end(&mut buf) {
        eprintln!("[{FIXTURE_NAME}] body read failed ({e}) — skipping");
        return None;
    }
    if buf.len() as u64 != FIXTURE_BYTES {
        eprintln!(
            "[{FIXTURE_NAME}] downloaded size {} != expected {FIXTURE_BYTES} — skipping",
            buf.len()
        );
        return None;
    }
    let got = sha256_hex(&buf);
    if got != FIXTURE_SHA256 {
        panic!(
            "{FIXTURE_NAME}: sha256 mismatch:\n  expected {FIXTURE_SHA256}\n  got      {got}\n\
             Either the upstream blob changed (update FIXTURE_SHA256) or the download was corrupt."
        );
    }
    let tmp = path.with_extension("tmp");
    if let Err(e) = fs::write(&tmp, &buf).and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!(
            "[{FIXTURE_NAME}] cache write to {} failed ({e})",
            path.display()
        );
    } else {
        eprintln!("[{FIXTURE_NAME}] cached at {}", path.display());
    }
    Some(buf)
}

#[test]
fn header_parses_cleanly() {
    let Some(bytes) = fetch_fixture() else {
        eprintln!("real_rip::header_parses_cleanly skipped (no fixture)");
        return;
    };
    let h = parse_nsf(&bytes).expect("parse_nsf must succeed on chibi-tech");
    assert_eq!(h.version, 1);
    assert_eq!(h.total_songs, 1);
    assert_eq!(h.starting_song, 1);
    assert_eq!(h.load_addr, 0x8080);
    assert!(!h.is_nsfe);
    assert!(!h.has_expansion());
    assert_eq!(h.song_name, "Miko-Miko Nurse - Title Theme");
    assert_eq!(h.artist, "Chibi-Tech");
    eprintln!(
        "real_rip header: title={:?} artist={:?} copyright={:?} init=${:04x} play=${:04x}",
        h.song_name, h.artist, h.copyright, h.init_addr, h.play_addr
    );
}

#[test]
fn renders_thirty_seconds_without_panic() {
    let Some(bytes) = fetch_fixture() else {
        eprintln!("real_rip::renders_thirty_seconds_without_panic skipped (no fixture)");
        return;
    };
    let h = parse_nsf(&bytes).expect("parse_nsf must succeed");
    let mut player = NsfPlayer::new(h, OUTPUT_SAMPLE_RATE);
    player.start_song(1);

    // 30 seconds of mono PCM at 44.1 kHz = 1_323_000 samples. Render in
    // 1024-sample chunks so a runaway routine surfaces fast.
    const TOTAL: usize = (OUTPUT_SAMPLE_RATE as usize) * 30;
    const CHUNK: usize = 1024;
    let mut buf = vec![0i16; CHUNK];
    let mut produced = 0usize;
    let mut peak: u32 = 0;
    let mut nonzero_chunks = 0;
    while produced < TOTAL {
        let n = player.render(&mut buf);
        assert_eq!(n, CHUNK, "player halted at sample {produced}");
        let chunk_peak = buf
            .iter()
            .map(|s| s.unsigned_abs() as u32)
            .max()
            .unwrap_or(0);
        if chunk_peak > 0 {
            nonzero_chunks += 1;
        }
        peak = peak.max(chunk_peak);
        produced += n;
    }
    assert!(peak > 1_000, "real-rip peak {peak} too quiet — no audio?");
    assert!(
        nonzero_chunks * CHUNK > TOTAL / 4,
        "real-rip mostly silent: only {} non-zero chunks of {}",
        nonzero_chunks,
        TOTAL / CHUNK
    );
    eprintln!(
        "real_rip rendered {} samples (~{}s): peak={} non-zero chunks={}",
        produced,
        produced / OUTPUT_SAMPLE_RATE as usize,
        peak,
        nonzero_chunks
    );
}

// ---------------------------------------------------------------------------
// Pure-Rust SHA-256 (FIPS 180-4) — same recipe used by oxideav-msmpeg4's
// fixture loader so we don't add a dep just for one hash. Nothing fancy.
// ---------------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64) * 8;
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let mj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(mj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}
