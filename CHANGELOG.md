# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1] - 2026-05-04

### Added

- Initial bootstrap of the `oxideav-nsf` crate (clean-room from nesdev.org wiki).
- 128-byte NSF v1.x header parser (`parse_nsf`) — magic `NESM\x1A`, version,
  total/starting song, load/init/play addresses, song name / artist /
  copyright (32 B each, ASCII / Latin-1, NUL-trimmed), NTSC / PAL playback
  speed (16-bit LE microseconds per playback period), bankswitch_init
  (8 B), region flags, expansion-chip flag bits (VRC6 / VRC7 / FDS / MMC5 /
  N163 / Sunsoft 5B), reserved bytes.
- NSFe chunk-based extension parser (magic `NSFE`) — parses INFO + DATA
  + auth + tlbl chunks. Unknown non-mandatory chunks are skipped per spec;
  unknown mandatory chunks (uppercase first letter) are rejected.
- 6502 CPU emulator (`Cpu6502`) — 151 of 256 official opcodes (all
  documented mnemonics including ADC/SBC decimal-mode-disabled NES
  variant, BRK, RTI, plus all addressing-mode permutations of LDA/STA/
  LDX/STX/LDY/STY/AND/ORA/EOR/CMP/CPX/CPY/INC/DEC/INX/INY/DEX/DEY/ASL/
  LSR/ROL/ROR/JMP/JSR/RTS/branches/flag ops/transfers/stack ops/NOP).
  Cycle-counting (not cycle-accurate inside the instruction) at base
  cycle counts, with page-crossing penalties on indexed reads + branch
  taken/page-cross penalties.
- 2A03 APU emulator (`Apu2A03`) — pulse 1, pulse 2, triangle, noise
  channels with envelope / sweep / length-counter / linear-counter logic
  per nesdev.org/wiki/APU. DMC channel partial (DAC + frame-counter
  registration only; no sample-fetch DMA in round 1). Non-linear
  mixer per nesdev.org/wiki/APU_Mixer (closed-form approximation).
  4-step / 5-step frame counter.
- `NsfPlayer` glue — loads a parsed NSF, primes the 2 KiB NES RAM and
  zero-page registers, runs the init routine for the chosen song, then
  emits PCM by stepping the CPU + APU at the play rate (NTSC 60 Hz
  default; PAL 50 Hz when bit 0 of the region flag is set). Output
  resampled by linear interpolation to 44 100 Hz mono.
- `Decoder` trait implementation gated on default-on `registry` feature
  (codec id `nsf`, audio, S16 mono @ 44 100 Hz). Container demuxer
  reads the whole file as a single packet (NSF has no internal framing).
- Self-test corpus: a hand-built NSF in `tests/parse_header.rs` exercises
  every header field and the round-trip CPU/APU pipeline.
