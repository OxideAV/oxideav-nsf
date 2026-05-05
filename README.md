# oxideav-nsf

Pure-Rust NSF (Nintendo Sound Format) player for the
[oxideav](https://github.com/OxideAV) framework. Clean-room from the
public [nesdev.org wiki](https://www.nesdev.org/wiki/NSF) — no NES
emulator source (FCEUX / Nestopia / Mesen / nestopia-rs / nes-rust /
NSFPlay / etc.) was consulted, paraphrased, or cross-checked.

## Round 2 scope

* **Header parser** ([`parse_nsf`]):
  * NSF v1.x — full 128-byte header (magic `NESM\x1a`, version, song
    count + start, load / init / play addresses, song name / artist /
    copyright Latin-1 strings, NTSC + PAL playback period, region
    flags, expansion-chip mask, bankswitch_init).
  * NSFe — chunk-based variant: parses `INFO` + `DATA` + `auth` +
    `tlbl`. Unknown lower-case-initial chunks are silently skipped;
    unknown upper-case-initial chunks are rejected as mandatory.
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
* **2A03 APU emulator** ([`Apu2A03`]):
  * Pulse 1 + Pulse 2 (sweep, envelope, length counter, duty).
  * Triangle (linear counter, length counter, 32-step sequencer).
  * Noise (LFSR with both tap modes).
  * **DMC fully wired** — sample-fetch DMA via the bus, NTSC + PAL
    rate tables, looping flag, IRQ flag surfaced through `$4015`
    (cleared on read), 1-bit delta DAC. CPU-stall timing is omitted
    (round-2 scope: music sample values, not cycle-perfect
    OAM-stall behaviour).
  * 4-step / 5-step frame counter.
  * Non-linear closed-form mixer per nesdev.org/wiki/APU_Mixer plus
    linearly-summed expansion-chip outputs.
* **Bankswitching** ([`bus`]):
  * `bankswitch_init` triggers 4 KiB-bank pool construction; eight
    bank-select registers `$5FF8..=$5FFF` route windows in
    `$8000..=$FFFF`. FDS extends with `$5FF6..=$5FF7` → `$6000`/`$7000`
    and turns `$8000..=$FFFF` into RAM.
* **Expansion chips** ([`expansion`]) — aggregate routed by the bus,
  outputs summed into the APU mixer:
  * **VRC6** — 2 pulses + sawtooth (`$9000..=$B002`).
  * **MMC5** — 2 pulses + 8-bit raw PCM (`$5000..=$5015`).
  * **Sunsoft 5B** — 3 squares with AY-style log-volume envelopes
    (`$C000` / `$E000` indirect register file).
  * **Namco 163** — wavetable RAM at `$4800` indexed via `$F800`
    pointer; up to 8 channels.
  * **VRC7** — 6 FM channels driven from `$9010` / `$9030` register
    indirection. Round 2 ships a coarse 2-operator approximation in
    place of the full OPLL operator chain — sufficient to mix at the
    right balance, not bit-exact.
  * **FDS** — wavetable + frequency modulator (`$4040..=$4089`).
* **Player glue** ([`NsfPlayer`]):
  * Loads the program (or builds the bank pool when bankswitching is
    active), runs the `init` routine for a chosen song, then steps
    CPU + APU at the NES clock and invokes `play` once per
    `play_period` (NTSC ~60 Hz / PAL ~50 Hz).
  * Resamples to 44 100 Hz mono S16 by hold-and-pick.
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

## Round 3+ followups

* Cycle-accurate per-cycle CPU + APU timing (frame-counter jitter,
  read-cycle-stall behaviour, DMC CPU-stall halt-cycle accounting).
* VRC7: replace the 2-operator approximation with a real OPLL operator
  chain (logarithmic sin / exp tables, 4-bit feedback, full envelope
  generator).
* N163: per-channel timer accumulators (currently the 8 channels share
  a coarse phase model).
* FDS: amplitude envelope on the main volume + LFO-style modulator
  rebiasing.
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
[`bus`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/bus/index.html
[`expansion`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/expansion/index.html
