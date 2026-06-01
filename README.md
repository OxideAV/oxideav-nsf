# oxideav-nsf

Pure-Rust NSF (Nintendo Sound Format) player for the
[oxideav](https://github.com/OxideAV) framework. Clean-room from the
public [nesdev.org wiki](https://www.nesdev.org/wiki/NSF) (mirrored
under `docs/audio/nsf/`) plus Kevin Horton's original NSF v1.61 spec
— no external emulator source was consulted, paraphrased, or
cross-checked.

Plays NSF v1, NSFe, and NSF v2 — including the NSF2 IRQ timer
device, vector overlay, non-returning INIT, and suppressed PLAY
paradigms. Round 4 adds: full NSFe extended-chunk metadata
(`auth` / `tlbl` / `taut` / `text` / `time` / `fade` / `plst` /
`psfx` / `mixe` / `regn` / `RATE` / `VRC7`) decoded for both NSFe
and NSF2 appended-metadata blobs; APU frame-counter + DMC IRQs
wired into the bus IRQ line. Round 5 adds: dedicated Dendy region
on a 1.773448 MHz CPU clock with `regn`-driven promotion + INIT
`X=2` + `RATE` Dendy-period preference; NSFe `mixe` per-device
gain overrides applied to the APU mixer (linear gain from signed
millibels); `plst` / `psfx` playlist iteration API on `NsfPlayer`.
Round 6 adds: region-aware noise channel — the PAL divider table
joins the NTSC one so PAL/2A07 rips no longer play their noise
channel at NTSC pitch. Round 7 adds: the FDS frequency-modulation
unit — the wave output now advances at the modulated pitch (mod
table → signed mod counter → pitch formula → 20-bit `wave_pitch`)
instead of the raw register frequency, so FDS vibrato is audible.
Round 8 adds: the FDS volume + mod envelope ramp generators — the
`$4080`/`$4084`/`$408A`/`$4083` envelope units ramp their gains on the
documented `c = 8·(e+1)·(m+1)` timer (with master-speed disable, the
`$4083` halt + 4x-speed bits, and the wave-position-0 PWM latch), so
FDS attack/decay/tremolo and mod-gain sweeps are no longer
register-level only. Round 9 adds: the FDS `$4023` master sound-enable —
clearing bit 1 halts the waveform (frozen wave + mod accumulators,
constant `$4040` output, envelopes not ticked) while `$4080`/`$4089`
writes still affect the held level, per the nesdev FDS-audio §"Master
I/O enable" + §"Frequency high" notes. Round 10 adds: the FDS read
register window at `$4090..=$4097` — current volume gain, wave
accumulator (bits 12-19), current mod gain, mod accumulator (bits 5-11),
`counter × gain` intermediate, next mod-counter increment in 4-bit
twos-complement display form, current wavetable sample, and signed
7-bit mod counter, per the nesdev FDS-audio §"Volume gain ($4090)"
through §"Mod counter value ($4097)" with documented open-bus top-bit
patterns. Round 11 adds: the Namco 163 per-channel timer
accumulators — one channel update every 15 CPU cycles per
`docs/audio/nsf/namco-163-audio-wiki.html` §"Channel Update", driving
the full 18-bit-freq / 24-bit-phase walk with modulo-`wave_len<<16`
wrap, sample-and-hold DAC output, top-down active-channel selection
from the `$7F` `CCC` field, and the `$F800` no-wrap-at-`$7F`
address-port behaviour. Round 12 adds: the Sunsoft 5B noise and
envelope generators — a 17-bit LFSR with taps at bits 16 and 13
clocked off the 5-bit `$06` period (one new random bit every 32 CPU
clocks per `docs/audio/nsf/sunsoft-5b-audio-wiki.html` §Noise), and
the full 16-bit-period / 32-step envelope with all ten §Shape rows
(four one-shot decay/attack patterns, falling + rising continued
sawtooths, falling + rising continued triangles, and four
attack-then-hold variants with optional end-of-attack flips). Tone
channels now flip on the documented `counter >= period` boundary so
period-shortening immediately re-triggers; period 0 behaves as 1 for
tone, noise and envelope per the §Sound period-zero footnote. The
`$07` mixer now honours both tone-disable and noise-disable bits per
channel — emitting tone, noise, tone-AND-noise, or constant-DC as
documented in §Sound — and bit 4 of `$08`..=`$0A` routes the envelope
DAC in place of the 4-bit volume per §Output's 0.75 dB-per-step
envelope-vs-volume mapping. Round 13 adds: the VRC7 patch table —
the dumped §"Internal patch set" 15-instrument ROM from
`docs/audio/nsf/vrc7-audio-wiki.html` lands as `VRC7_INSTRUMENT_ROM`,
the §"Custom Patch" 8-byte bitfield decodes to a `Vrc7Patch` struct
(per-operator tremolo / vibrato / sustain / KSR / fmult, modulator
output level, KSL per operator, both operator waveforms, feedback,
attack / decay / sustain-level / release per operator), and each
channel's `$3X` high nibble + `$2X` sustain bit are now decoded so
`Vrc7::active_patch(ch)` returns the patch the channel is asking for.
Round 14 lands the real OPLL operator pipeline against the newly
staged operator-internals tables in `docs/audio/nsf/opll-ym2413/`:
a per-channel modulator + carrier pair driven by a 19-bit phase
accumulator + 10-bit-period sine table, the log-sin / exp ROMs from
andete's `ym2413-logsin-exp-tables-andete-2015-04-09.txt`, the §3
MUL multiplier table, the §5 FB feedback π-multiple table, half-
rectified sine waveform support per the DC/DM bit, modulator self-
feedback with the documented 2-sample averaging shift, and a 7-bit
envelope generator implementing the Idle → Attack → Decay →
Sustain → Release state machine with EG-TYP percussive-vs-sustained
behaviour. `Vrc7::tick` now runs the operator pipeline at the
OPLL's 49.7163 kHz sample clock (CPU cycles accumulated in Q8 and
emitted every ~36 CPU cycles) and `Vrc7::output` reads the latched
6-channel sum normalised to the host mixer's float range; the
sinusoidal stand-in is gone. The §6 row-256 peak-amplitude
ground-truth `[255, 180, 127, 90, 63, 45, 31, 22, 15, 11, 7, 5, 3,
2, 1, 1]` is matched within ±1 LSB across all 16 volumes via the
log-sin → exp pipeline. The KSL attenuation table (§4) and the
per-rate envelope-increment numeric arrays (§7) remain documented
DOCS-GAP followups — both are deliberately not lifted from
emulator source per the staging's §"Provenance & non-emulator
sourcing" appendix, and need the OPLx-decapsulated independent-RE
article transcribed before they can land verbatim.

