//! Never-panic / never-hang robustness tests for the whole NSF parse
//! surface and the player render loop.
//!
//! The header parser ([`oxideav_nsf::parse_nsf`]) accepts three on-wire
//! shapes — the 128-byte NSF v1 header, the NSF v2 feature-flag variant
//! with an appended NSFe-style metadata blob, and the chunk-based NSFe
//! container — and dispatches to the matching decoder. Each of those
//! decoders walks attacker-controlled length fields, copies into fixed
//! buffers, and re-feeds optional chunks into the extended-metadata
//! parser. The contract under test is: **for any byte slice,
//! `parse_nsf` returns `Ok(NsfHeader)` or `Err(NsfError)` — it never
//! panics, never integer-overflows in debug, and never indexes out of
//! bounds.**
//!
//! For inputs that *do* parse, we additionally build an [`NsfPlayer`]
//! and render a bounded buffer. That drives the 6502 core, the 2A03
//! APU, and every expansion chip the header's chip mask enables against
//! whatever 6502 program bytes the fuzzer produced. The render contract
//! is: **building + starting + rendering never panics and always
//! returns within a bounded number of samples** (the player is
//! halt-guarded, so a program that never returns from PLAY must not
//! wedge the render loop).
//!
//! Every input here is synthesised inside the test from a tiny
//! self-contained LCG — no external crates, no fixtures, no third-party
//! decoder source. This is the deterministic, CI-runnable half of the
//! coverage-guided `fuzz/` libfuzzer harness: the libfuzzer targets
//! explore the same `parse_nsf` / render surface with corpus
//! minimisation; this test pins a fixed battery so a regression that
//! reintroduces a panic path is caught in plain `cargo test`.

use oxideav_nsf::{parse_nsf, NsfPlayer};

// ----- deterministic byte generator ----------------------------------

/// Minimal xorshift-style LCG so the test is fully reproducible without
/// pulling in `rand`. Same generator shape used across the workspace's
/// in-tree fuzz harnesses.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero fixed point.
        Lcg(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_u8(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }

    /// Fill a buffer with pseudo-random bytes.
    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = self.next_u8();
        }
    }
}

// ----- shared harness -------------------------------------------------

/// Parse `bytes`; if it parses, build a player and render a bounded
/// buffer. The whole path is on the never-panic / always-terminate
/// contract. Returns whether the input parsed (so callers can assert
/// coverage where they expect a parse to succeed).
fn drive(bytes: &[u8]) -> bool {
    let header = match parse_nsf(bytes) {
        Ok(h) => h,
        Err(_) => return false,
    };

    // Build a player at the standard output rate. `new` derives the
    // play period + mixer overrides from the (possibly hostile) header.
    let mut player = NsfPlayer::new(header.clone(), 44_100);

    // Start each of a few songs (the count field is attacker-controlled;
    // `start_song` is 1-based and must clamp internally). We bound the
    // loop so a huge song count can't blow the test budget.
    let song_count = header.total_songs.min(4);
    for song in 1..=song_count.max(1) {
        let mut p = NsfPlayer::new(header.clone(), 44_100);
        p.start_song(song);
        // Render a short buffer — long enough to invoke PLAY several
        // times (so the CPU/APU/expansion loop is exercised) but
        // bounded so a non-returning program can't hang the test.
        let mut out = [0i16; 2048];
        let produced = p.render(&mut out);
        assert!(
            produced <= out.len(),
            "render must never report more samples than the buffer holds",
        );
    }

    // Also drive the playlist iteration surface — indices are
    // attacker-derivable through NSFe `plst`, and the accessors must
    // bounds-check.
    let _ = player.playlist_len();
    let _ = player.sfx_playlist_len();
    for i in 0..player.playlist_len().min(8) {
        let _ = player.playlist_song(i);
        let _ = player.start_playlist_entry(i);
    }
    // Out-of-range indices must return None, not panic.
    assert!(player.playlist_song(usize::MAX).is_none());
    assert!(player.sfx_playlist_song(usize::MAX).is_none());

    true
}

