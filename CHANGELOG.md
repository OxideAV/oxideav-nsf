# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Dendy region support** (round 5): new `NsfRegion::Dendy` variant
  carrying the 1.773448 MHz CPU clock per
  `docs/audio/nsf/apu-pulse-wiki.html`. NSFe `regn` chunks with
  `preferred = 2` (both on header-level NSFe files and on NSF 2
  appended-metadata blobs) now promote the region to `Dendy` instead
  of folding onto PAL. The player honours the dedicated Dendy speed
  from the NSFe `RATE` chunk byte $0004 (with PAL fallback per spec)
  and seeds INIT with `X = 2` per `docs/audio/nsf/nsfe-nesdev-wiki.html`
  §regn. New `NsfHeader::play_period_us()` getter centralises the
  per-region speed-selection logic.
- **NSFe `mixe` per-device gain overrides** (round 5): `Apu2A03` now
  carries an 8-slot `device_gain` table indexed by NSFe device id
  (`apu::mixe_device::*` — APU squares, APU TND, VRC6, VRC7, FDS,
  MMC5, N163, 5B) and applies the gain inside `output_sample`. A
  new `Apu2A03::apply_mixe_overrides(&[NsfeMixerEntry])` converts
  signed millibels to a linear `10^(mB / 2000)` scalar per the
  `dB = 20 * log10(linear)` convention from the §mixe spec. `NsfPlayer::new`
  auto-applies the overrides from `header.metadata.mixer`. Expansion
  output gets a parallel `Expansion::output_with_device_gain` path
  that scales each enabled chip's contribution by its mixe slot.
- **`plst` / `psfx` playlist iteration API** (round 5):
  `NsfPlayer::playlist_len()` / `sfx_playlist_len()`,
  `playlist_song(index)` / `sfx_playlist_song(index)`,
  `playlist_iter()`, and `start_playlist_entry(index)`. Plays the
  NSFe playlist (which is 0-based on disk) using the 1-based song
  convention `start_song` already uses.
- 9 new integration tests (`tests/parse_header.rs`) and 1 new unit
  test (apu) covering: Dendy region detection from `regn`, fallback
  to PAL speed when Dendy speed is missing, Dendy CPU clock + INIT
  X=2, NSF2 appended `regn` promotion to Dendy, mixe gain table
  construction, mixe gain propagated into APU `output_sample`, plst
  helpers, `start_playlist_entry` seeding, and an end-to-end Dendy
  render that confirms the player produces non-trivial PCM through
  the new clock.
- `RATE` chunk on the NSF 2 appended-metadata blob path now
  overrides the v1 header speed fields (matching the NSFe
  header-path behaviour landed in round 4).

- **NSFe extended-chunk metadata parser** (round 4): new `nsfe` module
  decodes every documented optional chunk — `auth` (title/artist/
  copyright/ripper), `tlbl` (per-track labels), `taut` (per-track
  authors), `text` (free-form notes), `time` and `fade` (signed-ms
  track timings), `plst` and `psfx` (music + sound-effect playlists),
  `mixe` (per-device millibel mixer overrides), `regn` (region mask +
  preferred), `RATE` (NTSC/PAL/Dendy playback periods), `VRC7` (device
  selector + optional 128/152-byte patch table). Surfaces as a new
  `NsfHeader::metadata: NsfeMetadata` field; legacy `song_name` /
  `artist` / `copyright` / `track_labels` / `ntsc_speed_us` /
  `pal_speed_us` / `region` fields now also lift from the matching
  extended chunks. The same parser runs over the NSF 2 appended-
  metadata blob (`$7D-$7F` length) so both shapes share one code path.
- **BANK / NSF2 chunks** on the NSFe header path: `bankswitch_init`
  and `Nsf2Features` are now populated from `BANK` / `NSF2` chunks in
  NSFe files (previously zeroed).
- **APU IRQ flags wired into the bus IRQ line** (round 4): `Apu2A03`
  now models the `$4017` frame-counter IRQ inhibit + 4-step end-of-
  frame flag set + `$4015` bit-6 acknowledge per nesdev wiki.
  `NesBus::irq_line()` OR's the NSF2 timer, the frame-counter IRQ,
  and the DMC IRQ — non-NSF2 NSFs that enable APU IRQs can now be
  observed by the CPU through the same vector path. Unit + integration
  tests cover the inhibit-clear, inhibit-set, 5-step (never sets), and
  acknowledge paths.
- 12 new unit tests in the new `nsfe` module + 7 new integration
  tests covering: full NSFe extended-chunk round-trip, NSF 2 appended-
  metadata blob parsing, unknown-uppercase-chunk rejection on the
  header path, APU frame IRQ bus wiring with inhibit on/off, 5-step
  mode never raising frame IRQ, and `$4015` acknowledge of frame +
  DMC IRQs.

- **NSF 2.x support** (round 3): header parser now accepts version
  byte `0x02`, decodes the `$7C` feature-flag byte (IRQ support,
  non-returning INIT, suppressed PLAY, mandatory metadata) into a
  new `Nsf2Features` type, and splits the program block from
  appended NSFe metadata using the 24-bit length field at `$7D-$7F`.
  Returns a typed `Nsf2DataLengthOverflow` error when the declared
  length runs past EOF.
- **NSF 2.x IRQ timer device** (`$401B/$401C/$401D`) on the bus —
  reload register, activate / acknowledge semantics, IRQ flag
  clear-on-read of `$401D`, fires every `N+1` cycles per spec.
- **`$FFFA-$FFFF` vector overlay**: the bus now installs RAM at the
  6502 vector slots when the player arms NSF2 paradigms. NMI/Reset
  vectors are owned by the player; the IRQ vector is preloaded from
  the underlying ROM and writable by the NSF program.
- **6502 IRQ + NMI servicing**: `Cpu6502::step` now checks the bus's
  IRQ line (honouring the I flag) and pending NMI request before
  fetching the next opcode; pushes PC + P (B=0, U=1), sets I, and
  vectors through `$FFFE` (IRQ) or `$FFFA` (NMI). Both take 7 cycles.
- **NSF2 non-returning INIT** paradigm in `NsfPlayer`: INIT is now
  called twice — first with `Y=$80` (returning), then with `Y=$81`
  (may run indefinitely). PLAY is delivered via a 14-byte NMI
  wrapper installed at `$0200` that preserves A/X/Y, JSRs to the
  play routine, and RTI's back into the still-running INIT.
- **NSF2 suppressed-PLAY** bit honoured: the play scheduler skips
  every PLAY/NMI dispatch when bit 6 of `$7C` is set.
- 14 new unit + integration tests covering NSF2 header parsing, IRQ
  timer semantics, vector-overlay routing, CPU IRQ + NMI dispatch,
  two-phase INIT, and end-to-end IRQ-driven NSF2 playback.

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