Round 15 lands four VRC7 register-level semantics that are fully
spec'd in `docs/audio/nsf/vrc7-audio-wiki.html` (no numeric tables
needed). 1) §"Test Register $0F" decodes the low 4 bits into a
`TestRegister` struct (bit 0 envelopes-forced-zero / full volume,
bit 1 LFO-phase-hold, bit 2 waveform-phase-hold, bit 3 LFO-speed
override) and the per-operator sample path consults it via the new
`OpllChannel::sample_with_test`: bit 0 bypasses the envelope's
exp-offset (envelopes still tick), bit 2 pins both phase
accumulators at 0 (silences output without halting envelopes), bits
1+3 are recorded for the future LFO landing. 2) §Channels' `$2X.S`
sustain bit now overrides both operators' release rate with `$5`
when set and reverts on clear, via the new
`OpllChannel::set_channel_sustain_override`. 3) §"Custom Patch"'s
modulator-only `$00.S` release-disable behaviour wires through a
new `Envelope::release_disabled` flag — the modulator's envelope
ignores key-off entirely while the carrier ($01.S) honours it
unconditionally per the spec's "the carrier does not behave this
way" carve-out. 4) §"Audio Reset ($E000)" bit 6 clears all VRC7
registers, silences `latched_output`, blocks writes to `$9010` /
`$9030` while held, and re-enables writes when cleared.

