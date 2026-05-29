# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **VRC7 OPLL operator pipeline** (round 14): the round-2 sinusoidal
  stand-in is replaced with a real OPLL (YM2413) operator chain wired
  off the newly-staged operator-internals tables in
  `docs/audio/nsf/opll-ym2413/`. A new `opll` module implements the
  log-sin and exp ROMs algorithmically per andete's
  `ym2413-logsin-exp-tables-andete-2015-04-09.txt` (`logsinTable[i] =
  round(-log2(sin((i+0.5)*π/2/256))*256)` 12-bit, `expTable[i] =
  round(exp2(i/256)*1024) - 1024` 10-bit) plus the `lookup_sin` /
  `lookup_exp` algorithm with 1024-step phase periods and
  sign-magnitude representation. The §3 MUL multiplier table
  (`½..15`, with the documented duplicate 10/12/15 entries) and
  the §5 FB feedback π-multiple table are transcribed verbatim from
  `opll-ym2413-tables.md`. An `Operator` carries a 19-bit phase
  accumulator (so `(fnum << block) * MUL` divides down cleanly), a
  7-bit envelope (0..=127, +0.375 dB per step matching andete §
  "envelope levels"), and the DC/DM half-rectified-sine waveform
  bit; an `OpllChannel` pairs modulator + carrier with the
  modulator self-feedback path (averaging the two prior outputs,
  shifted right by `9 - fb`). The envelope generator runs the
  documented Idle → Attack → Decay → Sustain → Release state
  machine with key-on edge resetting phase, EG-TYP percussive vs.
  sustained semantics, and rate-0 halt. `Vrc7::tick` accumulates
  CPU cycles in Q8 fixed-point and emits one operator sample every
  `35.9956` CPU cycles (1.789773 MHz / 49.7163 kHz). `Vrc7::output`
  reads the latched sum of the 6 channels and normalises to the
  host mixer's ±1.0 range. The register-level state (`Vrc7Chan`)
  is unchanged so the round-13 patch-decode tests still pass;
  key-on / key-off / patch-select / volume-change writes now drive
  the OPLL channels through edge-detected transitions inside
  `refresh_from_regs`. +21 unit tests: log-sin table first / last
  entry against andete's formula, 12-bit fit, exp table first / last
  entry and 10-bit fit, the §6 row-256 peak-amplitude
  ground-truth `[255, 180, 127, 90, 63, 45, 31, 22, 15, 11, 7, 5,
  3, 2, 1, 1]` match (within ±1 LSB across all 16 volumes), MUL
  table exact match, FB shift table exact match, log-sin
  quadrant-mirror symmetry, pure-sine peak at phase 256, sine
  zero-crossings at phase 0 / 512, phase-index wrap modulo 1024,
  envelope key-on → attack transition, key-off → release
  transition, rate-0 halt, percussive-mode through-sustain
  release, channel patch loading from the Trumpet ROM bytes,
  key-on phase reset, key-off transition to release on both
  operators, and end-to-end channel sample stream producing
  non-trivial audio after a Flute-patch key-on. The KSL
  attenuation table (§4, requires the OPL-family base table that
  the staging deliberately does not lift from emulator source) and
  the per-rate envelope-increment numeric arrays (§7, same
  provenance reason) remain documented followups; rhythm-mode
  drum operator allocation is also out of scope for this round.

