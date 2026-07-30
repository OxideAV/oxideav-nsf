//! Hostile-input battery for the container layer: truncated chunk
//! headers, oversized/overflowing declared sizes, duplicate and
//! out-of-order chunks, boundary-exact lengths, and the NSF2
//! appended-metadata split. Complements the never-panic sweeps in
//! `tests/parse_fuzz.rs` with exact-error assertions, and pins the
//! writer round-trip contract on deterministically mutated streams.
//!
//! Layouts per `docs/audio/nsf/nsf-container-layout.md` and the
//! NSF/NSFe/NSF2 wiki snapshots.

use oxideav_nsf::header::NSF_HEADER_LEN;
use oxideav_nsf::nsfe::NsfeMetaError;
use oxideav_nsf::{parse_nsf, NsfError, NsfWriteError};

fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(body);
}

fn minimal_v1() -> Vec<u8> {
    let mut buf = vec![0u8; NSF_HEADER_LEN];
    buf[..5].copy_from_slice(b"NESM\x1a");
    buf[0x05] = 1;
    buf[0x06] = 1;
    buf[0x07] = 1;
    buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes());
    buf[0x0a..0x0c].copy_from_slice(&0x8000u16.to_le_bytes());
    buf[0x0c..0x0e].copy_from_slice(&0x8003u16.to_le_bytes());
    buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes());
    buf.extend_from_slice(&[0x78, 0xd8, 0x60, 0x60]);
    buf
}

// ----- NSFe chunk-framing hostility ------------------------------------

#[test]
fn truncated_chunk_header_at_every_length_is_rejected() {
    // NSFE magic followed by 1..=7 bytes: not enough for the 8-byte
    // length + fourcc chunk header.
    for extra in 1..8usize {
        let mut out = b"NSFE".to_vec();
        out.extend(std::iter::repeat(0xAAu8).take(extra));
        assert_eq!(
            parse_nsf(&out).unwrap_err(),
            NsfError::NsfeTruncatedChunk,
            "{extra} dangling bytes must be a truncated chunk header"
        );
    }
}

#[test]
fn bare_magic_is_missing_info_not_a_panic() {
    // Magic + zero chunks: hits the missing-required-chunk path (INFO
    // is checked before NEND).
    assert_eq!(
        parse_nsf(b"NSFE").unwrap_err(),
        NsfError::NsfeMissingRequired("INFO")
    );
}

#[test]
fn declared_size_running_past_buffer_is_rejected() {
    let mut out = b"NSFE".to_vec();
    // Chunk claims 100 bytes; only 4 follow.
    out.extend_from_slice(&100u32.to_le_bytes());
    out.extend_from_slice(b"INFO");
    out.extend_from_slice(&[0u8; 4]);
    assert_eq!(parse_nsf(&out).unwrap_err(), NsfError::NsfeChunkOverflow);
}

#[test]
fn u32_max_declared_size_is_rejected_not_allocated() {
    let mut out = b"NSFE".to_vec();
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(b"DATA");
    out.extend_from_slice(&[0u8; 16]);
    assert_eq!(parse_nsf(&out).unwrap_err(), NsfError::NsfeChunkOverflow);
}

#[test]
fn chunk_size_exactly_to_buffer_end_is_accepted() {
    // DATA body runs exactly to the end of the NEND-less region;
    // append NEND afterwards so only the boundary math is under test.
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    push_chunk(&mut out, b"INFO", &info);
    push_chunk(&mut out, b"DATA", &[0x60; 32]);
    push_chunk(&mut out, b"NEND", &[]);
    let h = parse_nsf(&out).unwrap();
    assert_eq!(h.program.len(), 32);

    // Same stream with the DATA length declared one byte long: the
    // fourcc of the following NEND chunk is swallowed into DATA's
    // body, so the walk desynchronizes and must fail (truncated
    // trailing header + missing NEND), never mis-parse.
    let data_len_offset = 4 + 8 + info.len();
    let bad = {
        let mut b = out.clone();
        b[data_len_offset..data_len_offset + 4].copy_from_slice(&33u32.to_le_bytes());
        b
    };
    assert!(parse_nsf(&bad).is_err());
}