// ----- a well-formed minimal NSF v1 we can mutate ---------------------

const NSF_MAGIC: [u8; 5] = [b'N', b'E', b'S', b'M', 0x1a];
const NSF_HEADER_LEN: usize = 0x80;

/// Build a minimal valid NSF v1: load/init/play in the program window,
/// one song, NTSC period, followed by a tiny program (`SEI; RTS`).
fn minimal_v1() -> Vec<u8> {
    let mut buf = vec![0u8; NSF_HEADER_LEN];
    buf[..5].copy_from_slice(&NSF_MAGIC);
    buf[0x05] = 0x01; // version
    buf[0x06] = 0x01; // total songs
    buf[0x07] = 0x01; // starting song (1-based)
    buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes()); // load
    buf[0x0a..0x0c].copy_from_slice(&0x8000u16.to_le_bytes()); // init
    buf[0x0c..0x0e].copy_from_slice(&0x8003u16.to_le_bytes()); // play
    buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes()); // NTSC period

    // Program: INIT at $8000 = SEI; CLD; RTS ; PLAY at $8003 = RTS.
    buf.extend_from_slice(&[0x78, 0xD8, 0x60, 0x60]);
    buf
}

// ----- tests ----------------------------------------------------------

#[test]
fn empty_and_tiny_slices_never_panic() {
    for len in 0..16usize {
        let buf = vec![0u8; len];
        assert!(!drive(&buf), "all-zero short slice must not parse");
    }
    // The bare magics are below the minimum header length and must be
    // rejected without panicking.
    assert!(!drive(&NSF_MAGIC));
    assert!(!drive(b"NSFE"));
}

#[test]
fn minimal_v1_parses_and_renders() {
    assert!(drive(&minimal_v1()), "the minimal NSF v1 must parse + play");
}

#[test]
fn truncated_prefixes_of_valid_v1_never_panic() {
    let full = minimal_v1();
    // Every prefix of a well-formed file must either parse cleanly (if
    // it lands past the header + a byte of program) or return Err —
    // never panic.
    for len in 0..=full.len() {
        let _ = drive(&full[..len]);
    }
}

#[test]
fn single_byte_mutations_of_valid_v1_never_panic() {
    let base = minimal_v1();
    // Flip every byte of the header to each of a spread of values. This
    // walks every length field, address field, region flag, and chip
    // mask through hostile values while keeping the rest well-formed.
    for pos in 0..NSF_HEADER_LEN {
        for &val in &[0x00u8, 0x01, 0x7F, 0x80, 0xFE, 0xFF] {
            let mut m = base.clone();
            m[pos] = val;
            let _ = drive(&m);
        }
    }
}

#[test]
fn random_byte_streams_never_panic() {
    let mut rng = Lcg::new(0x4E53_465A ^ 0x1234_5678);
    // A few thousand fully-random slices of assorted lengths. None is
    // expected to parse (the magic almost never appears), but the
    // dispatch + length guards must hold regardless.
    for _ in 0..4000 {
        let len = (rng.next_u64() % 4096) as usize;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let _ = drive(&buf);
    }
}

#[test]
fn random_payloads_behind_a_valid_v1_header_never_panic() {
    // Keep a well-formed header but feed the *program* region random
    // 6502 bytes. This is the highest-value fuzz: every parse succeeds,
    // so the CPU + APU + expansion render loop runs on adversarial code
    // — the surface most likely to hide a panic (bad opcode timing,
    // out-of-range bank select, expansion register edge).
    let header = minimal_v1();
    let mut rng = Lcg::new(0xC0DE_FACE);
    for _ in 0..512 {
        let prog_len = (rng.next_u64() % 1024) as usize + 4;
        let mut buf = header[..NSF_HEADER_LEN].to_vec();
        let mut prog = vec![0u8; prog_len];
        rng.fill(&mut prog);
        buf.extend_from_slice(&prog);
        assert!(drive(&buf), "valid header + random program must parse");
    }
}