Round 16 lands the OPLL KSR (Key Scale of RATE) pipeline per
`docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`
§III-1-2 + Table III-2 — a fully-spec'd table that needs no
emulator source. Each operator's KSR bit (`$00`/`$01` D4) is
loaded from the patch on every `OpllChannel::load_patch`, and the
new `Envelope::update_rks(block, fnum_msb)` derives the cached
`Rks` offset from the channel's pitch: `KSR=0` → `Rks = block >> 1`
(D4=0 row reads `0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3`); `KSR=1` →
`Rks = (block << 1) | fnum_msb` (D4=1 row reads `0..15`). The
4-bit per-stage R from the patch is widened to a 6-bit
`RATE = 4·R + Rks` via the new `Envelope::effective_rate(r)`
helper, with the explicit "Note that when R=0, RATE=0" carve-out
honoured (R=0 still halts the envelope regardless of pitch). A
pure pitch-only `$1X` / `$2X` register write that doesn't change
the patch or volume now re-derives both operators' Rks via the
new `OpllChannel::refresh_rks`, so a glide mid-note honours the
new pitch's rate amplification on the very next envelope step.
KSR's *contribution* to the per-stage rate is now bit-correct
against §III-1-2; the absolute per-RATE step magnitude is still
the coarse `2^(rate-1)` Q16-units-per-sample approximation that
remains the documented §7 DOCS-GAP followup.

## Round 2 scope

* **Header parser** ([`parse_nsf`]):
  * NSF v1.x — full 128-byte header (magic `NESM\x1a`, version, song
    count + start, load / init / play addresses, song name / artist /
    copyright Latin-1 strings, NTSC + PAL playback period, region
    flags, expansion-chip mask, bankswitch_init).
  * NSF v2 — version byte `0x02`. Decodes the `$7C` feature-flag
    byte into [`Nsf2Features`] (IRQ support / non-returning INIT /
    suppressed PLAY / mandatory metadata) and splits the program
    block from appended NSFe-style metadata using the 24-bit length
    at `$7D-$7F`. `Nsf2DataLengthOverflow` is returned when the
    declared length runs past EOF.
  * NSFe — chunk-based variant: parses `INFO` + `DATA` + `BANK` +
    `NSF2` at the header layer and feeds every other chunk into the
    NSFe extended-metadata decoder (`auth` / `tlbl` / `taut` / `text`
    / `time` / `fade` / `plst` / `psfx` / `mixe` / `regn` / `RATE`
    / `VRC7`). Unknown lower-case-initial chunks are silently
    skipped; unknown upper-case-initial chunks are rejected as
    mandatory per spec. `RATE` overrides the default playback period;
    `regn`'s `preferred` field overrides the INFO region byte.
* **6502 CPU emulator** ([`Cpu6502`]) — **all 256 opcodes implemented**:
  * 151 documented mnemonics × every legal addressing mode.
  * Unofficial / "illegal" opcodes per
    [nesdev.org/wiki/CPU_unofficial_opcodes](https://www.nesdev.org/wiki/CPU_unofficial_opcodes):
    LAX, SAX, DCP, ISB/ISC, SLO, RLA, SRE, RRA, ANC, ALR, ARR,
    SBX/AXS, the duplicate SBC (`$EB`), full multi-byte NOP variants,
    KIL/JAM (latches the `halted` bit so the player loop short-circuits
    the period), plus the unstable SHA, SHX, SHY, TAS, LAS, ANE/XAA,
    LXA. Unstable opcodes pick the deterministic "magic = 0xFF"
    interpretation documented on the wiki.
  * NES variant: decimal mode inert in `ADC` / `SBC`.
  * Cycle-counting (page-cross + branch penalties + RMW unofficial-op
    timings). Sub-instruction cycle accuracy is not modelled.
  * **IRQ + NMI dispatch** (round 3): `step` checks the bus's IRQ
    line (gated on the I flag) and any pending NMI request before
    fetching the next opcode; pushes PC + P (B=0, U=1), sets I, and
    vectors through `$FFFE` (IRQ) or `$FFFA` (NMI) in 7 cycles each.
    Round 4 hooks the APU's own IRQ sources (DMC end-of-sample +
    frame-counter end-of-frame) into the same line so non-NSF2 NSFs
    that enable APU IRQs can observe them.