#[test]
fn duplicate_info_and_data_last_one_wins() {
    // The spec doesn't define duplicates; the parser's documented
    // behavior is last-wins. Pin it so a change is deliberate.
    let mut out = b"NSFE".to_vec();
    let info_a: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    let info_b: [u8; 10] = [0x00, 0x90, 0x00, 0x90, 0x03, 0x90, 0x00, 0x00, 7, 2];
    push_chunk(&mut out, b"INFO", &info_a);
    push_chunk(&mut out, b"DATA", &[0x11]);
    push_chunk(&mut out, b"INFO", &info_b);
    push_chunk(&mut out, b"DATA", &[0x22, 0x33]);
    push_chunk(&mut out, b"NEND", &[]);
    let h = parse_nsf(&out).unwrap();
    assert_eq!(h.load_addr, 0x9000);
    assert_eq!(h.total_songs, 7);
    assert_eq!(h.starting_song, 2);
    assert_eq!(h.program, vec![0x22, 0x33]);
}

#[test]
fn thousands_of_unknown_optional_chunks_walk_linearly() {
    // A deep chain of skippable chunks must not recurse or blow up.
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    push_chunk(&mut out, b"INFO", &info);
    for _ in 0..5000 {
        push_chunk(&mut out, b"zzzz", &[0xEE; 3]);
    }
    push_chunk(&mut out, b"DATA", &[0x60]);
    push_chunk(&mut out, b"NEND", &[]);
    assert!(parse_nsf(&out).is_ok());
}

