#![no_main]

//! Coverage-guided fuzz harness for the full `parse_nsf` → `NsfPlayer`
//! render path.
//!
//! This is the highest-value NSF target: for any byte slice that
//! `parse_nsf` accepts, we build an [`NsfPlayer`], start a song, and
//! render a bounded buffer. That runs the 6502 core, the 2A03 APU, and
//! every expansion chip the header's `$7B` mask enables against whatever
//! 6502 program bytes the fuzzer produced — the surface most likely to
//! hide a panic (bad opcode timing, out-of-range bank select, an
//! expansion register edge, an empty-RAM FDS access, …).
//!
//! Contract under test: building + starting + rendering never panics and
//! always returns within a bounded number of samples (the player is
//! halt-guarded, so a program that never returns from PLAY must not
//! wedge the render loop). `render` must never report more samples than
//! the output buffer holds.

use libfuzzer_sys::fuzz_target;
use oxideav_nsf::{parse_nsf, NsfPlayer};

fuzz_target!(|data: &[u8]| {
    let header = match parse_nsf(data) {
        Ok(h) => h,
        Err(_) => return,
    };

    // First playable song (1-based). `start_song` clamps internally.
    let song = header.total_songs.max(1).min(8);
    let mut player = NsfPlayer::new(header, 44_100);
    player.start_song(song);

    let mut out = [0i16; 2048];
    let produced = player.render(&mut out);
    assert!(produced <= out.len());

    // Out-of-range playlist indices must return None, not panic.
    let _ = player.playlist_song(usize::MAX);
    let _ = player.sfx_playlist_song(usize::MAX);
});
