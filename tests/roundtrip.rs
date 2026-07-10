//! Container round-trip tests: `parse_nsf` ∘ `write_nsf` /
//! `write_nsfe` must preserve every semantic field, and the writers'
//! canonical encodings must be byte-idempotent
//! (`write(parse(write(h))) == write(h)`).
//!
//! Layouts per `docs/audio/nsf/nsf-container-layout.md` and the
//! NSF/NSFe/NSF2 wiki snapshots in `docs/audio/nsf/`.

use oxideav_nsf::header::NSF_HEADER_LEN;
use oxideav_nsf::nsfe::{NsfeAuth, NsfeMixerEntry, NsfeRate, NsfeRegions, NsfeVrc7};
use oxideav_nsf::{parse_nsf, NsfRegion, NsfWriteError};

/// Emit one `[u32 len][fourcc][body]` chunk.
fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(body);
}

/// A v1 file exercising every fixed-header field.
fn v1_bytes() -> Vec<u8> {
    let mut buf = vec![0u8; NSF_HEADER_LEN];
    buf[..5].copy_from_slice(b"NESM\x1a");
    buf[0x05] = 1;
    buf[0x06] = 5; // total songs
    buf[0x07] = 3; // starting song, 1-based
    buf[0x08..0x0a].copy_from_slice(&0x8abcu16.to_le_bytes());
    buf[0x0a..0x0c].copy_from_slice(&0x9000u16.to_le_bytes());
    buf[0x0c..0x0e].copy_from_slice(&0xfffeu16.to_le_bytes());
    buf[0x0e..0x14].copy_from_slice(b"Muddle");
    buf[0x2e..0x36].copy_from_slice(b"Karpeles");
    buf[0x4e..0x52].copy_from_slice(b"2026");
    buf[0x6e..0x70].copy_from_slice(&16639u16.to_le_bytes());
    buf[0x70..0x78].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]); // banked
    buf[0x78..0x7a].copy_from_slice(&19997u16.to_le_bytes());
    buf[0x7a] = 0x02; // dual
    buf[0x7b] = 0x15; // VRC6 + FDS + N163
    buf.extend_from_slice(&[0x78, 0xd8, 0xa9, 0x0f, 0x60]);
    buf
}

/// A kitchen-sink NSFe exercising every chunk the parser understands.
fn nsfe_bytes() -> Vec<u8> {
    let mut out = b"NSFE".to_vec();
    let info: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x02, 0x03, 3, 1];
    push_chunk(&mut out, b"INFO", &info);
    push_chunk(&mut out, b"BANK", &[7, 6, 5, 4, 3, 2, 1, 0]);
    let mut rate = Vec::new();
    rate.extend_from_slice(&16639u16.to_le_bytes());
    rate.extend_from_slice(&19997u16.to_le_bytes());
    rate.extend_from_slice(&19120u16.to_le_bytes());
    push_chunk(&mut out, b"RATE", &rate);
    push_chunk(&mut out, b"NSF2", &[0x40]); // suppressed PLAY
    let mut vrc7 = vec![1u8]; // YM2413 variant
    vrc7.extend_from_slice(&[0u8; 8]);
    vrc7.extend((0..120).map(|i| (i * 3) as u8));
    push_chunk(&mut out, b"VRC7", &vrc7);
    push_chunk(&mut out, b"regn", &[0x07, 0x01]);
    push_chunk(&mut out, b"plst", &[2, 0, 1, 2]);
    push_chunk(&mut out, b"psfx", &[1]);
    let mut time = Vec::new();
    for v in [90_000i32, -1, 0] {
        time.extend_from_slice(&v.to_le_bytes());
    }
    push_chunk(&mut out, b"time", &time);
    let mut fade = Vec::new();
    for v in [4_000i32, 0, -1] {
        fade.extend_from_slice(&v.to_le_bytes());
    }
    push_chunk(&mut out, b"fade", &fade);
    push_chunk(&mut out, b"tlbl", b"Intro\0Boss fight\0Credits\0");
    push_chunk(&mut out, b"taut", b"Composer A\0Composer B\0");
    push_chunk(
        &mut out,
        b"auth",
        "Gamé\0Artist\0© 2026\0Ripper\0".as_bytes(),
    );
    push_chunk(&mut out, b"text", b"Two lines\r\nof notes\0");
    let mixe = [0u8, 0x00, 0x00, 6u8, 0x4c, 0x04]; // APU 0 mB, N163 +1100 mB
    push_chunk(&mut out, b"mixe", &mixe);
    push_chunk(&mut out, b"DATA", &[0x78, 0xd8, 0x60]);
    push_chunk(&mut out, b"NEND", &[]);
    out
}