- **VRC7 patch table + per-channel patch selection** (round 13): the
  15-instrument hardwired §"Internal patch set" ROM dumped in
  `docs/audio/nsf/vrc7-audio-wiki.html` is now exposed as the
  `VRC7_INSTRUMENT_ROM: [[u8; 8]; 16]` table (slot 0 is the
  user-programmable placeholder; slots 1..=15 are the named presets
  "Buzzy Bell" through "Sweep"). A new `Vrc7Patch` struct decodes the
  §"Custom Patch" 8-byte bitfield per operator: modulator + carrier
  tremolo (T) / vibrato (V) / sustain (S) / key-rate-scaling (K) /
  multiplier (M) from `$00`/`$01`; modulator KSL + 6-bit output level
  from `$02`; carrier KSL, carrier waveform (Q), modulator waveform
  (W), and 3-bit feedback from `$03`; attack + decay per operator
  from `$04`/`$05`; sustain level + release per operator from
  `$06`/`$07`. `Vrc7Patch::from_bytes` works on both the
  `VRC7_INSTRUMENT_ROM` rows and the user patch at
  `regs[0x00..=0x07]`. The `$3X` channel register's high nibble (I)
  now maps to `Vrc7Chan::patch_index` and `Vrc7::active_patch(ch)`
  returns the patch the channel currently asks for; slot 0 reads
  through to the live custom-patch registers so a runtime
  re-program is reflected immediately. The `$2X` sustain bit (S) is
  also decoded into `Vrc7Chan::sustain` so the §Channels "S overrides
  patch release with $5" rule can be honoured by a future OPLL
  operator implementation. +9 unit tests covering the 16-entry ROM
  size, the §"Custom Patch" bitfield decode against Buzzy Bell + Vibes
  byte-for-byte, slot-0 custom-patch reads through `Vrc7::patch`, the
  `$2X` decode of sustain + key-on combinations, the `$3X` decode of
  instrument index + inverted volume, the default-everything fresh-chip
  state, and the patch-index modulo-16 defensive wrap. The audible
  signal path is unchanged — VRC7 output is still the round-2
  sinusoidal stand-in — but the patch-selection plumbing now matches
  the wiki and unblocks a real OPLL operator implementation (#861)
  without another API break.

- **Sunsoft 5B noise + envelope generators** (round 12): the 5B
  expansion chip now drives the documented 17-bit LFSR noise
  generator (5-bit period at `$06`, taps at bits 16 and 13, one new
  random bit every 32 CPU clocks per
  `docs/audio/nsf/sunsoft-5b-audio-wiki.html` §Noise) and the full
  16-bit-period 4-bit-shape envelope generator (period at
  `$0B`/`$0C`, shape at `$0D`, 32-step ramp per §Envelope and §Shape).
  All ten §Shape rows are implemented — the four `$00..$07`
  one-shot decay/attack patterns silence to step 0 and hold there;
  `$08` is a continued falling sawtooth that wraps to 31; `$0A` is
  a continued falling-then-rising triangle; `$0C` is a continued
  rising sawtooth that wraps to 0; `$0E` is the rising-then-falling
  triangle; `$09`/`$0B` hold at the floor (with `$0B` flipping to
  31 at the end of the attack); `$0D`/`$0F` hold at the top (with
  `$0F` flipping to 0 at the end). Writing `$0D` resets the envelope
  phase to the start of the shape. Tone channels now also flip on
  the documented `counter >= period` boundary (counter resets to 0,
  immediate flip if the new period is smaller than the current
  counter per the §Sound period-shortening note); period 0 behaves
  as period 1 for tone, noise, and envelope per the §Sound
  period-zero footnote. The §Sound mixer at `$07` now interprets
  both tone-disable AND noise-disable bits per channel — when both
  are clear the channel emits the logical AND of tone and noise;
  when both are set the channel emits a constant DC at the
  configured volume. Bit 4 of `$08`..=`$0A` routes the envelope DAC
  in place of the 4-bit volume; the 32-step envelope DAC table is
  generated at 0.75 dB per step (envelope step 0 and 1 both
  silent; envelope step `2k+2` matches volume step `k` per §Output).
  +13 tests (tone period 0, tone flip cadence, noise LFSR cycle
  length 2^17-1, six envelope shape walks, shape-reset phase, both
  mixer-mode combinations).

- **Namco 163 per-channel timer accumulators** (round 11): the N163
  expansion chip now ticks one channel every 15 CPU cycles per
  `docs/audio/nsf/namco-163-audio-wiki.html` §"Channel Update" +
  §"Frequency" instead of approximating channel output from the static
  RAM contents. Each enabled channel walks the documented
  `phase' = (phase + freq) % (wave_len << 16)` update with its full
  18-bit frequency (`$78`/`$7A`/`$7C` low+mid+high-2-bits) and 24-bit
  phase (`$79`/`$7B`/`$7D`); the updated phase is written back into
  sound RAM at the same three bytes so a program can read it back via
  the `$4800` data port. The DAC output `(sample(((phase>>16)+wave_addr)
  &0xFF) - 8) * volume` is held until the next channel update —
  matching the sample-and-hold behaviour of the real chip's
  serial-output DAC. The wave length decoder honours the
  `256 - (LLLLLL00)`-formula from §"Sound RAM $7C". The control byte
  `$7F`'s `CCC` field selects channels `9-N..=8` (top-down enabling per
  §"Sound RAM $7F - Volume"); round-robin starts at slot 0 = channel
  `9-N` and wraps after the last enabled channel. The address port at
  `$F800` now stops at `$7F` instead of wrapping per the §"Address
  Port" footnote-cited correction. Previously the channel output was
  computed from raw `$40+8*ch` RAM contents at the audio-resample rate
  with no actual phase advance, so N163 tones followed the program's
  write cadence rather than the chip's clock.