* **2A03 APU emulator** ([`Apu2A03`]):
  * Pulse 1 + Pulse 2 (sweep, envelope, length counter, duty).
  * Triangle (linear counter, length counter, 32-step sequencer).
  * Noise (LFSR with both tap modes). Round 6 makes the period
    region-aware — NTSC and PAL divider tables per
    `docs/audio/nsf/apu-noise-wiki.html`, selected off the same
    region flag the DMC uses and re-derived when `set_cpu_hz`
    flips the region.
  * **DMC fully wired** — sample-fetch DMA via the bus, NTSC + PAL
    rate tables, looping flag, IRQ flag surfaced through `$4015`
    (cleared on read) AND through `NesBus::irq_line()` (round 4),
    1-bit delta DAC. CPU-stall timing is omitted (round-2 scope:
    music sample values, not cycle-perfect OAM-stall behaviour).
  * 4-step / 5-step frame counter. Round 4 honours `$4017` bit 6
    (frame-interrupt inhibit) and latches the frame-counter IRQ at
    the end of step 3 in 4-step mode per
    `docs/audio/nsf/apu-frame-counter-wiki.html`; 5-step mode never
    raises the flag. Acknowledged by `$4015` read.
  * Non-linear closed-form mixer per nesdev.org/wiki/APU_Mixer plus
    linearly-summed expansion-chip outputs.
  * **NSFe `mixe` per-device gain overrides** (round 5) — `Apu2A03`
    carries an 8-slot `device_gain` table indexed by NSFe device id
    (`apu::mixe_device::{APU_SQUARES, APU_TND, VRC6, VRC7, FDS,
    MMC5, N163, S5B}`). `apply_mixe_overrides` decodes signed
    millibels via `10^(mB/2000)` linear gain (per the
    `dB = 20·log10` §mixe convention) and `output_sample` multiplies
    each channel's contribution by the matching slot.
    `Expansion::output_with_device_gain` runs the same scaling on
    the expansion-chip path. `NsfPlayer::new` auto-applies the
    overrides from `header.metadata.mixer`.
* **Bankswitching** ([`bus`]):
  * `bankswitch_init` triggers 4 KiB-bank pool construction; eight
    bank-select registers `$5FF8..=$5FFF` route windows in
    `$8000..=$FFFF`. FDS extends with `$5FF6..=$5FF7` → `$6000`/`$7000`
    and turns `$8000..=$FFFF` into RAM.
* **NSF2 IRQ timer device** (round 3) at `$401B/$401C/$401D` —
  reload register, activate / deactivate, cycle-counting underflow
  every `N+1` cycles, IRQ flag latched on underflow and cleared on
  read of `$401D`. Drives the CPU IRQ line via `NesBus::irq_line`.
* **NSF2 vector overlay** at `$FFFA..=$FFFF` — RAM that shadows the
  6502 vector slots when the player arms it. NMI / Reset slots are
  reserved to the player; the IRQ slot is preloaded from the
  underlying ROM and writable by the NSF program (so it can install
  its own IRQ handler during INIT).
