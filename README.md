# oxideav-nsf

Pure-Rust NSF (Nintendo Sound Format) player for the
[oxideav](https://github.com/OxideAV) framework. Clean-room from the
public NSF / NSFe documentation staged under `docs/audio/nsf/`.

Plays NSF v1, NSFe, and NSF v2 — including the NSF2 IRQ timer device,
vector overlay, non-returning INIT, and suppressed-PLAY paradigms.

## What it does

* **Header parser** (`parse_nsf`):
  * **NSF v1.x** — full 128-byte header (magic `NESM\x1a`, version,
    song count + start, load / init / play addresses, Latin-1
    name / artist / copyright, NTSC + PAL period, region flags,
    expansion-chip mask, bankswitch init).
  * **NSF v2** — `$7C` feature-flag byte decoded into `Nsf2Features`
    (IRQ support / non-returning INIT / suppressed PLAY / mandatory
    metadata), with the program block split from appended NSFe-style
    metadata via the 24-bit length at `$7D-$7F`.
  * **NSFe** — chunk-based variant: `INFO` / `DATA` / `BANK` / `NSF2`
    at the header layer plus the full extended-metadata decoder
    (`auth` / `tlbl` / `taut` / `text` / `time` / `fade` / `plst` /
    `psfx` / `mixe` / `regn` / `RATE` / `VRC7`). `NSFDRV` sound-driver
    tag identification (`OFGS` / `FTDRV` / `NSDL`) is surfaced.

* **6502 CPU emulator** (`Cpu6502`) — all 256 opcodes (151 documented
  mnemonics across every legal addressing mode, plus the unofficial /
  unstable opcodes), NES decimal-inert ADC/SBC, cycle counting
  (page-cross + branch + RMW penalties), and IRQ + NMI dispatch.

* **2A03 APU emulator** (`Apu2A03`) — pulse 1/2 (sweep, envelope,
  length, duty), triangle, region-aware noise (NTSC + PAL divider
  tables), fully-wired DMC (DMA sample fetch, rate tables, looping,
  IRQ), 4-step / 5-step frame counter with `$4017` interrupt-inhibit,
  the non-linear closed-form mixer, and NSFe `mixe` per-device gain
  (seeded from the documented default mix levels).

* **Bankswitching** (`bus`) — `$5FF8..=$5FFF` bank-select registers
  routing 4 KiB windows; FDS extends with `$5FF6..=$5FF7` and turns
  `$8000..=$FFFF` into RAM.

* **NSF2 paradigms** — IRQ timer device at `$401B-$401D`, vector
  overlay at `$FFFA..=$FFFF`, non-returning INIT (twice-invoked with
  `Y=$80` / `Y=$81` + NMI-wrapper PLAY), and suppressed PLAY.

* **Expansion chips** (`expansion`) — summed into the APU mixer:
  * **VRC6** — 2 pulses (16-step duty down-counter) + 14-step sawtooth.
  * **MMC5** — 2 pulses with the chip's fixed 240 Hz envelope + length
    unit, plus 8-bit raw PCM with `$5010` PCM Mode/IRQ semantics and the
    analog Pin 2 DAC transfer curve (the
    `Voltage = (DAC/255)·0.4·AVcc + 0.1·AVcc` characteristic, AC-coupled
    about its 0.3·AVcc midpoint).
  * **Sunsoft 5B** — 3 squares with AY-style log-volume envelopes
    (17-bit LFSR noise, 32-step envelope shapes, select-port write
    lock-out).
  * **Namco 163** — wavetable RAM, up to 8 channels, per-channel timer
    accumulators (one update / 15 CPU cycles), the documented
    emitted-frequency / channel-update-rate calibration, and the
    §"Mixing" multi-channel mix (sum the active channels' held samples
    and divide by their count) so multi-voice tracks stay balanced
    instead of presenting only the most-recently-updated channel.
  * **VRC7** — 6 FM channels driven by the OPLL (YM2413) operator
    pipeline: 10.9-bit fixed-point phase generator (10 integer sine-index
    bits + 9 fractional bits, one sine period = `1024 << 9` accumulator
    units per the andete §9 silicon measurement, so a key-on'd channel
    renders at the doc-predicted fundamental and an end-to-end frame-render
    test recovers it from the rendered PCM), log-sin / exp ROMs, MUL / FB
    tables, half-rectified waveforms, modulator self-feedback, the
    Idle→Attack→Decay→Sustain→Release envelope with EG-TYP behaviour —
    the **Decay / percussive-Sustain / Release** advance now driven by
    the silicon-measured §7 global-counter rate-increment model (a
    chip-wide counter shared by all 18 operators), with **Attack**
    timing from the YM2413 Application Manual Table III-7 (the §7a
    attack-level recurrence is an open DOCS-GAP) — KSR + KSL attenuation
    (Tables III-2 / III-5), the `$0F` test register, `$E000` audio
    reset, the
    audible AM/VIB LFO — both modulation paths now apply their
    **silicon-measured depth tables**: the VIB (vibrato) sweep uses the
    §8b 8×8 phase-modulation table (`VIB_PM_TABLE`, indexed
    `pmTable[fnum>>6][vib_phase]`) through the exact phase-step
    `(((2*fnum + lfo_pm) * mlTab[ML]) << block) >> 2`, and the AM
    (tremolo) uses the §8a 210-entry 14-level (0..13) truncated-triangle
    waveform (`AM_LFO_LEVELS`) applied as `16 * am` in the operator exp
    index for the measured ≈ 4.8 dB peak depth — each on its operator's
    `$00`/`$01` AM / VIB bit — rhythm-mode register decoding, and
    bass-drum (BD) rhythm synthesis (`RhythmBassDrum`) — the §V-4
    two-slot FM pair on channel 7, keyed from the `$0E` BD bit, with
    the §III-4 percussion ×2 DAC doubling. The shared rhythm **noise
    generator** (`OpllNoiseLfsr`) is now pinned: the 23-bit
    maximal-length LFSR (`x^23 + x^9 + 1`, Galois step `^= 0x40_0181`)
    recovered by Berlekamp-Massey from the silicon-RE SD-tail capture,
    with the all-zero trap, non-zero seed, and the 72-cycle
    HH-sample / 3-step / SD-sample / 15-step rhythm-frame tap protocol —
    the noise source HH + SD synthesis consume.
  * **FDS** — wavetable + frequency-modulation unit, the volume + mod
    envelope ramp generators (`c = 8·(e+1)·(m+1)` timer), the `$4023`
    master sound-enable / waveform-halt, and the `$4090..=$4097` read
    register window.

* **Player glue** (`NsfPlayer`) — loads the program / bank pool, runs
  INIT for a chosen song, steps CPU + APU at the NES clock, invokes
  PLAY once per period (NTSC ~60 Hz / PAL ~50 Hz / Dendy ~50 Hz on the
  1.773448 MHz Dendy clock), exposes `plst` / `psfx` playlist
  iteration, and resamples to 44 100 Hz mono S16.

* **`Decoder` + `Demuxer` glue** behind the default-on `registry`
  feature wires the codec into the `oxideav-core` registry with a
  magic-byte probe. `default-features = false` drops the `oxideav-core`
  dep; the free-standing `parse_nsf` / `NsfPlayer` API is unaffected.

## Known gaps

* Cycle-accurate per-cycle CPU + APU timing (frame-counter jitter,
  DMC CPU-stall accounting); envelope tick timers are stepped in
  CPU-cycle batches — adequate for music, not cycle-exact.
* OPLL envelope *attack*: the per-RATE step magnitude on the *live*
  **Attack** path derives from the YM2413 Application Manual Table III-7
  ms timings, because the §7a *attack-level* per-step recurrence remains
  a documented DOCS-GAP (andete measured the attack timing but not the
  exact level sequence). The **Decay / percussive-Sustain / Release**
  path is now driven by the silicon-measured §7 global-counter
  rate-increment model (`EG_SELECT_TABLE` / `EG_HIGHRATE_TABLE` /
  `eg_decay_advance`, the `eg_shift`/`eg_select` algorithm with the
  rate-52..59 high-rate corrections): a chip-wide global counter — shared
  by all 18 operators, incremented once per output sample in `Lfo::tick`
  — is threaded through `Envelope::step_eg`, so a decaying note follows
  the measured stair-stepped per-sample `{0,+1,+2}` EG-level increments
  (e.g. the `1024/1024/2048`-sample segment cadence for effective decay
  rate 14) instead of a linearised ramp. End-to-end frame-render tests
  confirm the rendered PCM amplitude ramps down per the model and that a
  faster decay rate quiets sooner.
* The VRC7 rhythm *synthesis* path for HH/SD/TOM/TOP-CYM (BD is now
  synthesised as the §V-4 two-slot FM pair, and the shared noise LFSR
  the §V-4 noise-mixed phase generator consumes is now pinned as
  `OpllNoiseLfsr`; the four noise/phase percussion voices still need
  their exact per-instrument phase formulas, which are not in the
  staged docs — DOCS-GAP #1786).
* RIFF-NSF container variant.

## Verification

* `tests/parse_header.rs` renders a synthetic NSF and asserts
  non-trivial audio output.
* `tests/real_rip.rs` fetches a real rip from `samples.oxideav.org`,
  renders 30 wall-clock seconds, and asserts the player never halts
  (network-gated by `OXIDEAV_NETWORK_TESTS=1`).
* Unit tests cover the CPU, APU, and each expansion chip's register
  decoding and signal generation.

## License

MIT — see [LICENSE](LICENSE).

[`parse_nsf`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/fn.parse_nsf.html
[`NsfPlayer`]: https://docs.rs/oxideav-nsf/latest/oxideav_nsf/struct.NsfPlayer.html
