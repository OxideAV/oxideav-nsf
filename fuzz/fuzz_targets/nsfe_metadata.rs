#![no_main]

//! Coverage-guided fuzz harness for the NSFe chunk walker + extended
//! metadata decoders, reached through the public `parse_nsf` entry.
//!
//! We prepend the `NSFE` magic and a minimal well-formed `INFO` + `DATA`
//! pair (so the container reaches the extended-metadata stage), then
//! append the fuzzer's bytes as the raw chunk stream. This exercises the
//! `auth`/`time`/`fade`/`plst`/`psfx`/`mixe`/`regn`/`RATE`/`VRC7` chunk
//! sub-parsers — each of which reads attacker-controlled lengths and
//! splits null-terminated string runs — without exposing any private
//! API.
//!
//! Contract under test: any chunk stream yields `Ok` or
//! `Err(NsfError)`; the metadata sub-parsers never panic on a hostile
//! payload (empty bodies, oversized declared sizes, unterminated string
//! runs, misaligned `mixe`/`i32`-array bodies, …).

use libfuzzer_sys::fuzz_target;
use oxideav_nsf::parse_nsf;

/// Emit a 4-byte LE size + 4-char tag + body as an NSFe chunk.
fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(body);
}

fuzz_target!(|data: &[u8]| {
    let mut buf = b"NSFE".to_vec();

    // Minimal INFO: load/init/play addresses (LE u16 ×3), region byte,
    // song count, start song. 9 bytes is the documented minimum.
    let info: [u8; 9] = [
        0x00, 0x80, // load $8000
        0x00, 0x80, // init $8000
        0x03, 0x80, // play $8003
        0x00, // region (NTSC)
        0x01, // song count
        0x00, // start song
    ];
    chunk(&mut buf, b"INFO", &info);

    // Minimal DATA program: SEI; CLD; RTS; RTS.
    chunk(&mut buf, b"DATA", &[0x78, 0xD8, 0x60, 0x60]);

    // The fuzzer drives the remaining (metadata) chunk stream verbatim.
    buf.extend_from_slice(data);

    let _ = parse_nsf(&buf);
});