- 10 new unit tests covering: the `$7F` C-field decoding into
  `channels_active`, the top-down active-channel set selection, the
  `$F800` no-wrap-at-`$7F` address-port behaviour, the per-15-cycle
  phase advance (with sub-window cycle accumulation), phase wrapping
  modulo `wave_len << 16`, sample decoding at `(phase>>16)+wave_addr`
  with the `-8` bias and linear-volume scaling, the round-robin
  ordering across two enabled channels (ch7 → ch8 → ch7 again), the
  sample-and-hold behaviour on partial-cycle ticks, the silent-when-
  disabled guarantee, and cycle-accumulator carrying across multiple
  short calls.

- **FDS read registers `$4090..=$4097`** (round 10): the FDS channel now
  exposes the read-only status register window documented in
  `docs/audio/nsf/fds-audio-wiki.html` §"Volume gain ($4090)" through
  §"Mod counter value ($4097)". The new `Fds::read` returns: `$4090`
  current volume gain (`0x40 | volume & 0x3F`, top 2 bits "01" open
  bus); `$4091` bits 12-19 of the wave accumulator; `$4092` current mod
  gain (`0x40 | mod_gain & 0x3F`, same open-bus pattern); `$4093` bits
  5-11 of the 12-bit mod accumulator (top bit 0); `$4094` bits 4-11 of
  the `mod_counter * mod_gain` intermediate; `$4095` the next mod-counter
  increment translated into 4-bit twos-complement display form
  (`0,1,2,3,4,5,6,7 → 0,1,2,4,C,C,E,F`); `$4096` the wavetable sample at
  the current position (same open-bus pattern); `$4097` the signed 7-bit
  mod counter (top bit 0). `Expansion::read` now also routes to FDS when
  the chip is enabled, so a music driver running on the bus can poll the
  unit's live state instead of always seeing the `0xFF` open-bus default.
  Previously every read inside this window fell through to open bus.
- 9 new FDS unit tests covering each of `$4090`/`$4091`/`$4092`/`$4093`/
  `$4094`/`$4095`/`$4096`/`$4097` (positive and negative mod-counter
  cases, full coverage of the mod-table display-form table including
  reset/entry-4), the open-bus fall-through for unmapped FDS reads
  (`$4080`/`$408A`/`$4040`/`$4098`/`$4099`), and the `Expansion::read`
  routing only triggering once the FDS chip flag is enabled.
- **FDS `$4023.D1` master sound-enable / waveform-halt** (round 9): the
  FDS channel now honours the `$4023` Master I/O enable register per
  `docs/audio/nsf/fds-audio-wiki.html` §"Master I/O enable" + the
  §"Frequency high" waveform-halt note. Bit 1 (S) must be set for the
  sound channel to function (the BIOS writes `$00` then `$83`); while it
  is clear the waveform is halted — the wave + modulation accumulators
  stop advancing and the wave position is frozen at 0, so the channel
  holds the constant `$4040` value, and the volume + mod envelopes are
  not ticked. Writes to the volume (`$4080`) and master-volume (`$4089`)
  registers still affect the held output. Defaults to enabled so a rip
  that relies on the BIOS having already set `$4023 = $83` (or that never
  re-writes `$4023`) still plays. Previously `$4023` writes were silently
  dropped, so the channel kept running even when the program had disabled
  sound.
- 5 new FDS unit tests covering the enabled-by-default state, the
  sound-disable wave-accumulator halt + position-freeze-to-0 + re-enable,
  the mod-accumulator halt while disabled, the envelopes being frozen
  while halted (and resuming on re-enable), and `$4080`/`$4089` volume
  writes still affecting the held output during halt.