#[test]
fn non_letter_leading_fourcc_is_optional_and_skipped() {
    // The mandatory rule keys on 'A'-'Z' specifically; a digit or
    // high-bit leading byte is not uppercase, so the chunk is
    // skippable.
    for lead in [b'0', b'~', 0x80u8, 0x00] {
        let mut out = b"NSFE".to_vec();
        let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, &[lead, b'x', b'y', b'z'], &[1, 2, 3]);
        push_chunk(&mut out, b"DATA", &[0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        assert!(
            parse_nsf(&out).is_ok(),
            "leading byte {lead:#04x} must mark an optional chunk"
        );
    }
}

#[test]
fn zero_length_data_chunk_is_accepted() {
    // "There is no minimum length for this chunk".
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    push_chunk(&mut out, b"INFO", &info);
    push_chunk(&mut out, b"DATA", &[]);
    push_chunk(&mut out, b"NEND", &[]);
    let h = parse_nsf(&out).unwrap();
    assert!(h.program.is_empty());
}

#[test]
fn oversized_bank_chunk_ignores_extra_bytes() {
    // "If longer, ignore the extra bytes."
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    push_chunk(&mut out, b"INFO", &info);
    let bank: Vec<u8> = (0..12).collect();
    push_chunk(&mut out, b"BANK", &bank);
    push_chunk(&mut out, b"DATA", &[0x60]);
    push_chunk(&mut out, b"NEND", &[]);
    let h = parse_nsf(&out).unwrap();
    assert_eq!(h.bankswitch_init, [0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn short_bank_chunk_presumes_zero_for_missing_bytes() {
    // "If this chunk is less than 8 bytes, presume 0".
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
    push_chunk(&mut out, b"INFO", &info);
    push_chunk(&mut out, b"BANK", &[9, 8]);
    push_chunk(&mut out, b"DATA", &[0x60]);
    push_chunk(&mut out, b"NEND", &[]);
    let h = parse_nsf(&out).unwrap();
    assert_eq!(h.bankswitch_init, [9, 8, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn malformed_extended_chunk_payloads_error_through_the_container() {
    // Misaligned mixe / undersized RATE / empty regn / bad VRC7 patch
    // length, each embedded in an otherwise-valid file, must surface
    // the metadata error (not panic, not silently decode).
    let cases: &[(&[u8; 4], Vec<u8>)] = &[
        (b"mixe", vec![0x00, 0x01]),    // not a multiple of 3
        (b"RATE", vec![0x1a]),          // below 4-byte minimum
        (b"regn", vec![]),              // missing bitfield byte
        (b"VRC7", vec![0x00; 1 + 130]), // patch len 130 (not 128/152)
        (b"VRC7", vec![]),              // missing device byte
    ];
    for (tag, body) in cases {
        let mut out = b"NSFE".to_vec();
        let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x00, 0x00, 1, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, tag, body);
        push_chunk(&mut out, b"DATA", &[0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        match parse_nsf(&out).unwrap_err() {
            NsfError::Metadata(NsfeMetaError::BadChunkPayload { tag: t, len }) => {
                assert_eq!(&t, *tag);
                assert_eq!(len, body.len());
            }
            other => panic!("{:?} payload len {} gave {other:?}", tag, body.len()),
        }
    }
}

// ----- NSF v1 / NSF2 fixed-header hostility -----------------------------

#[test]
fn v1_exact_header_length_boundary() {
    // 0x7F bytes: one short of the fixed header.
    let mut buf = minimal_v1();
    buf.truncate(NSF_HEADER_LEN - 1);
    assert_eq!(
        parse_nsf(&buf).unwrap_err(),
        NsfError::TooShort {
            needed: NSF_HEADER_LEN,
            got: NSF_HEADER_LEN - 1
        }
    );

    // Exactly 0x80 bytes: valid, empty program.
    let mut buf = minimal_v1();
    buf.truncate(NSF_HEADER_LEN);
    let h = parse_nsf(&buf).unwrap();
    assert!(h.program.is_empty());
}

#[test]
fn nsf2_declared_length_boundaries() {
    // declared == available: all program, no metadata.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4; // program is 4 bytes
    let h = parse_nsf(&buf).unwrap();
    assert_eq!(h.program.len(), 4);
    assert!(h.nsf2_metadata.is_empty());

    // declared == available + 1: overflow.
    buf[0x7d] = 5;
    assert_eq!(
        parse_nsf(&buf).unwrap_err(),
        NsfError::Nsf2DataLengthOverflow {
            declared: 5,
            available: 4
        }
    );
}

#[test]
fn nsf2_hostile_appended_metadata_is_rejected() {
    // Appended metadata whose single chunk header is truncated.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    buf.extend_from_slice(&[0x01, 0x02, 0x03]); // 3 dangling bytes
    assert_eq!(
        parse_nsf(&buf).unwrap_err(),
        NsfError::Metadata(NsfeMetaError::TruncatedChunk)
    );

    // Appended metadata containing the chunks forbidden in the
    // embedded context. Per nsf-container-layout.md §2.6 the list is
    // exactly four — INFO, DATA, BANK, NSF2 — and each gets the
    // dedicated ForbiddenChunk error.
    for tag in [b"INFO", b"DATA", b"BANK", b"NSF2"] {
        let mut buf = minimal_v1();
        buf[0x05] = 2;
        buf[0x7d] = 4;
        let mut meta = Vec::new();
        push_chunk(&mut meta, tag, &[0u8; 10]);
        push_chunk(&mut meta, b"NEND", &[]);
        buf.extend_from_slice(&meta);
        assert_eq!(
            parse_nsf(&buf).unwrap_err(),
            NsfError::Metadata(NsfeMetaError::ForbiddenChunk(*tag)),
            "{} must be forbidden in NSF2 embedded metadata",
            core::str::from_utf8(tag).unwrap()
        );
    }

    // An uppercase chunk that is merely *unknown* (not on the
    // forbidden-four list) keeps the distinct unknown-mandatory error.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    push_chunk(&mut meta, b"QQQQ", &[]);
    push_chunk(&mut meta, b"NEND", &[]);
    buf.extend_from_slice(&meta);
    assert_eq!(
        parse_nsf(&buf).unwrap_err(),
        NsfError::Metadata(NsfeMetaError::UnknownMandatory(*b"QQQQ"))
    );
}

#[test]
fn nsf2_metadata_nend_semantics() {
    // §2.6: NEND is not forbidden — it is the expected terminator,
    // and nothing is read past it. A truncated chunk header after
    // NEND must therefore be ignored, not rejected.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    push_chunk(&mut meta, b"auth", b"Embedded\0\0\0\0");
    push_chunk(&mut meta, b"NEND", &[]);
    meta.extend_from_slice(&[0xff, 0x01, 0xde]); // hostile trailing bytes
    buf.extend_from_slice(&meta);
    let h = parse_nsf(&buf).unwrap();
    assert_eq!(h.song_name, "Embedded");

    // "Should end with an NEND chunk" is advisory: a chunk run that
    // simply ends at the file boundary still parses.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    push_chunk(&mut meta, b"auth", b"NoNend\0\0\0\0");
    buf.extend_from_slice(&meta);
    let h = parse_nsf(&buf).unwrap();
    assert_eq!(h.song_name, "NoNend");
}

#[test]
fn nsf2_mandatory_metadata_bit_with_known_mandatory_chunk() {
    // §2.6 mandatory chunks: header byte $7C bit 7 declares the
    // appended metadata may contain a mandatory (uppercase-initial)
    // chunk required for playback — the wiki's worked example is a
    // VRC7 chunk substituting YM2413 for VRC7. A player that
    // understands it (we do) plays the file; the chunk's payload must
    // be applied, not merely tolerated.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7c] = 0x80; // mandatory-metadata feature bit
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    push_chunk(&mut meta, b"VRC7", &[1u8]); // device 1 = YM2413
    push_chunk(&mut meta, b"NEND", &[]);
    buf.extend_from_slice(&meta);
    let h = parse_nsf(&buf).unwrap();
    assert!(h.nsf2.mandatory_metadata());
    assert_eq!(h.metadata.vrc7.as_ref().unwrap().device, 1);

    // The same file with a mandatory chunk we do NOT understand must
    // be rejected — bit 7 says it is required for correct playback.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7c] = 0x80;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    push_chunk(&mut meta, b"XSND", &[0u8; 3]);
    push_chunk(&mut meta, b"NEND", &[]);
    buf.extend_from_slice(&meta);
    assert_eq!(
        parse_nsf(&buf).unwrap_err(),
        NsfError::Metadata(NsfeMetaError::UnknownMandatory(*b"XSND"))
    );
}

#[test]
fn nsf2_rate_and_regn_are_legal_in_appended_metadata() {
    // Per the NSF2 wiki §Metadata, RATE and regn "may be included to
    // provide additional Dendy region playback information".
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    let mut rate = Vec::new();
    rate.extend_from_slice(&16666u16.to_le_bytes());
    rate.extend_from_slice(&19997u16.to_le_bytes());
    rate.extend_from_slice(&19120u16.to_le_bytes());
    push_chunk(&mut meta, b"RATE", &rate);
    push_chunk(&mut meta, b"regn", &[0x07, 0x02]); // prefer Dendy
    push_chunk(&mut meta, b"NEND", &[]);
    buf.extend_from_slice(&meta);
    let h = parse_nsf(&buf).unwrap();
    assert_eq!(h.region, oxideav_nsf::NsfRegion::Dendy);
    assert_eq!(h.metadata.rate.unwrap().dendy_us, Some(19120));
    assert_eq!(h.play_period_us(), 19120);
    // §2.6: the embedded RATE is partially redundant with the header
    // $6E/$78 words — the chunk overrides them when present.
    assert_eq!(h.ntsc_speed_us, 16666);
    assert_eq!(h.pal_speed_us, 19997);

    // Minimum 4-byte RATE (no Dendy word) + Dendy regn: the region
    // sticks, and the play period falls back to the PAL word per the
    // documented Dendy fallback chain.
    let mut buf = minimal_v1();
    buf[0x05] = 2;
    buf[0x7d] = 4;
    let mut meta = Vec::new();
    let mut rate = Vec::new();
    rate.extend_from_slice(&16666u16.to_le_bytes());
    rate.extend_from_slice(&19997u16.to_le_bytes());
    push_chunk(&mut meta, b"RATE", &rate);
    push_chunk(&mut meta, b"regn", &[0x07, 0x02]);
    push_chunk(&mut meta, b"NEND", &[]);
    buf.extend_from_slice(&meta);
    let h = parse_nsf(&buf).unwrap();
    assert_eq!(h.region, oxideav_nsf::NsfRegion::Dendy);
    assert_eq!(h.play_period_us(), 19997);
}

// ----- writer hostility --------------------------------------------------

#[test]
fn writer_rejects_program_over_24_bit_length_only_when_metadata_needs_it() {
    let mut h = parse_nsf(&minimal_v1()).unwrap();
    h.program = vec![0x60; 0x100_0000]; // 16 MiB + 1 over the field? exactly 2^24

    // Without metadata the 24-bit length stays zero ("until EOF") and
    // any program size serializes.
    let bytes = h.write_nsf().unwrap();
    assert_eq!(bytes.len(), NSF_HEADER_LEN + 0x100_0000);

    // With metadata the length field must delimit the program, so the
    // same program is now unrepresentable.
    h.track_labels = vec!["One".into()];
    assert_eq!(
        h.write_nsf().unwrap_err(),
        NsfWriteError::ProgramTooLong { len: 0x100_0000 }
    );

    // One byte under the limit fits.
    h.program = vec![0x60; 0xFF_FFFF];
    let bytes = h.write_nsf().unwrap();
    let h2 = parse_nsf(&bytes).unwrap();
    assert_eq!(h2.program.len(), 0xFF_FFFF);
    assert_eq!(h2.track_labels, vec!["One".to_string()]);
}

// ----- deterministic mutation round-trip sweep ---------------------------

/// Minimal xorshift generator (same shape as `tests/parse_fuzz.rs`)
/// so the sweep is reproducible without a rand dependency.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
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
}

/// Mirror of the `roundtrip` libfuzzer target's contract, driven by
/// deterministic mutations so CI exercises it on every run: whatever
/// parses must survive write → parse → write byte-identically.
fn assert_roundtrip_contract(bytes: &[u8]) {
    let Ok(header) = parse_nsf(bytes) else {
        return;
    };

    let w1 = header
        .write_nsfe()
        .expect("write_nsfe must succeed for any parsed header");
    let h2 = parse_nsf(&w1).expect("write_nsfe output must re-parse");
    let w2 = h2.write_nsfe().expect("second write_nsfe must succeed");
    assert_eq!(w1, w2, "write_nsfe must be byte-idempotent");

    match header.write_nsf() {
        Ok(w1) => {
            let h2 = parse_nsf(&w1).expect("write_nsf output must re-parse");
            let w2 = h2.write_nsf().expect("second write_nsf must succeed");
            assert_eq!(w1, w2, "write_nsf must be byte-idempotent");
        }
        Err(NsfWriteError::BadVersion(_)) | Err(NsfWriteError::StringTooLong { .. }) => {}
        Err(e) => panic!("unexpected write_nsf failure on a parsed header: {e}"),
    }
}

#[test]
fn mutated_v1_headers_uphold_the_roundtrip_contract() {
    let base = minimal_v1();
    // Every single-byte mutation of the header region.
    let mut rng = Lcg::new(0xC0FF_EE00);
    for offset in 0..NSF_HEADER_LEN {
        for _ in 0..3 {
            let mut buf = base.clone();
            buf[offset] = rng.next_u8();
            assert_roundtrip_contract(&buf);
        }
    }
}

#[test]
fn mutated_nsfe_streams_uphold_the_roundtrip_contract() {
    let base = {
        // Reuse a chunk-rich stream: INFO + BANK + RATE + several
        // string/track chunks + DATA + NEND.
        let mut out = b"NSFE".to_vec();
        let info: [u8; 10] = [0x00, 0x80, 0x00, 0x80, 0x03, 0x80, 0x02, 0x03, 3, 1];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"BANK", &[0, 1, 2, 3, 4, 5, 6, 7]);
        let mut rate = Vec::new();
        rate.extend_from_slice(&16639u16.to_le_bytes());
        rate.extend_from_slice(&19997u16.to_le_bytes());
        push_chunk(&mut out, b"RATE", &rate);
        push_chunk(&mut out, b"auth", b"T\0A\0C\0R\0");
        push_chunk(&mut out, b"tlbl", b"a\0b\0c\0");
        let mut time = Vec::new();
        for v in [1000i32, -1, 0] {
            time.extend_from_slice(&v.to_le_bytes());
        }
        push_chunk(&mut out, b"time", &time);
        push_chunk(&mut out, b"plst", &[0, 2]);
        push_chunk(&mut out, b"DATA", &[0x78, 0xd8, 0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        out
    };

    let mut rng = Lcg::new(0xDEAD_BEA7);
    for offset in 0..base.len() {
        for _ in 0..3 {
            let mut buf = base.clone();
            buf[offset] = rng.next_u8();
            assert_roundtrip_contract(&buf);
        }
    }
}
