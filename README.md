# oxideav-nsf

Pure-Rust NSF (Nintendo Sound Format) player for the
[oxideav](https://github.com/OxideAV) framework. Clean-room from the
public [nesdev.org wiki](https://www.nesdev.org/wiki/NSF) — no NES
emulator source (FCEUX / Nestopia / Mesen / nestopia-rs / nes-rust /
etc.) was consulted, paraphrased, or cross-checked.

## Round 1 scope

* **Header parser** ([`parse_nsf`]):
  * NSF v1.x — full 128-byte header (magic `NESM\x1a`, version, song
    count + start, load / init / play addresses, song name / artist /
    copyright Latin-1 strings, NTSC + PAL playback period, region
    flags, expansion-chip mask, bankswitch_init).
  * NSFe — chunk-based variant: parses `INFO` + `DATA` + `auth` +
    `tlbl`. Unknown lower-case-initial chunks are silently skipped;
    unknown upper-case-initial chunks are rejected as mandatory.
* **6502 CPU emulator** ([`Cpu6502`]):
  * All 256 official opcodes (every documented mnemonic × every legal
    addressing mode — 151 distinct ops). NES variant: decimal mode
    inert in `ADC` / `SBC`.
  * Cycle-counting (page-cross + branch penalties) — accurate enough
    to drive the play-rate scheduler. Cycle-accurate sub-instruction
    timing is not modelled.
  * Unofficial / illegal opcodes execute as NOP of correct base length
    so a malformed (or genuine illegal-using) NSF does not desynchronise
    the play loop. Real semantics are deferred to round 2.
* **2A03 APU emulator** ([`Apu2A03`]):
  * Pulse 1 + Pulse 2 (with sweep, envelope, length counter, duty).
  * Triangle (with linear counter, length counter, 32-step sequencer).
  * Noise (LFSR with both tap modes).
  * DMC (DAC level only — sample-fetch DMA deferred to round 2).
  * 4-step / 5-step frame counter.
  * Non-linear closed-form mixer per nesdev.org/wiki/APU_Mixer.
* **Player glue** ([`NsfPlayer`]):
  * Loads the program, runs the `init` routine for a chosen song,
    then steps CPU + APU at the NES clock and invokes `play` once per
    `play_period` (NTSC ~60 Hz / PAL ~50 Hz).
  * Resamples to 44 100 Hz mono S16 by hold-and-pick.
* **`Decoder` + `Demuxer` glue** behind the default-on `registry`
  feature — wires the codec into the `oxideav-core` registry as the
  `nsf` codec / container with magic-byte probe.

### Standalone use

`default-features = false` drops the `oxideav-core` dep. The
[`parse_nsf`] / [`NsfPlayer`] free-standing API is unaffected.

## Verification

`tests/parse_header.rs` builds a synthetic NSF whose `init` programs
the pulse-1 channel at constant volume + 50 % duty, then renders 4096
samples (~93 ms) and asserts the output is non-trivially audible
(non-zero samples, peak > 1 000 LSB, mean |amplitude| > 200 LSB).

A real NSF rip is **not** bundled in round 1 — the synthetic test is
the binding guard. Round 2 will add a binary `nsfplay` /
`ffmpeg + libgme` cross-check fixture pulled from a permissive NES
homebrew repo (Garbage Day, GradualGames packs, etc.) once the unofficial
opcodes + DMC sample fetch are in place.

## Round 2+ followups

* Cycle-accurate per-cycle CPU + APU timing (frame-counter jitter,
  read-cycle-stall behaviour).
* Real semantics for the ~80 unofficial 6502 opcodes (LAX, SAX, DCP,
  ISB, SLO, RLA, SRE, RRA, ANC, ALR, ARR, AXS, KIL).
* DMC sample-fetch DMA + IRQ routing.
* Expansion-chip emulation: VRC6, VRC7, FDS, MMC5, N163, Sunsoft 5B.
* NSF 2.x extra-feature byte (IRQ + non-returning init handlers).
* RIFF-NSF container variant.
* `oxideav-source` magic-detection registration so the framework
  auto-dispatches `*.nsf` and `*.nsfe` URIs.

## License

MIT — see [LICENSE](LICENSE).

[`parse_nsf`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/fn.parse_nsf.html
[`Cpu6502`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.Cpu6502.html
[`Apu2A03`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.Apu2A03.html
[`NsfPlayer`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.NsfPlayer.html