- **FDS volume + mod envelope ramp generators** (round 8): the FDS
  `$4080` / `$4084` / `$408A` / `$4083` envelope units now ramp their
  gains over time instead of only taking direct register writes, per
  `docs/audio/nsf/fds-audio-wiki.html` §"Unit tick → Envelopes" +
  §"Frequency calculation and timing → Envelopes". Each envelope counts
  a `c = 8 · (e + 1) · (m + 1)` CPU-cycle timer (`e` = the 6-bit speed
  in `$4080`/`$4084`, `m` = the master speed `$408A`); on underflow it
  increases the gain by 1 (capped at 32 on the active edge) or decreases
  it by 1 (floored at 0). `$408A = 0` disables both envelopes; `$4083`
  bit 6 halts both and resets their timers; `$4083` bit 7 runs them 4x
  faster (and also halts the mod-table accumulator per §"Frequency
  high"). The volume envelope is a PWM unit, so a volume-gain *change*
  is staged and only commits while the wave position is 0 (a direct
  `$4080` write of gain 0 still mutes immediately). `$4080`/`$4084`
  mode-bit-set (M=1) writes set the gain directly and suppress the ramp;
  the speed field is latched regardless of the mode bit. Writing the
  control registers resets the affected unit's timer. `$408A` (master
  envelope speed, BIOS-initialised to `$E8`) is now decoded. Previously
  the envelope ramps were register-level only, so FDS attack/decay/
  tremolo and mod-gain sweeps were silent — only instantaneous gain
  writes were heard.
- 11 new unit tests covering the `c = 8·(e+1)·(m+1)` period formula
  (incl. the 4x-fast division and master-speed-0 disable), the volume
  envelope decreasing to 0 and increasing to its 32 clamp, the mod
  envelope ramping the mod gain in both directions, master-speed-0
  freezing the envelopes, `$4083` bit-6 halt/resume, `$4083` bit-7 4x
  speed, `$4083` bit-7 halting the mod-table accumulator, the mode-bit
  direct-write and immediate-mute paths, the wave-position-0 PWM latch
  on volume-gain changes, and the mode bit blocking the ramp.

- **FDS frequency-modulation unit** (round 7): the wave output unit now
  advances at the *modulated* pitch instead of the raw 12-bit register
  value, per `docs/audio/nsf/fds-audio-wiki.html` §"Modulation unit" +
  §"Frequency calculation and timing". Both the mod unit and the wave
  unit tick every 16 CPU cycles; the mod accumulator adds the 12-bit
  mod frequency each tick and, on a carry out of bit 11, steps the
  32-entry mod table (each entry applied twice via the unused LSB of a
  64-step pointer) and updates the signed 7-bit mod counter by the
  table's `{0,+1,+2,+4,reset,-4,-2,-1}` increment. The mod counter,
  the 6-bit mod gain (`$4084`) and the 12-bit pitch feed the
  documented pitch formula to produce a 20-bit `wave_pitch` that the
  wave accumulator (6 address bits over 18 fractional bits) consumes.
  New register handling: `$4084` now sets the mod gain (previously
  mis-wired to the mod position), `$4085` sets the signed mod counter,
  `$4087` bit 7 resets the mod accumulator, and `$4088` only writes the
  mod table while the unit is disabled (advancing the pointer by one
  entry per write). Previously the modulator computed a position that
  was never applied to the wave, so FDS vibrato/modulation was silent.
- 8 new unit tests covering the pitch formula against the spec's
  C-style reference (centered, positive round-up, and negative-counter
  branches), the `$4084`/`$4085` register decode, mod-table write
  gating + pointer advance, bit-11-carry counter stepping, signed
  7-bit counter wrap, the entry-4 reset, accumulator reset on disable,
  and an end-to-end check that an active modulator changes the
  accumulated wave position relative to an unmodulated channel.

- **Region-aware noise period table** (round 6): the noise channel now
  carries both the NTSC and the PAL divider tables from
  `docs/audio/nsf/apu-noise-wiki.html` §"Period"
  (`[4, 8, 14, 30, 60, 88, 118, 148, 188, 236, 354, 472, 708, 944,
  1890, 3778]` for PAL) and selects between them off the same `pal`
  flag the DMC already follows. `Apu2A03::set_cpu_hz` re-derives the
  active noise period when the region flips, and `$400E` stores the
  period index so a later region change still picks the correct
  divider. Previously a PAL rip's noise channel always used the NTSC
  table and played at the wrong pitch.

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