* **Expansion chips** ([`expansion`]) — aggregate routed by the bus,
  outputs summed into the APU mixer:
  * **VRC6** — 2 pulses + sawtooth (`$9000..=$B002`).
  * **MMC5** — 2 pulses + 8-bit raw PCM (`$5000..=$5015`).
  * **Sunsoft 5B** — 3 squares with AY-style log-volume envelopes
    (`$C000` / `$E000` indirect register file).
  * **Namco 163** — wavetable RAM at `$4800` indexed via `$F800`
    pointer; up to 8 channels. Round 11 wires the per-channel timer
    accumulators per `docs/audio/nsf/namco-163-audio-wiki.html`
    §"Channel Update" + §"Frequency": every 15 CPU cycles the chip
    updates one channel (round-robin across the active set), adding
    the 18-bit frequency to the 24-bit phase modulo `wave_len << 16`
    and producing one `(sample - 8) * volume` DAC output that is held
    until the next channel-update tick. The control byte at `$7F`'s
    `CCC` field selects channels `9-N..=8` top-down, and the `$F800`
    address pointer stops at `$7F` rather than wrapping.
  * **VRC7** — 6 FM channels driven from `$9010` / `$9030` register
    indirection. Round 14 wires the OPLL operator pipeline from the
    `docs/audio/nsf/opll-ym2413/` staging: per-channel modulator +
    carrier with 19-bit phase generator, the andete log-sin / exp
    tables (12 / 10 bits), the §3 MUL multiplier table, the §5 FB
    feedback π-multiple table, DC/DM half-rectified sine waveforms,
    modulator self-feedback with two-sample averaging, and the
    Idle → Attack → Decay → Sustain → Release envelope state
    machine honouring EG-TYP percussive-vs-sustained semantics.
    `Vrc7::tick` runs the OPLL at its 49.7163 kHz sample clock; the
    §6 row-256 peak-amplitude ground truth is matched within ±1
    LSB across all 16 volumes.
  * **FDS** — wavetable + frequency modulator (`$4040..=$4089`).
    Round 7 wires the modulation unit per
    `docs/audio/nsf/fds-audio-wiki.html`: the mod accumulator adds the
    12-bit mod frequency every 16 CPU cycles, steps the 32-entry mod
    table on each bit-11 carry, updates the signed 7-bit mod counter
    (`{0,+1,+2,+4,reset,-4,-2,-1}` increments with 7-bit wrap), and
    folds counter × mod gain (`$4084`) × pitch through the documented
    pitch formula into a 20-bit `wave_pitch` that drives the wave
    output unit. `$4085` directly sets the counter; `$4087` bit 7
    resets the mod accumulator; `$4088` writes the table only while
    the unit is disabled. Round 8 adds the volume + mod envelope ramp
    generators: each runs a `c = 8·(e+1)·(m+1)` CPU-cycle timer
    (`$4080`/`$4084` speed × `$408A` master speed) and steps its gain
    ±1 toward the 0..=32 range on the active edge; `$408A = 0` disables
    both, `$4083` bit 6 halts + resets their timers, `$4083` bit 7 runs
    them 4x faster (and halts the mod-table accumulator), and a
    volume-gain *change* only commits while the
    wave position is 0 (direct gain-0 writes mute immediately). The
    slow PWM volume-latch on wave-table edges other than position 0 is
    modelled; cycle-exact sub-tick timer phase is not. Round 9 adds the
    `$4023` master sound-enable / waveform-halt: bit 1 (S) gates the
    channel (BIOS writes `$00` then `$83`), and while it is clear the
    wave + mod accumulators stop, the wave position holds at 0 (constant
    `$4040` output) and the envelopes are not ticked — yet `$4080` /
    `$4089` writes still affect the held level (per
    `docs/audio/nsf/fds-audio-wiki.html` §"Master I/O enable" +
    §"Frequency high"). Defaults to enabled for rips that rely on the
    BIOS having already set `$4023`.
* **Player glue** ([`NsfPlayer`]):
  * Loads the program (or builds the bank pool when bankswitching is
    active), runs the `init` routine for a chosen song, then steps
    CPU + APU at the NES clock and invokes `play` once per
    `play_period` (NTSC ~60 Hz / PAL ~50 Hz / Dendy ~50 Hz).
  * **Dendy region** (round 5) — `regn` preferred = 2 promotes
    `NsfRegion::Dendy`; the player runs on the 1.773448 MHz Dendy
    CPU clock and seeds INIT with `X = 2` per
    `docs/audio/nsf/nsfe-nesdev-wiki.html` §regn. Period preference
    is Dendy RATE → PAL RATE → 19 997 µs default.
  * **`plst` / `psfx` playlist API** (round 5) — `playlist_len`,
    `playlist_song(idx)`, `playlist_iter()`, `start_playlist_entry(idx)`
    plus the symmetric `sfx_*` getters. The on-disk 0-based song
    indexes are lifted to the 1-based convention `start_song` uses.
  * Resamples to 44 100 Hz mono S16 by hold-and-pick.
  * **NSF2 paradigms** (round 3):
    * **IRQ support** — the player honours `$7C` bit 4 by enabling
      the bus's timer device; the NSF program writes its handler to
      `$FFFE/$FFFF` during INIT, then `CLI`'s to take IRQs.
    * **Non-returning INIT** (`$7C` bit 5) — INIT is invoked twice:
      first with `Y=$80` (must return) then with `Y=$81` (may run
      forever). PLAY is delivered through a 14-byte NMI wrapper at
      `$0200` (`PHA TXA PHA TYA PHA JSR play PLA TAY PLA TAX PLA
      RTI`) that the player installs and points `$FFFA` at.
    * **Suppressed PLAY** (`$7C` bit 6) — the player never invokes
      the play routine (typically combined with non-returning INIT).
* **`Decoder` + `Demuxer` glue** behind the default-on `registry`
  feature — wires the codec into the `oxideav-core` registry as the
  `nsf` codec / container with magic-byte probe.

### Standalone use

`default-features = false` drops the `oxideav-core` dep. The
[`parse_nsf`] / [`NsfPlayer`] free-standing API is unaffected.

## Verification

