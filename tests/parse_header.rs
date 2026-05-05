//! End-to-end parse + render smoke test using a hand-built NSF blob.
//!
//! No real-world NSF rip is bundled — round 1 verifies the pipeline by
//! constructing a tiny synthetic NSF whose init routine programs the
//! 2A03 pulse 1 channel with a fixed period and duty. Stepping the CPU
//! at 1.789 MHz for ~93 ms of wall-clock time should yield non-trivial
//! audio output.

use oxideav_nsf::{parse_nsf, NsfPlayer, NsfRegion};

/// Manually-assembled NSF with all the documented header fields set so
/// the parser exercises every byte-position offset we care about.
fn synth_nsf() -> Vec<u8> {
    let mut header = vec![0u8; 0x80];
    header[..5].copy_from_slice(b"NESM\x1a");
    header[0x05] = 1; // version
    header[0x06] = 1; // total_songs
    header[0x07] = 1; // starting_song
    header[0x08..0x0A].copy_from_slice(&0x8000u16.to_le_bytes());
    header[0x0A..0x0C].copy_from_slice(&0x8000u16.to_le_bytes());
    header[0x0C..0x0E].copy_from_slice(&0x8030u16.to_le_bytes());
    header[0x0E..0x14].copy_from_slice(b"OxNSF\0");
    header[0x2E..0x36].copy_from_slice(b"OxideAV\0");
    header[0x4E..0x56].copy_from_slice(b"2026 KL\0");
    header[0x6E..0x70].copy_from_slice(&16666u16.to_le_bytes());
    header[0x78..0x7A].copy_from_slice(&19997u16.to_le_bytes());
    header[0x7A] = 0; // NTSC
    header[0x7B] = 0; // no expansion

    // 6502 program at $8000:
    //   init:  LDA #$01     ; enable pulse 1
    //          STA $4015
    //          LDA #$BF     ; duty 50, halt, constant volume 15
    //          STA $4000
    //          LDA #$40     ; period lo
    //          STA $4002
    //          LDA #$00     ; period hi
    //          STA $4003
    //          RTS
    //   play:  NOP / RTS
    let prog: Vec<u8> = vec![
        // $8000 init
        0xA9, 0x01, 0x8D, 0x15, 0x40, 0xA9, 0xBF, 0x8D, 0x00, 0x40, 0xA9, 0x40, 0x8D, 0x02, 0x40,
        0xA9, 0x00, 0x8D, 0x03, 0x40,
        0x60, // RTS
              // pad to $8010 (offset 16 from $8000)
    ];
    let mut blob = header.clone();
    blob.extend_from_slice(&prog);
    while blob.len() < 0x80 + 0x30 {
        blob.push(0xEA); // NOP
    }
    // play at $8030: NOP, RTS.
    blob.push(0xEA);
    blob.push(0x60);
    blob
}

#[test]
fn parse_synthetic_header_fields() {
    let bytes = synth_nsf();
    let h = parse_nsf(&bytes).unwrap();
    assert_eq!(h.version, 1);
    assert_eq!(h.total_songs, 1);
    assert_eq!(h.starting_song, 1);
    assert_eq!(h.load_addr, 0x8000);
    assert_eq!(h.init_addr, 0x8000);
    assert_eq!(h.play_addr, 0x8030);
    assert_eq!(h.song_name, "OxNSF");
    assert_eq!(h.artist, "OxideAV");
    assert_eq!(h.copyright, "2026 KL");
    assert_eq!(h.ntsc_speed_us, 16666);
    assert_eq!(h.pal_speed_us, 19997);
    assert_eq!(h.region, NsfRegion::Ntsc);
    assert!(!h.has_expansion());
    assert!(!h.is_nsfe);
    assert!(h.program.len() >= 0x12);
}

#[test]
fn end_to_end_render_emits_pcm() {
    let bytes = synth_nsf();
    let header = parse_nsf(&bytes).unwrap();
    let mut player = NsfPlayer::new(header, 44_100);
    player.start_song(1);
    let mut pcm = vec![0i16; 4096]; // ~93 ms
    let n = player.render(&mut pcm);
    assert_eq!(n, pcm.len(), "player halted prematurely");

    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "no non-zero samples produced");

    let peak = pcm.iter().map(|s| s.unsigned_abs() as u32).max().unwrap();
    assert!(
        peak > 1000,
        "peak {peak} too quiet for a 50% pulse @ vol 15"
    );

    // The test pulse runs at constant amplitude, so the average abs
    // value should be reasonable (not just transient noise).
    let mean_abs: f64 = pcm.iter().map(|s| s.unsigned_abs() as f64).sum::<f64>() / pcm.len() as f64;
    assert!(mean_abs > 200.0, "mean amplitude {mean_abs} too low");
}
