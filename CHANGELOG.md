# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- 6502 CPU emulator now covers **all 256 opcodes** (round 2): real
  semantics for the unofficial / "illegal" opcodes per
  nesdev.org/wiki/CPU_unofficial_opcodes. Stable group: LAX, SAX,
  DCP, ISB/ISC, SLO, RLA, SRE, RRA, ANC, ALR, ARR, SBX/AXS, the
  duplicate SBC (`$EB`), full multi-byte NOP variants, KIL/JAM
  (latches the `halted` bit). Unstable group: SHA, SHX, SHY, TAS,
  LAS, ANE/XAA, LXA — implemented with the deterministic
  "magic = 0xFF" interpretation documented on the wiki.
- 2A03 APU **DMC channel completed** (round 2): sample-fetch DMA
  via the bus, NTSC + PAL rate tables, looping flag, IRQ flag latched
  + cleared on `$4015` read, 1-bit delta DAC. The bus drains the
  per-cycle pending-fetch queue inside `tick_cycles`.
- New `bus` features: 4 KiB-bank pool + bank-select registers
  (`$5FF8..=$5FFF`) wired off the NSF header's `bankswitch_init`
  array. FDS extends with `$5FF6..=$5FF7` for `$6000`/`$7000` window
  banking.
- New `expansion` module covering all six NSF expansion chips:
  - **VRC6** — 2 pulses + sawtooth (`$9000..=$B002`).
  - **MMC5** — 2 pulses + 8-bit raw PCM (`$5000..=$5015`).
  - **Sunsoft 5B** — 3 squares with AY-style log-volume envelopes.
  - **Namco 163** — wavetable RAM with `$F800` pointer addressing.
  - **VRC7** — 6 FM channels driven from `$9010` / `$9030` register
    indirection. Round 2 ships a coarse 2-operator approximation in
    place of the full OPLL operator chain.
  - **FDS** — wavetable + frequency modulator (`$4040..=$4089`).
- Real-rip integration test (`tests/real_rip.rs`): downloads
  `chibi-tech_-_miko_miko_nurse.nsf` from `samples.oxideav.org` and
  asserts 30 wall-clock seconds (~1.32 M PCM samples at 44.1 kHz)
  render without panic + with non-trivial audio. Network gated by
  `OXIDEAV_NETWORK_TESTS=1`; cached in `target/test-fixtures/`.
  Adds `ureq` as a dev-dependency.
- New unit-test coverage: 9 unofficial-opcode CPU tests, 4 DMC tests,
  6 expansion-chip tests.

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