* `tests/parse_header.rs` builds a synthetic NSF whose `init` programs
  the pulse-1 channel at constant volume + 50 % duty, then renders 4096
  samples (~93 ms) and asserts the output is non-trivially audible
  (non-zero samples, peak > 1 000 LSB, mean |amplitude| > 200 LSB).
* `tests/real_rip.rs` fetches `chibi-tech_-_miko_miko_nurse.nsf` (1
  track, NTSC, no expansion) from `samples.oxideav.org`, parses the
  header, and renders 30 wall-clock seconds (~1.32 M samples). Asserts
  the player never halts and produces non-trivial audio across the
  buffer. Network gated by `OXIDEAV_NETWORK_TESTS=1`; cached in
  `target/test-fixtures/oxideav-nsf-real-rip/` after first download.
* APU unit tests cover DMC address-seed, fetch-pending bookkeeping,
  status-bit accuracy, and IRQ-flag latching.
* CPU unit tests cover the unofficial LAX, SAX, DCP, ISB, SLO, RLA,
  ANC, SBX, JAM, duplicate-SBC, and multi-byte-NOP opcode behaviours.
* Expansion-chip unit tests cover register decoding for VRC6, MMC5,
  Sunsoft 5B, FDS, and N163 — plus the routing logic in
  [`expansion::Expansion`].
* Round-11 N163 unit tests cover the per-channel timer accumulator:
  the `$7F` `CCC` field decoding `channels_active`, top-down
  active-channel selection (with N=1→{ch8}, N=2→{ch7,ch8},
  N=8→{ch1..ch8}), the `$F800` no-wrap-at-`$7F` address-port footnote,
  the per-15-cycle phase advance (with sub-window cycle accumulation),
  the phase wrap modulo `wave_len << 16` (4-sample wave at freq=0x20000
  cycling 0x30000 → 0x10000), sample decoding at `(phase>>16)+wave_addr`
  with the `-8` bias and linear-volume scaling, round-robin ordering
  across 2 enabled channels (ch7 → ch8 → ch7 again), the
  sample-and-hold behaviour across partial-cycle ticks, the
  silent-when-disabled guarantee, and the cycle accumulator carrying
  leftover cycles across multiple short calls.
* Round-10 FDS unit tests cover the `$4090..=$4097` read-register window:
  `$4090` volume-gain readback with the documented `01` open-bus top
  bits, `$4091` wave-accumulator bits 12-19, `$4092` mod-gain readback,
  `$4093` mod-accumulator bits 5-11 with top bit 0, `$4094` `counter ×
  gain` intermediate (positive + negative cases), `$4095` next mod
  increment in 4-bit twos-complement display form (including the entry-4
  reset → `0xC` mapping), `$4096` wavetable sample at the current
  position, `$4097` signed 7-bit mod counter across the full -64..=63
  range, the open-bus fall-through for unmapped FDS reads, and the
  `Expansion::read` routing only triggering once the FDS chip flag is
  enabled.
* Round-9 FDS unit tests cover the `$4023.D1` sound-enable default, the
  sound-disable wave-accumulator halt + wave-position freeze-to-0 +
  re-enable, the mod-accumulator halt while sound is disabled, the
  envelopes being frozen while halted (and resuming on re-enable), and
  `$4080` / `$4089` volume writes still affecting the held output during
  the halt.
* Round-8 FDS unit tests cover the `c = 8·(e+1)·(m+1)` envelope-period
  formula (including the `$4083` 4x-fast division and the master-speed-0
  disable), the volume envelope decreasing to 0 and increasing to its 32
  clamp, the mod envelope ramping the mod gain in both directions,
  master-speed-0 freezing the envelopes, `$4083` bit-6 halt/resume,
  `$4083` bit-7 4x speed, `$4083` bit-7 halting the mod-table
  accumulator, the `$4080` mode-bit direct-write and
  immediate-mute paths, the wave-position-0 PWM latch staging a
  volume-gain change until the wave position returns to 0, and the mode
  bit blocking the ramp entirely.
* Round-7 FDS unit tests cover the modulation pitch formula against
  the spec's C-style reference (centered, positive-round-up, and
  negative-counter branches), the `$4084` mod-gain / `$4085`
  mod-counter decode, mod-table write gating + pointer advance,
  bit-11-carry counter stepping, signed-7-bit wrap, the entry-4
  counter reset, accumulator reset on `$4087` disable, and an
  end-to-end check that an active modulator changes the accumulated
  wave position relative to an unmodulated channel.
