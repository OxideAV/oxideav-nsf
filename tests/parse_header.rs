//! End-to-end parse + render smoke test using a hand-built NSF blob.
//!
//! No real-world NSF rip is bundled — round 1 verifies the pipeline by
//! constructing a tiny synthetic NSF whose init routine programs the
//! 2A03 pulse 1 channel with a fixed period and duty. Stepping the CPU
//! at 1.789 MHz for ~93 ms of wall-clock time should yield non-trivial
//! audio output.

use oxideav_nsf::{parse_nsf, Nsf2Features, NsfPlayer, NsfRegion};

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

/// Build a NSF2 file that uses the IRQ-timer device to drive APU
/// register pokes from an IRQ service routine. INIT writes the IRQ
/// vector, primes the timer at a fast period, then CLI-and-RTS.
/// The IRQ handler bumps a counter at `$02` so we can prove it fired,
/// then poke `$4000` to keep pulse 1 audible, and RTI's after
/// acknowledging $401D.
///
/// Per `docs/audio/nsf/nsf2-nesdev-wiki.html`:
///   - `$7C` bit 4 (IRQ support) is set.
///   - `$401B/$401C` = reload; `$401D` = activate / ack.
///   - `$FFFE/$FFFF` = IRQ vector (program-owned).
fn synth_nsf2_irq() -> Vec<u8> {
    let mut header = vec![0u8; 0x80];
    header[..5].copy_from_slice(b"NESM\x1a");
    header[0x05] = 2; // NSF2
    header[0x06] = 1;
    header[0x07] = 1;
    header[0x08..0x0A].copy_from_slice(&0x8000u16.to_le_bytes());
    header[0x0A..0x0C].copy_from_slice(&0x8000u16.to_le_bytes()); // init
    header[0x0C..0x0E].copy_from_slice(&0x8060u16.to_le_bytes()); // play (no-op)
    header[0x6E..0x70].copy_from_slice(&16666u16.to_le_bytes());
    header[0x7B] = 0;
    header[0x7C] = 0x10; // IRQ support
    header[0x7D] = 0;
    header[0x7E] = 0;
    header[0x7F] = 0;

    // INIT at $8000: install IRQ vector, enable APU pulse 1, arm timer.
    //
    //   A9 40       LDA #$40        ; lo byte of IRQ handler at $8040
    //   8D FE FF    STA $FFFE       ; install handler (vector overlay RAM)
    //   A9 80       LDA #$80
    //   8D FF FF    STA $FFFF
    //   A9 01       LDA #$01
    //   8D 15 40    STA $4015       ; enable pulse 1
    //   A9 BF       LDA #$BF
    //   8D 00 40    STA $4000       ; pulse 1: duty 50, halt, vol 15
    //   A9 40       LDA #$40
    //   8D 02 40    STA $4002       ; period lo
    //   A9 00       LDA #$00
    //   8D 03 40    STA $4003       ; period hi + length-counter load
    //   A9 64       LDA #$64        ; timer reload = 100
    //   8D 1B 40    STA $401B
    //   A9 00       LDA #$00
    //   8D 1C 40    STA $401C
    //   A9 01       LDA #$01
    //   8D 1D 40    STA $401D       ; activate timer
    //   58          CLI             ; enable IRQs
    //   60          RTS
    let init: Vec<u8> = vec![
        0xA9, 0x40, 0x8D, 0xFE, 0xFF, // STA $FFFE
        0xA9, 0x80, 0x8D, 0xFF, 0xFF, // STA $FFFF
        0xA9, 0x01, 0x8D, 0x15, 0x40, // STA $4015
        0xA9, 0xBF, 0x8D, 0x00, 0x40, // STA $4000
        0xA9, 0x40, 0x8D, 0x02, 0x40, // STA $4002
        0xA9, 0x00, 0x8D, 0x03, 0x40, // STA $4003
        0xA9, 0x64, 0x8D, 0x1B, 0x40, // STA $401B (reload lo = 100)
        0xA9, 0x00, 0x8D, 0x1C, 0x40, // STA $401C (reload hi)
        0xA9, 0x01, 0x8D, 0x1D, 0x40, // STA $401D (activate)
        0x58, // CLI
        0x60, // RTS
    ];

    // IRQ handler at $8040:
    //   48          PHA
    //   E6 02       INC $02          ; bump IRQ-fire counter
    //   AD 1D 40    LDA $401D        ; acknowledge IRQ flag
    //   68          PLA
    //   40          RTI
    let irq: Vec<u8> = vec![0x48, 0xE6, 0x02, 0xAD, 0x1D, 0x40, 0x68, 0x40];

    // PLAY at $8060: no-op.
    let play: Vec<u8> = vec![0xEA, 0x60];

    let mut prog = vec![0xEAu8; 0x70];
    prog[..init.len()].copy_from_slice(&init);
    prog[0x40..0x40 + irq.len()].copy_from_slice(&irq);
    prog[0x60..0x60 + play.len()].copy_from_slice(&play);

    let mut blob = header;
    blob.extend_from_slice(&prog);
    blob
}

#[test]
fn nsf2_irq_handler_fires_under_timer() {
    let bytes = synth_nsf2_irq();
    let header = parse_nsf(&bytes).unwrap();
    assert_eq!(header.version, 2);
    assert_eq!(header.nsf2, Nsf2Features(0x10));
    let mut player = NsfPlayer::new(header, 44_100);
    player.start_song(1);
    // Render a small window first, then assert the counter strictly
    // increased from zero (the IRQ handler runs `INC $02` every time
    // the NSF2 timer underflows; the byte will wrap many times across
    // larger renders, so we sample early).
    let mut probe = vec![0i16; 128];
    let _ = player.render(&mut probe);
    assert!(
        player.bus.ram[0x02] > 0,
        "IRQ handler must have fired during first 128 samples; got $02 = {}",
        player.bus.ram[0x02]
    );
    // Finish rendering to verify pulse 1 stays audible while IRQs fire.
    let mut pcm = vec![0i16; 4096];
    let _ = player.render(&mut pcm);
    let peak = pcm.iter().map(|s| s.unsigned_abs() as u32).max().unwrap();
    assert!(peak > 1000, "IRQ-driven NSF2 produced too-quiet audio");
}