#[test]
fn v1_parse_write_parse_preserves_all_fields() {
    let h1 = parse_nsf(&v1_bytes()).unwrap();
    let written = h1.write_nsf().unwrap();
    let h2 = parse_nsf(&written).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn v1_write_is_byte_idempotent_and_reproduces_canonical_input() {
    // v1_bytes is already in canonical form (padded NUL strings, no
    // metadata), so the writer must reproduce it exactly.
    let bytes = v1_bytes();
    let written = parse_nsf(&bytes).unwrap().write_nsf().unwrap();
    assert_eq!(written, bytes);
}

#[test]
fn nsfe_parse_write_parse_preserves_all_fields() {
    let h1 = parse_nsf(&nsfe_bytes()).unwrap();
    let written = h1.write_nsfe().unwrap();
    let h2 = parse_nsf(&written).unwrap();
    assert_eq!(h1, h2);

    // Spot-check the interesting decoded values survived the trip.
    assert_eq!(h2.total_songs, 3);
    assert_eq!(h2.starting_song_number(), 2); // 0-based 1 == track 2
    assert_eq!(h2.region, NsfRegion::Pal); // regn preferred = 1
    assert!(h2.expansion.vrc6() && h2.expansion.vrc7());
    assert_eq!(h2.bankswitch_init, [7, 6, 5, 4, 3, 2, 1, 0]);
    assert_eq!(h2.song_name, "Gamé");
    assert_eq!(h2.copyright, "© 2026");
    assert!(h2.nsf2.suppressed_play());
    assert_eq!(
        h2.metadata.rate,
        Some(NsfeRate {
            ntsc_us: Some(16639),
            pal_us: Some(19997),
            dendy_us: Some(19120),
        })
    );
    assert_eq!(
        h2.metadata.regions,
        Some(NsfeRegions {
            mask: 0x07,
            preferred: Some(1),
        })
    );
    assert_eq!(h2.track_labels, vec!["Intro", "Boss fight", "Credits"]);
    assert_eq!(h2.metadata.track_authors, vec!["Composer A", "Composer B"]);
    assert_eq!(h2.metadata.track_times_ms, vec![90_000, -1, 0]);
    assert_eq!(h2.metadata.track_fades_ms, vec![4_000, 0, -1]);
    assert_eq!(h2.metadata.playlist, vec![2, 0, 1, 2]);
    assert_eq!(h2.metadata.sfx_playlist, vec![1]);
    assert_eq!(
        h2.metadata.mixer,
        vec![
            NsfeMixerEntry {
                device: 0,
                millibel: 0
            },
            NsfeMixerEntry {
                device: 6,
                millibel: 1100
            },
        ]
    );
    let vrc7 = h2.metadata.vrc7.as_ref().unwrap();
    assert_eq!(vrc7.device, 1);
    assert_eq!(vrc7.patches.as_ref().map(Vec::len), Some(128));
    assert_eq!(h2.metadata.text.as_deref(), Some("Two lines\r\nof notes"));
    assert_eq!(
        h2.metadata.auth,
        Some(NsfeAuth {
            title: "Gamé".into(),
            artist: "Artist".into(),
            copyright: "© 2026".into(),
            ripper: "Ripper".into(),
        })
    );
}

#[test]
fn nsfe_write_is_byte_idempotent() {
    let h1 = parse_nsf(&nsfe_bytes()).unwrap();
    let w1 = h1.write_nsfe().unwrap();
    let w2 = parse_nsf(&w1).unwrap().write_nsfe().unwrap();
    assert_eq!(w1, w2);
}

#[test]
fn v1_to_nsfe_conversion_rehomes_header_fields_into_chunks() {
    // Convert a fixed-header file to NSFe: strings move to a
    // synthesized auth chunk, play periods to a RATE chunk, banks to
    // BANK, and the starting song re-bases from 1-based to 0-based.
    let h1 = parse_nsf(&v1_bytes()).unwrap();
    let nsfe = h1.write_nsfe().unwrap();
    let h2 = parse_nsf(&nsfe).unwrap();

    assert!(h2.is_nsfe);
    assert_eq!(h2.song_name, h1.song_name);
    assert_eq!(h2.artist, h1.artist);
    assert_eq!(h2.copyright, h1.copyright);
    assert_eq!(h2.metadata.auth.as_ref().unwrap().ripper, "");
    assert_eq!(h2.ntsc_speed_us, h1.ntsc_speed_us);
    assert_eq!(h2.pal_speed_us, h1.pal_speed_us);
    assert_eq!(h2.bankswitch_init, h1.bankswitch_init);
    assert_eq!(h2.region, h1.region);
    assert_eq!(h2.expansion, h1.expansion);
    assert_eq!(h2.total_songs, h1.total_songs);
    assert_eq!(h2.starting_song_number(), h1.starting_song_number());
    assert_eq!(h2.starting_song, 2); // raw NSFe byte is 0-based
    assert_eq!(h2.program, h1.program);
}

#[test]
fn nsfe_to_v1_conversion_preserves_metadata_via_appended_chunks() {
    // Convert the kitchen-sink NSFe to the fixed-header shape: the
    // per-track chunks survive as NSF2-style appended metadata, and
    // the 24-bit length at $7D-$7F delimits the program.
    let h1 = parse_nsf(&nsfe_bytes()).unwrap();
    let nsf = h1.write_nsf().unwrap();
    let h2 = parse_nsf(&nsf).unwrap();

    assert!(!h2.is_nsfe);
    assert_eq!(h2.program, h1.program);
    assert_eq!(h2.total_songs, h1.total_songs);
    assert_eq!(h2.starting_song_number(), h1.starting_song_number());
    assert_eq!(h2.song_name, h1.song_name);
    assert_eq!(h2.artist, h1.artist);
    assert_eq!(h2.copyright, h1.copyright);
    assert_eq!(h2.region, h1.region);
    assert_eq!(h2.ntsc_speed_us, h1.ntsc_speed_us);
    assert_eq!(h2.pal_speed_us, h1.pal_speed_us);
    assert_eq!(h2.bankswitch_init, h1.bankswitch_init);
    assert_eq!(h2.track_labels, h1.track_labels);
    assert_eq!(h2.metadata.track_authors, h1.metadata.track_authors);
    assert_eq!(h2.metadata.track_times_ms, h1.metadata.track_times_ms);
    assert_eq!(h2.metadata.track_fades_ms, h1.metadata.track_fades_ms);
    assert_eq!(h2.metadata.playlist, h1.metadata.playlist);
    assert_eq!(h2.metadata.mixer, h1.metadata.mixer);
    assert_eq!(h2.metadata.vrc7, h1.metadata.vrc7);
    assert_eq!(h2.metadata.text, h1.metadata.text);
    // The auth chunk rides along verbatim in the appended metadata.
    assert_eq!(h2.metadata.auth, h1.metadata.auth);
}

#[test]
fn dendy_region_survives_both_container_shapes() {
    // Dendy is only expressible through a regn chunk; the writers
    // must synthesize one when the header carries the region without
    // explicit regn metadata.
    let mut h = parse_nsf(&v1_bytes()).unwrap();
    h.region = NsfRegion::Dendy;
    assert!(h.metadata.regions.is_none());

    let h2 = parse_nsf(&h.write_nsf().unwrap()).unwrap();
    assert_eq!(h2.region, NsfRegion::Dendy);
    let regn = h2.metadata.regions.unwrap();
    assert!(regn.supports_dendy());
    assert_eq!(regn.preferred, Some(2));

    let h3 = parse_nsf(&h.write_nsfe().unwrap()).unwrap();
    assert_eq!(h3.region, NsfRegion::Dendy);
    assert_eq!(h3.metadata.regions.unwrap().preferred, Some(2));
}

#[test]
fn nsf2_features_and_appended_metadata_roundtrip() {
    let mut h = parse_nsf(&v1_bytes()).unwrap();
    h.version = 2;
    h.nsf2 = oxideav_nsf::Nsf2Features(0x90); // IRQ + mandatory metadata
    h.track_labels = vec!["One".into(), "Two".into()];

    let bytes = h.write_nsf().unwrap();
    // 24-bit length must delimit the program exactly.
    let declared = (bytes[0x7d] as usize) | ((bytes[0x7e] as usize) << 8);
    assert_eq!(declared, h.program.len());

    let h2 = parse_nsf(&bytes).unwrap();
    assert_eq!(h2.version, 2);
    assert_eq!(h2.nsf2, h.nsf2);
    assert_eq!(h2.track_labels, h.track_labels);
    assert_eq!(h2.program, h.program);
}

#[test]
fn v1_writer_rejects_out_of_contract_headers() {
    let good = parse_nsf(&v1_bytes()).unwrap();

    let mut h = good.clone();
    h.total_songs = 0;
    assert_eq!(h.write_nsf().unwrap_err(), NsfWriteError::NoSongs);
    assert_eq!(h.write_nsfe().unwrap_err(), NsfWriteError::NoSongs);

    let mut h = good.clone();
    h.version = 3;
    assert_eq!(h.write_nsf().unwrap_err(), NsfWriteError::BadVersion(3));

    let mut h = good.clone();
    h.nsf2 = oxideav_nsf::Nsf2Features(0x10); // features on a v1 header
    assert_eq!(h.write_nsf().unwrap_err(), NsfWriteError::FeaturesRequireV2);

    let mut h = good.clone();
    h.song_name = "x".repeat(32);
    assert_eq!(
        h.write_nsf().unwrap_err(),
        NsfWriteError::StringTooLong {
            field: "song name",
            len: 32
        }
    );

    // 31 bytes is the documented maximum and must succeed.
    let mut h = good.clone();
    h.song_name = "x".repeat(31);
    let h2 = parse_nsf(&h.write_nsf().unwrap()).unwrap();
    assert_eq!(h2.song_name, h.song_name);

    let mut h = good.clone();
    h.artist = "inter\0nul".into();
    assert_eq!(
        h.write_nsf().unwrap_err(),
        NsfWriteError::StringContainsNul { field: "artist" }
    );

    let mut h = good.clone();
    h.track_labels = vec!["bad\0label".into()];
    assert_eq!(
        h.write_nsfe().unwrap_err(),
        NsfWriteError::StringContainsNul {
            field: "tlbl label"
        }
    );

    let mut h = good.clone();
    h.metadata.vrc7 = Some(NsfeVrc7 {
        device: 0,
        patches: Some(vec![0u8; 100]),
    });
    assert_eq!(
        h.write_nsfe().unwrap_err(),
        NsfWriteError::BadVrc7PatchLen { len: 100 }
    );
}

#[test]
fn v1_string_fields_reencode_legacy_bytes_as_utf8() {
    // A legacy 8-bit-encoded header string (invalid UTF-8) decodes
    // via the byte-map fallback; writing re-encodes it as UTF-8 and
    // the trip converges after one pass.
    let mut bytes = v1_bytes();
    bytes[0x0e..0x12].copy_from_slice(b"caf\xe9");
    bytes[0x12..0x14].copy_from_slice(&[0, 0]);
    let h1 = parse_nsf(&bytes).unwrap();
    assert_eq!(h1.song_name, "café");
    let h2 = parse_nsf(&h1.write_nsf().unwrap()).unwrap();
    assert_eq!(h2.song_name, "café");
    assert_eq!(h1, h2);
}

#[test]
fn write_metadata_chunks_bare_run_reparses_via_public_parser() {
    // The standalone metadata serializer emits the NSF2
    // appended-metadata shape: a bare chunk run, no magic, no
    // INFO/DATA/BANK/NSF2 chunks.
    let h = parse_nsf(&nsfe_bytes()).unwrap();
    let blob = oxideav_nsf::write_metadata_chunks(&h.metadata, &h.track_labels).unwrap();
    let meta = oxideav_nsf::nsfe::parse_metadata_chunks(&blob).unwrap();
    assert_eq!(meta.track_authors, h.metadata.track_authors);
    assert_eq!(meta.track_times_ms, h.metadata.track_times_ms);
    assert_eq!(meta.rate, h.metadata.rate);
    assert_eq!(meta.auth, h.metadata.auth);
    // tlbl labels come back in the metadata struct (the header parser
    // is what hoists them out).
    assert_eq!(meta.track_labels, vec!["Intro", "Boss fight", "Credits"]);
}