* Round-5 integration tests cover: Dendy region detection from
  `regn`, fallback to PAL speed when the Dendy RATE field is absent,
  Dendy CPU clock + INIT `X = 2` seeding, NSF 2 appended-`regn`
  promotion to Dendy, `mixe` gain-table construction (`10^(mB/2000)`),
  `mixe` gain propagating into `output_sample` at ~0.5x for -6 dB,
  `plst` helpers (`playlist_song` / `playlist_iter` /
  `start_playlist_entry`), and an end-to-end Dendy render that
  produces non-trivial PCM.

## Round 8+ followups

* Cycle-accurate per-cycle CPU + APU timing (frame-counter jitter,
  read-cycle-stall behaviour, DMC CPU-stall halt-cycle accounting). For
  FDS specifically: the envelope tick timers are stepped in CPU-cycle
  batches, not per individual cycle, so sub-tick write-resets land on a
  batch boundary rather than the exact write cycle — adequate for music,
  not cycle-exact.
* VRC7 OPLL operator chain (logsin / exp / phase / feedback /
  envelope) — LANDED round 14 against the new staged tables in
  `docs/audio/nsf/opll-ym2413/`. Round 15 wired the `$0F` test
  register (envelope/phase/LFO holds), the `$2X.S` channel-level
  release-rate-to-5 override, the modulator `$00.S` release-disable
  carve-out, and the `$E000` bit-6 audio reset / silence / write
  block. Round 16 wired KSR (Key Scale of RATE) per the §III-1-2
  Table III-2 — fully spec'd, no emulator source needed — so the
  `RATE = 4·R + Rks` widening is now bit-correct against the
  application-manual table even though the per-RATE step magnitude
  remains the coarse approximation. Remaining numeric DOCS-GAPs
  (all flagged by the staging as deliberately not lifted from
  emulator source): the §4 KSL attenuation table (needs the
  OPLx-decapsulated independent-RE base table transcribed before
  KSL contributes attenuation; currently 0 dB) and the §7 per-RATE
  envelope-increment numeric arrays (the underlying per-RATE step
  magnitude is still a `2^(rate-1)` Q16-units-per-sample monotonic
  ladder honouring R=0 halt + RATE=63 saturating-fast semantics
  but NOT bit-exact against the OPLx-decapsulated per-rate table;
  KSR now widens the rate correctly into this table even though
  the table itself is the approximation). LFO numeric step arrays
  (§7) are the third DOCS-GAP; the test register `$0F` bits 1 + 3
  are wired and recorded but have no audible effect until the LFO
  lands. Rhythm-mode operator allocation (`$0E` bit 5, drum
  channels at `$36`/`$37`/`$38`) is also out of scope — VRC7 has
  no rhythm DAC so the drum patches in the ROM are inaudible
  there, but a future YM2413 consumer (an MSX-format crate, say)
  would need rhythm-mode decoded.
* N163: round 11 added the per-channel timer accumulators. Remaining
  gap is matching the documented `f = (n * p) / (15 * 65536 * l * c)`
  output frequency in a calibration test against a known fixture (the
  current synthetic tests verify per-tick phase advances and
  round-robin ordering, but not the multi-channel-divided emitted
  frequency end-to-end).
* FDS: round 8 added the envelope ramp generators and round 9 the
  `$4023.D1` waveform-halt (constant `$4040` output + frozen
  accumulators + envelopes not ticked while halted, per §"Master I/O
  enable" + the §"Frequency high" TODO). The remaining gap is
  cycle-exact envelope timer phase on register-write resets (the timers
  are stepped in CPU-cycle batches, so a sub-tick write-reset lands on a
  batch boundary rather than the exact write cycle).
* MMC5 PCM: round 4 leaves the channel decoded at register-level only;
  needs a software-mode timer + read-mode wiring.
* RIFF-NSF container variant.
* `oxideav-source` magic-detection registration so the framework
  auto-dispatches `*.nsf` and `*.nsfe` URIs.

## License

MIT — see [LICENSE](LICENSE).

[`parse_nsf`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/fn.parse_nsf.html
[`Cpu6502`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.Cpu6502.html
[`Apu2A03`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.Apu2A03.html
[`NsfPlayer`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.NsfPlayer.html
[`Nsf2Features`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.Nsf2Features.html
[`bus`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/bus/index.html
[`expansion`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/expansion/index.html