#[test]
fn every_expansion_chip_mask_renders() {
    // The `$7B` expansion byte enables VRC6/VRC7/FDS/MMC5/N163/5B (bits
    // 0..5). Sweep every combination so each chip's register-decode +
    // signal-generation path is constructed and rendered at least once
    // against the minimal program. None may panic.
    for mask in 0u8..=0x3F {
        let mut buf = minimal_v1();
        buf[0x7b] = mask;
        assert!(drive(&buf), "chip mask {mask:#04x} must parse + render");
    }
}

#[test]
fn bankswitched_with_every_chip_mask_and_random_program_never_panic() {
    // Set a non-zero `bankswitch_init` (header `$70..=$77`) so the
    // bankswitched load path runs, sweep every expansion-chip mask, and
    // feed a random program. This exercises the FDS bank-pool copy paths
    // (`$5FF6/$5FF7` → cart RAM, `$5FF8..` → FDS RAM image) and the
    // bank_read window resolution against a fuzzer-built bank pool. The
    // bank-select bytes are also mutated mid-stream by the random
    // program's writes to `$5FFx`.
    let mut rng = Lcg::new(0xBA5E_BA11);
    for _ in 0..256 {
        let mut buf = minimal_v1();
        // Chip mask in `$7B`.
        buf[0x7b] = rng.next_u8() & 0x3F;
        // Non-zero bankswitch init across all 8 windows.
        for w in 0..8usize {
            buf[0x70 + w] = rng.next_u8();
        }
        // Random program of a random (sometimes sub-bank, sometimes
        // multi-bank) length.
        let prog_len = (rng.next_u64() % 5000) as usize + 1;
        let mut prog = vec![0u8; prog_len];
        rng.fill(&mut prog);
        buf.extend_from_slice(&prog);
        assert!(
            drive(&buf),
            "bankswitched header + chip mask + random program must parse + render",
        );
    }
}

#[test]
fn nsfe_random_chunk_streams_never_panic() {
    // Build `NSFE` + random chunk bytes. The chunk walker reads a 32-bit
    // size + 4-char tag per chunk; hostile sizes must be caught by the
    // `checked_add` + bounds guard, not panic.
    let mut rng = Lcg::new(0xFEED_BEEF);
    for _ in 0..4000 {
        let len = (rng.next_u64() % 512) as usize;
        let mut buf = b"NSFE".to_vec();
        let mut body = vec![0u8; len];
        rng.fill(&mut body);
        buf.extend_from_slice(&body);
        let _ = drive(&buf);
    }
}

#[test]
fn nsfe_structured_chunk_fuzz_never_panic() {
    // Emit syntactically-shaped chunks (valid 8-byte headers) with
    // attacker-chosen sizes + bodies, including the known extended tags
    // so the metadata sub-parsers (`auth`/`time`/`fade`/`plst`/`psfx`/
    // `mixe`/`regn`/`RATE`/`VRC7`) run on adversarial payloads.
    let tags: &[&[u8; 4]] = &[
        b"INFO", b"DATA", b"BANK", b"NSF2", b"auth", b"time", b"fade", b"plst", b"psfx", b"mixe",
        b"regn", b"RATE", b"VRC7", b"tlbl", b"taut", b"text", b"NEND",
    ];
    let mut rng = Lcg::new(0x0BAD_F00D);
    for _ in 0..6000 {
        let mut buf = b"NSFE".to_vec();
        let n_chunks = (rng.next_u64() % 6) as usize + 1;
        for _ in 0..n_chunks {
            let tag = tags[(rng.next_u64() as usize) % tags.len()];
            // Declared size: sometimes honest, sometimes wildly large.
            let body_len = (rng.next_u64() % 64) as usize;
            let declared = if rng.next_u8() & 1 == 0 {
                body_len as u32
            } else {
                rng.next_u64() as u32 // possibly huge → must be rejected
            };
            buf.extend_from_slice(&declared.to_le_bytes());
            buf.extend_from_slice(tag);
            let mut body = vec![0u8; body_len];
            rng.fill(&mut body);
            buf.extend_from_slice(&body);
        }
        let _ = drive(&buf);
    }
}
