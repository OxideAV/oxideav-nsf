# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Every `start_song` now performs the documented pre-INIT machine
  scrub** (`docs/audio/nsf/nsf-nesdev-wiki.html` §"Initializing a
  tune", mirrored by `docs/audio/nsf/nsfspec-kevtris-v1.61.txt`
  §"'Proper' way to init a tune"): clear all RAM at `$0000-$07FF` and
  `$6000-$7FFF`, write `$00` to `$4000-$4013`, `$00` then `$0F` to
  `$4015`, `$40` to `$4017` (4-step, IRQ inhibit), and re-seed the
  bank registers from header `$070-$077` — including the FDS-extended
  `$5FF6/$5FF7` pair, which the wiki says must mirror `$076/$077` for
  the `$6000-$7FFF` windows "before INIT is called" (previously never
  seeded at all). New `NesBus::reset_for_tune` implements the
  sequence; the player calls it before every INIT, so switching
  tracks no longer leaks the previous song's RAM contents, APU
  register state, bank mapping, or FDS RAM self-modifications into
  the next song. A non-bankswitched tune loaded below `$8000` has its
  program bytes re-placed after the RAM clear, per the documented
  ordering. 3 new bus tests cover the RAM + register scrub (with the
  `$4015=$0F` / `$4017=$40` observables), the low-load program
  reload, and the bank-selection restore.

- **Triangle `$4015`-disable now holds the sequencer's output instead
  of snapping to zero.** "Use $4015 to turn off the channel, which
  will clear its length counter" is one of the silencing methods the
  triangle doc says "halt it in whatever its current output position
  is", and the register reference states "Silencing the triangle
  channel merely halts it. It will continue to output its last value
  rather than 0" — but the `enabled == false` path still hard-zeroed
  the output, re-introducing exactly the pop the counter-expiry hold
  fix removed for the other silencing paths. The enable flag no longer
  gates the DAC level (it already halts the sequencer through the
  cleared length counter); the residual DC of a held step is removed
  by the documented post-DAC high-pass chain downstream. The
  ultrasonic "7.5" average is now also correctly limited to a
  *cycling* sequencer (counters non-zero) — a halted channel holds its
  step whatever the period. The power-up sequencer position is not
  pinned by the staged docs; it is seeded at step 15 (the sequence's
  zero-output value) so a never-played triangle idles silent. New
  `triangle_4015_disable_holds_position_not_zero` test; the ultrasonic
  test now pins the cycling-only midpoint.

- **`$4015` write/read IRQ-flag semantics now match the documented
  register contract** (`docs/audio/nsf/apu-nesdev-wiki.html` §"Status
  ($4015)"). A `$4015` *write* now clears the DMC interrupt flag
  ("Writing to this register clears the DMC interrupt flag") — it
  previously left the flag latched, so a tune that acknowledged a DMC
  IRQ the documented way kept re-entering its IRQ handler. A `$4015`
  *read* no longer clears the DMC interrupt flag ("Reading this
  register clears the frame interrupt flag (but not the DMC interrupt
  flag)") — it previously acknowledged both, so polling `$4015` bit 7
  for end-of-sample destroyed the very flag the read was reporting
  before the program could dispatch on it. The frame interrupt flag is
  untouched by writes, cleared by reads, exactly as before. Tests: the
  end-of-sample IRQ test now pins read-preserves / write-clears, plus
  new `dmc_irq_flag_cleared_by_4010_irq_disable` and
  `status_write_does_not_touch_frame_irq_flag`.

- **Frame counter now fires its events at the exact documented CPU
  half-cycles** (`docs/audio/nsf/apu-frame-counter-wiki.html` §"Mode
  0"/"Mode 1"). Three corrections in one restructure to a per-event
  schedule table:
  * Quarter-/half-frame signals land on the **PUT half** of their APU
    cycle — the doc's "additional delay of one CPU cycle for the
    quarter and half frame signals" — i.e. CPU offset `2×APU + 1`
    (7457/14913/22371/29829 NTSC 4-step), not the even GET cycle.
  * The 4-step **frame interrupt flag is set at three consecutive CPU
    cycles** (step-4 GET 29828, its PUT 29829, and the wrap GET
    29830; PAL 33252/33253/33254), so a program that acknowledges via
    `$4015` at the first set point sees the documented immediate
    re-assertion. Previously the flag was a single-shot at 29828.
  * **5-step mode's 4th step clocks nothing and its 5th step clocks
    BOTH units** per the Mode 1 table (quarter at steps 1/2/3/5, half
    at 2/5). The old code issued a spurious quarter clock at step 4
    and only a half clock at step 5, so every envelope in 5-step mode
    decayed on a wrong cadence.
  New `frame_counter_quarter_signal_lands_on_put_cycle` and
  `five_step_clocks_nothing_at_fourth_step_and_both_at_fifth` tests;
  the IRQ-schedule test now pins the triple set-point behaviour.

- **Frame-counter clocks now interleave exactly with the channel
  timers, independent of CPU cycle batching.** `tick_cpu_cycles` splits
  every batch at the next scheduled frame-sequence event, so a
  quarter-/half-frame clock fires exactly between the channel-timer
  cycles that surround its documented CPU offset. Previously a whole
  instruction's cycles ticked the channel timers first and the frame
  events fired afterwards, which was observably wrong wherever the two
  interact — a sweep's period rewrite landed relative to the wrong
  pulse-timer reload, and a linear-counter expiry froze the triangle
  sequencer a few cycles late, so the audible stream depended on how
  the CPU chunked its cycles. This closes the README's
  "envelope tick timers are stepped in CPU-cycle batches" caveat for
  the frame-clocked units. New `frame_clocked_units_are_chunk_invariant`
  test runs a sweeping pulse + short-linear-counter triangle over
  100 000 cycles bulk-vs-single-cycle and requires identical state.

- **`$4017` writes now take effect on the documented 3-or-4-CPU-cycle
  delay** (`docs/audio/nsf/apu-frame-counter-wiki.html` §"Side
  effects": "After 3 or 4 CPU clock cycles*, the timer is reset. If
  the mode flag is set, then both 'quarter frame' and 'half frame'
  signals are also generated" — 3 cycles for a write on the second
  (odd) half of an APU cycle, 4 for a write on the first (even) half,
  so the effects always land on the same CPU/APU phase). The mode +
  inhibit register bits still apply immediately, but the sequence
  reset and the 5-step write's quarter+half clock were previously
  instantaneous; a tune that syncs the frame counter by writing
  `$C0`/`$FF` once per frame now sees the hardware's phase-dependent
  reset latency. The old sequence keeps running until the reset
  lands. New `frame_counter_4017_reset_delay_depends_on_write_phase`
  and `frame_counter_4017_bit7_clear_does_not_clock_units` tests; the
  timing tests now measure from the delayed sequence start.

- **DMC output unit now powers up silent** per
  `docs/audio/nsf/apu-dmc-wiki.html` §"Output unit": the sample buffer
  is empty at power-up, the silence flag is set whenever an output
  cycle starts with an empty buffer, and "The DPCM unit can only
  transition from silent to playing at the end of an output cycle."
  The channel previously started with the silence flag clear and the
  bits-remaining counter at 0, so the very first timer clock applied a
  bogus −2 delta (from the never-loaded shift register) to the output
  level — audibly corrupting a `$4011` direct-load PCM level before
  any sample played. Power-up is now silence-set with a fresh 8-bit
  output cycle; new `dmc_output_unit_powers_up_silent` test pins a
  `$4011` level as rock-steady across 10 000 idle CPU cycles.

- **Pulse channels no longer self-mute on low (bass) notes when no sweep
  is configured.** The sweep adder-overflow mute was applied
  unconditionally, but with the default zero shift count the adder
  computes `target = period + (period >> 0) = 2 × period`, so any pulse
  with a period above `0x3FF` (a perfectly audible low note per the
  pulse frequency formula) was silenced even though its sweep was never
  set up. The adder-overflow mute now only applies when the shift count
  is non-zero, while a genuine sweep overflow (non-zero shift, target >
  `0x7FF`) still mutes as before. New
  `pulse_low_note_plays_without_sweep_configured` and
  `pulse_sweep_overflow_still_mutes` tests.

- **Noise channel now clocks its shift register at the correct rate.**
  The `$400E` period table is expressed in CPU cycles ("The period
  determines how many CPU cycles happen between shift register clocks"),
  but the channel timer was being driven at the APU (CPU/2) rate and
  reloaded with the full table value, making the noise pitch run roughly
  2.5× too low. The noise timer is now driven at the full CPU clock and
  reloads with `period - 1`, so register `$80` (period 4) clocks the
  LFSR at `1789773 / 4 ≈ 447 kHz` — matching the documented
  §"Pitches of 93-step noise" NTSC sample rate. Covered by the new
  `noise_shift_rate_matches_spec_sample_rate` test.
- **Pulse channels no longer slowly detune under odd CPU cycle chunks.**
  The pulse /2 (APU-cycle) prescaler dropped the low bit of every
  `cycles / 2`, so an instruction that consumed an odd number of CPU
  cycles silently lost half an APU cycle. Over a track this accumulates
  into an audible pitch drift. A prescaler carry now retains the dropped
  half-cycle across calls, making the pulse timer phase invariant to how
  the CPU batches its cycles (new
  `pulse_prescaler_carry_is_chunk_invariant` test).

- **Frame counter now follows the documented region/mode event schedule
  instead of a uniform 4×7457 approximation.** The quarter-/half-frame
  events fire at the exact CPU-cycle offsets from the frame-counter spec
  (NTSC/PAL × 4-step/5-step), so the 4-step interrupt period is exactly
  the documented 29830 (NTSC) / 33254 (PAL) CPU cycles and the IRQ flag
  latches at the documented final-step offset (29828 / 33252). NSF tunes
  that poll `$4015` for the frame IRQ to keep time now see correct
  cadence. New `frame_counter_irq_fires_on_documented_schedule` and
  `frame_counter_pal_period_is_documented` tests.

- **Triangle silencing now holds its output position instead of snapping
  to zero.** Previously an ultrasonic or counter-expired triangle returned
  a hard 0, injecting a pop where the hardware simply freezes the
  sequencer at its current step. The channel now holds its last sequencer
  level when the length/linear counters expire, and for the ultrasonic
  (period < 2) case reports the spec's lowpass-averaged "7.5" level
  rather than silence — the mixer carries the triangle in half-steps over
  a doubled divisor so that 7.5 stays exact. New
  `triangle_holds_position_when_counters_expire` and
  `triangle_ultrasonic_reports_midpoint_not_silence` tests.

- **FDS RAM image is now sized whenever the FDS chip is enabled**, not
  only on the bankswitched load path. An FDS-flagged header that did not
  bankswitch (`bankswitch_init` all-zero) left `fds_ram` a zero-length
  vector, so the first write to `$8000..=$FFFF` (and any non-bankswitched
  read once FDS turns that window into RAM) indexed an empty vector and
  panicked. `configure_from_header` now allocates the `$8000`-byte FDS
  RAM image and primes it from the loaded program for the
  non-bankswitched case, and the non-bankswitched read path routes
  `$8000..=$FFFF` through `fds_ram` so a self-modifying FDS program sees
  its own writes. Found by the new `tests/parse_fuzz.rs` harness.

### Added

- **DMC DMA CPU-stall accounting** — closes the README's named
  "DMC CPU-stall accounting" gap. Per
  `docs/audio/nsf/apu-dmc-wiki.html` §"Memory reader", every
  sample-byte fetch stalls the CPU ("The CPU is stalled for 1-4 CPU
  cycles to read a sample byte […] The processor will continue on from
  where it was stalled"). The bus now accrues
  `apu::DMC_DMA_STALL_CYCLES` (4 — the top of the documented range)
  per fetch while keeping the APU + NSF2 timer running through the
  stolen cycles; `Cpu6502::step` folds the stall into its returned
  cycle count and the player's idle path folds it into the elapsed
  time, so the PLAY cadence and samples-per-cycle budget both see the
  DMA-stretched wall clock. A DPCM-heavy tune's PLAY rate no longer
  runs fast relative to its sample playback. DOCS-GAP: the wiki's DMA
  article (with the 1/2/3-cycle per-alignment special cases) is not
  staged, so all fetches account the full 4-cycle halt. New
  `dmc_dma_fetch_stalls_are_accounted` (bus) and
  `step_includes_dmc_dma_stall_cycles` (CPU) tests.

- **Post-DAC analog filter chain on the player output.** The mixer doc
  describes the NES hardware following the channel DACs with two
  first-order high-pass filters (90 Hz, 440 Hz) and a first-order
  low-pass at 14 kHz. The player now runs every rendered sample through
  this chain before scaling to i16: the high-passes centre the
  positive-only `[0, 1]` mixer output about zero (removing the DC bias —
  which also cleanly resolves the held-DC level a silenced triangle now
  produces) and the low-pass rolls off the harshest aliasing. The filter
  resets on `start_song`. New `output_filter_removes_dc_bias`,
  `output_filter_passes_audible_ac`, and
  `output_filter_coefficients_are_in_unit_range` tests.

- **`tests/parse_fuzz.rs` — never-panic / never-hang robustness battery**
  for the whole parse + render surface. A self-contained xorshift LCG
  (no external crates) drives `parse_nsf` and, for inputs that parse, an
  `NsfPlayer` render through truncated prefixes, every single-byte header
  mutation, every expansion-chip mask (`$7B` bits 0..5), random byte
  streams, random programs behind a valid header (so the 6502 + 2A03 APU
  + expansion render loop runs on adversarial code), and structured /
  random NSFe chunk streams (so the `auth`/`time`/`fade`/`plst`/`psfx`/
  `mixe`/`regn`/`RATE`/`VRC7` metadata sub-parsers run on hostile
  payloads). This is the deterministic, CI-runnable half of the
  coverage-guided `fuzz/` libfuzzer harness.

### Changed

- **OPLL envelope *attack* now also §7 global-counter-driven** with the
  measured 12-level sequence
  (`docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §7 +
  `ym2413-envelope-attack-rates-andete-2015-03-27.txt`). The live attack
  path (`Envelope::step_eg`) no longer linearises the Table III-7
  ms-timing: its transition *timing* now reuses the same §7
  `eg_shift`/`eg_select` global-counter duty as decay (new
  `eg_attack_advance`), and each transition lands on the next entry of the
  silicon-measured 12-level `ATTACK_LEVEL_SEQUENCE`
  (`127,95,71,53,39,28,20,13,9,5,1,0`) that andete found *every* attack
  passes through regardless of rate. Effective rate ≤ 3 never completes
  the attack; rate ≥ 60 is instantaneous (jump straight to the loudest
  level). Only the exact level-generating *recurrence* (and its
  initial-level dependence) remains the open §7a gap — the measured level
  *sequence* itself is concrete data. New `step_eg`
  attack-cadence / level-sequence / boundary unit tests cover it.

- **OPLL envelope decay/release now driven by the §7 silicon-measured
  global-counter rate-increment model** (`docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md`
  §7 + `ym2413-envelope-decay-rates-andete-2015-03-20.txt`). A single
  chip-wide global counter — shared by all 18 operators, incremented once
  per per-operator output sample in `Lfo::tick` — now drives the live
  Decay, percussive-Sustain, and Release EG advance through the
  `eg_shift`/`eg_select` duty algorithm (`Envelope::step_eg`), replacing
  the earlier Table III-7 *ms-timing* linear approximation
  (`decay_step_q16_per_sample`) on the audible path. A decaying note now
  follows the measured stair-stepped per-sample `{0,+1,+2}` EG-level
  increments (e.g. the `1024/1024/2048`-sample segment cadence for an
  effective decay rate of 14) instead of a smooth ramp, including the
  rates-52..59 high-rate correction tables. The **Attack** phase retains
  the Table III-7 timing because the §7a attack-EG-**level** recurrence is
  a documented reverse-engineering gap (only the attack timing — not the
  per-step level sequence — is silicon-measured). Both melody channels and
  the channel-7 bass-drum rhythm voice share the one counter, as on the
  die. New `step_eg` cadence/attack/halt/counter unit tests cover it,
  plus two end-to-end `tests/vrc7_frame_render.rs` integration tests that
  program a decaying carrier through the `$9010`/`$9030` register ports
  and confirm the rendered PCM amplitude ramps down per the §7 model and
  that a faster decay rate quiets sooner — proving the model drives the
  audible path, not just the unit envelope.

### Fixed

- **VRC7 user-patch ($00-$07) writes now reload patch-0 channels**: a
  write to the user-programmable instrument registers ($00-$07) previously
  only reached a channel's live operator envelopes via the $3X
  patch-index/volume *swap* path, so a track that programmed the user
  patch *after* selecting patch slot 0 (with no subsequent $3X change) ran
  the operators with the default zeroed AR/DR/SL/RR constants. The chip
  now reloads every channel currently selecting patch 0 on any $00-$07
  write (ROM-patch channels are left untouched). Regression test added.

- **OPLL phase-generator pitch**: the phase accumulator's fractional-bit
  count was 19, but the YM2413 phase counter is **10.9 fixed-point** (10
  integer sine-index bits + **9** fractional bits) per andete's silicon
  measurement (`docs/audio/nsf/opll-ym2413/ym2413-phase-counter-andete-2015-03-16.txt`
  §"conclusion" + `opll-ym2413-tables.md` §9, die-shot = 19 bands). With
  19 fractional bits the accumulator wrapped 1024× too slowly, so every
  VRC7/OPLL note rendered ~1024× below pitch (subsonic). `PHASE_ACC_FRAC_BITS`
  is now 9; one sine period spans `1024 << 9 = 524288` accumulator units,
  exactly reproducing andete's "#repeats = 512 / step-size" and
  high-frequency period-length tables (e.g. ML=0, block=0, fnum=256 →
  step `0x80`, period 4096 samples). The per-sample phase `inc` deltas are
  unchanged (the existing `step_phase` / `step_phase_pm` unit tests only
  checked those), so only the audible wrap-period — and pitch — is fixed.

### Other

- **VRC7 frame-render integration test** (`tests/vrc7_frame_render.rs`):
  drives the chip end-to-end through the `$9010` / `$9030` register ports,
  programs a single melody note, renders a full ~1/60 s frame of operator
  samples, and verifies the PCM is (1) audible and zero-crossing, (2) at
  the §9 doc-predicted fundamental (recovered by zero-crossing count to
  within 5 %), (3) one octave higher when `block` increments, and (4)
  silenced by key-off + fast release. This is the test that surfaced the
  phase-frac-bits pitch bug above.

- OPLL tests: add **per-channel synthesis property tests** that validate
  the §8a/§8b LFO depth tables on the live `OpllChannel::sample_with_test`
  path (not just the bare table readers). A keyed-on full-volume carrier
  is rendered with AM/VIB enabled: `am_depth_on_synthesis_path_*` asserts
  the carrier peak dips by the measured ≈ 4.8 dB between the AM trough
  (level 0) and crest (level 13); `am_enabled_modulates_peak_across_period`
  asserts the peak is LFO-phase-invariant with AM off and modulated with
  AM on; `vib_enabled_sweeps_phase_rate_on_synthesis_path` asserts the
  carrier's per-sample phase increment is larger at the §8b sharp column
  (2) than the flat column (6) and constant with VIB off.

- OPLL envelope: stage the **silicon-measured §7 EG rate-increment
  model** (`opll::EG_SELECT_TABLE`, `EG_HIGHRATE_TABLE`,
  `eg_decay_advance`). The YM2413 EG has 128 levels (0.375 dB each — the
  datasheet's "0.325 dB" is a measured typo) advanced by a shared
  chip-wide global counter: `eg_shift = 13 - rate/4`, `eg_select = rate &
  3` picks one of four duty patterns (4/8, 5/8, 6/8, 7/8), and the EG
  advances only on the samples where the `eg_shift`-windowed counter
  rolls over. Effective rates 52..=59 use 16-entry high-rate tables that
  andete found **correct the emulator model** (e.g. rate 54's measured
  `2,2,1,1,1,1` detail). The §7 worked example — decay rate 14 →
  `eg_shift = 10`, 6/8 duty → repeating segment lengths `1024, 1024,
  2048` samples — is reproduced bit-exact by a test
  (`docs/audio/nsf/opll-ym2413/ym2413-envelope-decay-rates-andete-2015-03-20.txt`,
  `tables/envelope-rate-increment*.csv`, §7 #138). The model is landed as
  validated public building blocks; wiring it into `Envelope::step` (the
  per-call chip-wide counter threading) is a tracked followup.

- OPLL AM: the per-operator amplitude modulation (tremolo) now uses the
  **silicon-measured §8a AM waveform table** instead of the earlier
  1.0 dB linear-triangle approximation. `opll::AM_LFO_LEVELS` is the
  exact 210-entry, 14-level (0..13) truncated triangle andete measured
  on a real YM2413
  (`docs/audio/nsf/opll-ym2413/ym2413-am-lfo-andete-2015-11-28.txt`,
  `tables/am-lfo-triangle.csv`, §8a #138): the OPLL drops the low bit of
  the OPL-family 0..26 ramp, holding level 0 for 15 steps (960 samples),
  levels 1..=12 for 8 steps each, and level 13 for 3 steps before
  descending — full period `210 × 64 = 13440` samples ≈ 3.7 Hz. The
  level is applied to the operator's exp index as `16 * am`
  (`AM_LFO_EXP_WEIGHT`), exactly as the envelope level is, giving a peak
  attenuation of `16 × 13 = 208` exp units ≈ 4.8 dB — the measured depth,
  which **corrects the previously-assumed 1.0 dB**. `Lfo::tremolo_am_level`
  reads the table and `Lfo::tremolo_atten_exp_units` returns `16 * am`;
  `OpllChannel::sample_with_test` now drives both operators through it.
  The old `Lfo::tremolo_atten_env_levels` (1.0 dB approximation) is
  retained `#[deprecated]` as a compatibility shim, off the synthesis
  path. Closes the crate's named §7 OPLL/VRC7 LFO depth-step-array gap.

- OPLL VIB: the per-operator vibrato (frequency modulation) now uses the
  **silicon-measured §8b phase-modulation table** instead of the earlier
  cents-scaled triangle approximation. `opll::VIB_PM_TABLE` is the exact
  8×8 integer `pmTable[fnum>>6][counter>>10]` andete independently
  confirmed on real hardware
  (`docs/audio/nsf/opll-ym2413/ym2413-vib-lfo-andete-2015-12-01.txt`,
  `tables/vib-lfo-pm.csv`): the top three F-Number bits select a row and
  the vibrato phase (one column per 1024 samples, 8192-sample period ≈
  6.07 Hz) selects a column. `Lfo::vibrato_pm` reads it and
  `Operator::step_phase_pm` folds the signed correction into the exact
  phase-step `(((2*fnum + lfo_pm) * mlTab[ML]) << block) >> 2`. The
  per-sample synthesis path (`OpllChannel::sample_with_test`) now drives
  both operators through this formula; with VIB clear it reduces exactly
  to the prior `((fnum * mlTab[ML]) << block) >> 1` step. The §8b worked
  example (`fnum=0x1c0, block=6, ML=1` → step sizes
  `28672,28768,28896,28768,28672,28576,28448,28576`) is reproduced
  bit-exact by a test. The legacy `vibrato_pitch_offset_q` /
  `apply_vibrato` cents helpers remain as public utilities but are no
  longer on the synthesis path.

- OPLL rhythm: YM2413 rhythm-channel pseudo-random **noise generator**
  (`opll::OpllNoiseLfsr`). The HH + SD percussion voices mix a noise
  source into their phase generators; per the independent silicon-RE
  measurement in
  `docs/audio/nsf/opll-ym2413/ym2413-noise-lfsr-andete-2018-05-13.txt`
  that source is a 23-bit maximal-length LFSR with polynomial
  `x^23 + x^9 + 1` (recovered repeatably by Berlekamp-Massey from the
  toggling-phase tail of the F-Num-0 snare-drum capture). The type
  encodes the measured hardware facts: the Galois single-operator step
  (`bit = state & 1; state >>= 1; if bit state ^= 0x40_0181`), the
  all-zero trap (a 0 state stays stuck — the chip must seed non-zero;
  `new()` seeds bit 0), the `2^23 - 1` maximal period, and the
  §"UPDATE" per-72-cycle rhythm-frame tap protocol (`rhythm_frame_bits`
  samples HH, iterates 3, samples SD, iterates 15 — 18 operator steps
  total). This is the shared noise source the §V-4 noise-mixed phase
  generators for HH/SD/TOM/TOP-CYM consume; their per-instrument phase
  formulas remain a docs gap (round 331)

- N163: multi-channel mixing per §"Mixing" of
  `docs/audio/nsf/namco-163-audio-wiki.html`. The chip time-multiplexes
  a single DAC across the active channels at the channel-update rate;
  the doc recommends "simply sum the channel outputs, and divide the
  output volume by the number of active channels" rather than reproduce
  the (aliasing / often-inaudible) switching waveform. The chip now
  sample-and-holds each active channel's last update separately
  (`chan_hold`) and `output()` averages them. Previously only the
  most-recently-ticked channel's sample was emitted, so a `c`-channel
  song dropped roughly `(c-1)/c` of its voices at any host sample —
  multi-voice N163 tracks were unbalanced. Single-channel output is
  unchanged (sum/1). Per the doc the approximation runs "slightly too
  loud" for `c >= 6`; that documented bound is accepted (round 327)
- OPLL rhythm mode: bass-drum (BD) percussion synthesis
  (`RhythmBassDrum`). Per YM2413 Application Manual §V-4 ("two slots
  are used to synthesize FM sounds") + Table III-9 (BD = slots 13+16 =
  channel 7's modulator/carrier pair), BD is the one percussion sound
  generated by the ordinary two-slot FM operator pipeline. It loads
  the fixed BD rhythm patch (`VRC7_RHYTHM_ROM[0]`), keys from the
  `$0E` D4 (BD) bit rather than `$2X`, tunes to channel 7's F-Num /
  Block (recommended §III-1-7 preset by default), takes the `$36`
  D3..D0 BD-VOL nibble at 3 dB/step, and doubles the output per §III-4
  Figure III-3(c) ("the same percussive sounds are output twice").
  HH/SD/TOM/TOP-CYM still need the §V-4 noise-mixed phase generator,
  whose per-instrument phase formulas remain a documented gap
  (round 323)
- OPLL: make the AM/VIB LFO audible — map the free-running tremolo /
  vibrato phase through a triangle scaled to the §7 *physical* depths
  (1.0 dB amplitude modulation / ±7-cent pitch modulation), gated by
  each operator's `$00`/`$01` AM / VIB enable bit (now wired from the
  patch into the operator pipeline). The exact emulator depth step
  arrays remain a documented §7 DOCS-GAP; the depth here is derived
  from the documented physical quantities, not a lifted table
  (round 319)
- MMC5: raw-PCM analog Pin 2 DAC transfer curve — replace the empirical
  PCM scale with the §"Pin 2 DAC Characteristic" affine equation
  `Voltage = (DAC/255)·0.4·AVcc + 0.1·AVcc` (AC-coupled about the
  0.3·AVcc midpoint) (round 315)

## [0.0.3](https://github.com/OxideAV/oxideav-nsf/compare/v0.0.2...v0.0.3) - 2026-06-15

### Other

- seed NSFe mixe table with documented per-device default mix levels
- Sunsoft 5B: select-port data-write lock-out (high nibble disables $E000) (round 307)
- Data Port ($4800) read-side auto-increment
- pulse 240 Hz envelope + length counter (no frame sequencer)
- round 290 — VRC6 pulse duty 15→0 down-count + E-bit phase reset
- §III-1-7 rhythm-mode register semantics + VRC7 no-rhythm-DAC carve-out (round 283)
- sound-driver identification tag at the start of the program data (round 279)
- emitted-frequency + channel-update-rate calibration API (round 274)
- wire AM/VIB LFO phase counters + $E000 audio-reset asymmetry (round 270)
- §III-7 envelope attack-time per RATE from YM2413 Application Manual Table III-7 (round 262)
- drop release-plz.toml — use release-plz defaults across the workspace
- §III-7 envelope decay-time per RATE from YM2413 Application Manual Table III-7 (round 232)
- §4 KSL byte base table from YM2413 Application Manual Table III-5 (round 228)
- sawtooth 14-step cycle + E-clear accumulator zero (round 223)
- $5010 PCM Mode/IRQ + $8000..=$BFFF read-mode write-by-read (round 18)
- §4 KSL formula scaffold + provenance prose scrub (round 17)
- OPLL KSR (Key Scale of RATE) per app-manual §III-1-2 Table III-2 (round 16)
- $0F test register + $2X.S sustain override + $00.S release disable + $E000 audio reset (round 15)

### Added

- **NSFe `mixe` per-device default mix levels** (round 311): per
  `docs/audio/nsf/nsfe-nesdev-wiki.html` §mixe "Any omitted device
  should instead use a default mix", the `Apu2A03` per-device gain
  table is now seeded from the documented signed-millibel defaults
  rather than a flat `1.0`. `MIXE_DEFAULT_MILLIBELS` pins the §mixe
  "Device byte values" list — APU Squares `0`, APU Triangle/Noise/DPCM
  `-20`, VRC6 `0`, VRC7 `1100`, FDS `700`, MMC5 `0`, N163 `1100`,
  Sunsoft 5B `-130` — and `Apu2A03::default_device_gains()` converts
  each via `10^(mB/2000)`. A device with no `mixe` entry now plays at
  its documented level (VRC7 ≈ 3.55x, FDS ≈ 2.24x, TND ≈ 0.977x, 5B ≈
  0.861x); an explicit `mixe` entry still replaces that device's
  default. The §mixe N163 default is documented as the literal "1100
  or 1900" string; the first-listed `1100` (matching the "compared in
  1-channel mode" note) is used and the `1900` alternative is flagged
  DOCS-GAP in the constant's doc-comment. 3 new tests cover the seeded
  defaults, the explicit-override-replaces-default semantic, and the
  updated unmentioned-slot-keeps-default invariant.
- **Sunsoft 5B select-port data-write lock-out** (round 307): per
  `docs/audio/nsf/sunsoft-5b-audio-wiki.html` §"Audio Register Select
  ($C000-$DFFF)" the select byte is `DDDDRRRR` — the high nibble
  `DDDD`, when nonzero, "Disable writes to $E000 if nonzero (like the
  original AY-3-8910)". The `$C000` write previously masked the byte to
  its low nibble and dropped the high nibble entirely, so a select with
  a nonzero high nibble incorrectly let the following `$E000` data-port
  write through. `Sunsoft5b` now tracks a `writes_disabled` flag set by
  the high nibble; while it is set, `$E000` writes are ignored, and a
  later select write with a zero high nibble re-enables the port. The
  low nibble always updates the selected register index regardless. 3
  new unit tests cover the disable, the re-enable, and the
  every-nonzero-high-nibble-blocks / zero-allows truth table.
- **N163 Data Port (`$4800`) read-side auto-increment** (round 301):
  the Namco 163 read port previously returned the sound-RAM byte at the
  current address but never advanced the internal pointer, even with the
  Address Port `I` bit set — an explicit TODO noted the increment lived
  only on the write path because `N163::read` took `&self`. Per
  `docs/audio/nsf/namco-163-audio-wiki.html` §"Address Port
  ($F800-$FFFF)" the address "will increment on writes **and reads** to
  the Data Port ($4800)" and §"Data Port" confirms "When read, the
  appropriate byte is returned." `N163::read` now takes `&mut self`
  (the whole `Expansion::read` → `Apu2A03::read_expansion` →
  `NesBus::read` chain was already `&mut`), returns the byte at the
  current address, then increments the pointer when `addr_inc` is set,
  clamping at `$7F` ("it does not wrap, instead stopping at $7F") to
  mirror the write path. A program that reads the wavetable back
  sequentially now walks sound RAM correctly. 5 new unit tests cover the
  non-incrementing read (pointer held), the incrementing read across
  three bytes, the `$7F` clamp, and the increment through the public
  `Expansion::read` router with the chip enabled.

- **MMC5 pulse 240 Hz envelope + length counter (no frame sequencer)**
  (round 294): the MMC5 pulse channels previously decoded `$5000`/
  `$5004` (duty / halt / constant / volume) and the `$5003`/`$5007`
  length register only at the byte level — the length counter was
  never clocked, the envelope decay was never generated, and
  `output()` always emitted the raw volume nibble. They now model the
  envelope + length unit per `docs/audio/nsf/mmc5-audio-wiki.html`
  §"Pulse 1 ($5000-$5003)" + §"Status ($5015)". The chip has "no
  equivalent frame sequencer (APU $4017); envelope and length counter
  are fixed to a 240hz update rate", so a new free-running
  `MMC5_FRAME_CPU`-cycle accumulator (the same 7457-cycle ≈240 Hz
  cadence the 2A03 frame counter uses) clocks both units each tick.
  The length counter now loads from the 2A03 `LENGTH_TABLE` on a
  `$5003`/`$5007` write (only while the channel is enabled in
  `$5015`), counts down at the 240 Hz clock — "twice as fast as the
  APU length counter" — silences the channel at 0, and is zeroed when
  the channel's `$5015` enable bit is cleared ("analogous to the APU
  Status register"). The envelope is the APU-identical decay generator
  (`$5003`/`$5007` write arms `env_start`; the shared `$5000` bit-5
  halt bit also acts as the envelope loop bit; bit 4 selects constant
  volume vs decay level), so `output()` now emits the decay level in
  envelope mode. The §"Pulse 1" "Frequency values less than 8 do not
  silence the MMC5 pulse channels" difference from the 2A03 is also
  honoured — the prior `timer_period >= 8` mute is removed, so sub-8
  periods emit ultrasonic tones. `$5001` remains unimplemented (the
  MMC5 pulse has no sweep). 13 new unit tests cover the
  LENGTH_TABLE-vs-raw-index load, the enabled-gating of the load, the
  240 Hz count-down to silence, the halt freeze, the disable-clears-
  length rule, envelope decay at 240 Hz, the envelope-period divider,
  the loop-on-halt wrap, the constant-vs-envelope volume select, the
  sub-8-period non-silence, the length-write envelope restart, and the
  full-period requirement of the 240 Hz clock.

- **VRC6 pulse duty generator 15→0 down-count + E-bit phase reset**
  (round 290): the VRC6 pulse channels now model the duty generator
  exactly as `docs/audio/nsf/vrc6-audio-wiki.html` §"Pulse Channels"
  describes — "The duty cycle generator takes 16 steps, counting down
  from 15 to 0. When the current step is less than or equal to the
  given duty cycle D, the channel volume V is output." The generator
  step now decrements 15→0 (wrapping back to 15) instead of the prior
  up-count, and a fresh chip seeds both pulses at the top of the
  countdown. The previously-missing §"Pulse Channels" disable
  semantic — "When the channel is disabled by clearing the E bit,
  output is forced to 0, and the duty cycle is immediately reset and
  halted; it will resume from the beginning when E is once again set"
  — now fires on the `$9002`/`$A002` E-bit falling edge: the duty
  generator is pinned to its beginning (step 15) and the timer
  reloaded, so the documented "reset phase by clearing and immediately
  setting E" technique lands the pulse at a deterministic phase. The
  duty *ratio* (D+1 of 16 steps high) and the M-mode 100 % override
  were already correct and are unchanged. 7 new unit tests cover the
  15→0 down-count + wrap, the D=3 → 4/16 duty ratio, the M-mode
  full-volume override across all 16 phases, the E-clear reset to
  step 15 + zero output + resume-from-beginning, and the
  clear-then-set phase-reset technique.

- **OPLL §III-1-7 rhythm-mode register semantics + VRC7 no-rhythm-DAC
  carve-out** (round 283): the rhythm-control register map is now
  decoded, per the Yamaha YM2413 Application Manual §III-1-7 /
  §III-1-8 (mirrored in
  `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`)
  and `docs/audio/nsf/vrc7-audio-wiki.html`. New in `opll`:
  `RhythmRegister` decodes `$0E` (`D5..D0` = `RHYTHM BD SD TOM TOP-CY
  HH`; D5 = 1 puts the OPLL in Rhythm mode with percussion through
  channels 7~9 and the melody section limited to six sounds);
  `RhythmInstrument` carries the **Table III-9** slot allocation
  (BD = slots 13 + 16, HH = 14, TOM = 15, SD = 17, TOP-CYM = 18) and
  the channel allocation derived from it + §V-4's "three channels and
  six slots" (BD owns channel 7 as the only two-slot FM pair; HH+SD
  share channel 8; TOM+TOP-CYM share channel 9); `RhythmVolumes`
  decodes the rhythm-mode `$36`~`$38` dual-volume nibbles (BD in
  `$36` low; HH high / SD low in `$37`; TOM high / T-CYM low in
  `$38`); `RHYTHM_FNUM_PRESET` pins the manual's recommended
  percussion F-Number/Block writes (`$16←$20 $17←$50 $18←$C0 $26←$05
  $27←$05 $28←$01`, Key-ON bits clear per "Key-ON bits $26, $27, $28
  must always be cleared to 0"). New in `expansion`:
  `VRC7_RHYTHM_ROM` pins the 3 drum patches in the VRC7 instrument
  ROM dump — inaudible there per §"Rhythm Register $0E" (no rhythm
  DAC) — including the documented snare-drum byte `$07` divergence
  (`$68` on VRC7 vs `$48` on YM2413); `Vrc7::rhythm_control()`
  surfaces the VRC7 carve-out (the rhythm-mode bit "is treated as
  though it were always enabled", `$0E` writes are ignored by the
  synthesis path, six audible FM channels always). Rhythm *synthesis*
  beyond the BD two-slot FM pair (the §V-4 noise-oscillator phases
  for HH/SD/TOM/TOP-CYM) is not numerically pinned by the staged
  material and stays out of scope — moot for VRC7. 7 new unit tests:
  the `$0E` per-bit decode (incl. D7/D6 exclusion), the Table III-9
  slot allocation + six-slot exact-cover property, the channel
  allocation + slot↔channel consistency, the `$36`~`$38` nibble
  decode, the F-Number preset + Key-ON-clear invariant, the rhythm
  ROM bytes + `$68` divergence, and a lockstep two-chip proof that a
  `$0E` write leaves the VRC7's melody output bit-identical.

- **NSFDRV sound-driver identification** (round 279): the 8-byte
  NSFDRV tag at the start of the program data is now decoded, per
  `docs/audio/nsf/nsfdrv-nesdev-wiki.html` §"File Format" (6-byte
  ASCII sound-driver ID at file offsets `$0080-$0085` in a plain NSF
  + major version byte at `$0086` + minor at `$0087`, immediately
  after the 128-byte header). A new `NsfDrvTag` struct carries the
  raw `id: [u8; 6]` / `major` / `minor`; `NsfDrvTag::read(program)`
  reads it off any 8-byte-plus program blob;
  `NsfDrvTag::known_id() -> Option<NsfDrvId>` classifies the ID
  against the wiki's §"List of NSFDRV sound driver IDs" registry —
  `NsfDrvId::Ofgs` (`"OFGS  "` = `$4F $46 $47 $53 $20 $20`),
  `NsfDrvId::Ftdrv` (`"FTDRV "` = `$46 $54 $44 $52 $56 $20`),
  `NsfDrvId::Nsdl` (`"NSDL  "` = `$4E $53 $44 $4C $20 $20`), and
  `NsfDrvId::Blank` (six spaces — "a blank NSFDRV ID may be used for
  sound drivers under development"). `NsfHeader::nsfdrv()` is the
  best-effort header-level surface: the wiki defines no presence
  predicate stronger than the ID registry itself, so the tag is
  reported only when the first 6 program bytes match a registered ID
  (anything else is treated as plain program code; callers can
  additionally filter out the ambiguous `Blank`). The tag is read
  from the parsed program blob for all three container shapes (plain
  NSF tail, NSFe `DATA` chunk, NSF2 pre-metadata program block).
  `NsfDrvTag::id_ascii()` renders printable-ASCII IDs for display.
  5 new unit tests pin the documented ASCII-vs-binary forms of all
  four registered IDs, end-to-end detection + major/minor byte
  placement through `parse_nsf`, ASCII rendering incl. the
  non-printable `None` case, the unregistered-ID / too-short-program
  negative paths, and detection through the NSFe `DATA` chunk.

- **N163 emitted-frequency + channel-update-rate calibration API**
  (round 274): the Namco 163's wavetable channels now expose their
  documented output frequency and update cadence as a first-class API,
  per `docs/audio/nsf/namco-163-audio-wiki.html` §"Channel Update" +
  §"Frequency". `N163::update_rate_hz(cpu_hz)` returns the per-channel
  refresh rate `cpu_hz / (15 * channels_active)` — the chip spends
  exactly 15 CPU cycles updating one channel and round-robins across
  the active set, so one channel is refreshed every `15 * c` cycles.
  `N163::emitted_frequency_hz(ch, cpu_hz)` implements the §"Frequency"
  closed form `f = (n * p) / (15 * 65536 * l * c)` (CPU clock × 18-bit
  frequency value, divided by the 15-cycle update period, the
  `l << 16` accumulator span of one full wave, and the channel count),
  returning 0 for inactive/silent channels. This closes the round-11
  N163 followup that left the emitted frequency verified only at the
  per-tick phase-advance level. 9 new unit tests validate
  `update_rate_hz` against the §"Channel Update" tabulated NTSC column
  (1 ch → 119.318 kHz down to 8 ch → 14.915 kHz) and PAL column
  (110.840 kHz down to 13.855 kHz), the no-channels-active zero case,
  the rate-halves-on-channel-doubling property, the
  `emitted_frequency_hz` closed form against a direct computation, its
  inverse scaling with channel count (the §"Frequency" note "the
  output frequency is thus divided by the number of channels enabled")
  and with wave length, the silent/out-of-range zero cases, and the
  PAL-clock frequency scaling by the `n_pal / n_ntsc` ratio.
- **OPLL AM/VIB LFO phase counters + `$E000` audio-reset asymmetry**
  (round 270): the VRC7/OPLL built-in tremolo (AM) + vibrato (VIB)
  low-frequency oscillators now carry phase counters at the spec'd
  cadence, per `docs/audio/nsf/vrc7-audio-wiki.html`
  §"Test Register $0F" + §"Audio Reset ($E000)". A new
  `opll::Lfo` struct advances a tremolo phase once every
  `opll::TREMOLO_LFO_DIVIDER` = 64 per-operator samples and a vibrato
  phase once every `opll::VIBRATO_LFO_DIVIDER` = 1024 samples in
  normal mode — the manual's bit-3 note "Tremolo is 64x faster, and
  vibrato is 1024x faster" describes the fast (`$0F` bit 3) mode where
  both dividers are bypassed and both advance once per sample. `$0F`
  bit 1 (hold) halts + resets both phases to zero per "Hold LFO phase
  at zero. This halts, disables, and resets both the tremolo and
  vibrato LFO." `Vrc7::tick` calls `Lfo::tick(hold_lfo, fast_lfo)`
  once per emitted operator sample so the phases track the chip's
  49.7163 kHz operator clock. The §"Audio Reset ($E000)" asymmetry
  "clear its registers (including tremolo LFO state, but not including
  vibrato LFO state)" is honoured by `Lfo::audio_reset` (called from
  the `$E000` bit-6 reset path): the tremolo phase is cleared while
  the vibrato phase is preserved. The two `$0F` bits 1 + 3 — recorded
  but inert since round 15 — now drive observable phase machinery.
  The numeric AM/VIB *depth* step arrays that translate phase into an
  audible attenuation (tremolo) / pitch offset (vibrato) remain a
  documented DOCS-GAP — flagged provenance-pending in the §7
  "Provenance & non-emulator sourcing" appendix of
  `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` — so the LFO has
  no audible effect yet; the phase→depth read is the single remaining
  edit once those arrays are staged. 7 new unit tests: the normal-mode
  64 / 1024 divider cadence, fast-mode every-sample advance, the
  bit-1 hold-resets-and-pins invariant, the audio-reset
  tremolo-cleared / vibrato-preserved asymmetry, hold-overrides-fast
  priority (5 in `opll`), plus 2 `Vrc7`-level integration tests that
  `Vrc7::tick` advances both phases at the right ratio and that
  `$E000` clears tremolo but preserves vibrato through the chip path.

- **OPLL §III-7 envelope attack-time per RATE — Yamaha YM2413
  Application Manual Table III-7** (round 262): the manual's
  "Attack and decay times in relation to RATE" table on page 14
  of the staged application manual
  (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`;
  HTML mirror `ym2413-application-manual-smspower.html`) is now
  also the source of truth for the OPLL envelope generator's
  per-RATE step magnitude on the Attack phase. The "EG attack
  time 0 dB → 40 dB" column is transcribed as
  `opll::TABLE_III_7_ATTACK_HUNDREDTHS_MS` (units of 0.01 ms),
  indexed by the post-key-scale `RATE = 4·R + Rks` (0..=63 — the
  same `RATE` produced by `Envelope::effective_rate`). The new
  helper `opll::attack_step_q16_per_sample(rate)` converts each
  table entry into the per-OPLL-sample Q16 envelope-level
  decrement that traverses the 40-dB attack span in the
  tabulated time at the OPLL operator clock (49.7163 kHz).
  `Envelope::step` now consults this helper in the Attack phase
  — the round-14 `2^(rate-1)` Q16-units-per-sample monotonic
  ladder is gone from the Attack phase too, so all four envelope
  phases (Attack / Decay / percussive-Sustain / Release) are
  now table-sourced. RATE 0..=3 are not tabulated by the manual
  (treated as halt); RATE 60..=63 (RM=15, any RL) are tabulated
  as `0.00 ms` and interpreted as instantaneous attack
  (`u32::MAX` step, saturating `level_q16` to zero in one
  sample). 6 new unit tests cover the table: five (RM, RL) spot
  checks against the manual (`RM=15 RL=0..3 → 0.00 ms`,
  `RM=1 RL=0 → 1730.15 ms`, `RM=8 RL=0 → 13.52 ms`,
  `RM=12 RL=0 → 0.84 ms`, `RM=6 RL=3 → 30.90 ms`,
  `RM=10 RL=2 → 2.25 ms`), the RATE-below-4 halt invariant with
  both table-zero and step-zero assertions, the RATE 60..=63
  instantaneous-attack saturation (envelope reaches
  `level_q16 == 0` in a single `step(1)` call), monotonicity of
  `attack_step_q16_per_sample` across RATE 4..=63, end-to-end
  traversal that `step × samples ≈ 40 dB` at RATE=32 (within
  ±2 %), the cross-column property that at every shared RATE
  the per-sample attack step is strictly larger than the
  decay step (the manual's attack column is uniformly shorter
  than the decay column at each RATE — attack is ≈10–12× faster
  than decay at the same RATE), and an end-to-end Envelope check
  that a slow attack (RATE=32) takes strictly more `step(1)`
  calls to clear the Attack phase than a fast attack (RATE=48).

  Note: the same "Likely transcription errors here, especially
  lower in the table" footnote that applied to the decay column
  applies here. The two visibly anomalous cells (`RM=9 RL=2` and
  `RM=3 RL=0`) surface only in the unused `10 % - 90 %` column;
  the consumed `0 dB - 40 dB` attack column is reproduced as
  printed.

- **OPLL §III-7 envelope decay-time per RATE — Yamaha YM2413
  Application Manual Table III-7** (round 232): the manual's
  "Attack and decay times in relation to RATE" table on page 14
  of the staged application manual
  (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`;
  HTML mirror `ym2413-application-manual-smspower.html`) is now
  the source of truth for the OPLL envelope generator's per-RATE
  step magnitude on the Decay, percussive-Sustain, and Release
  phases. The "EG decay time 0 dB → 40 dB" column is transcribed
  as `opll::TABLE_III_7_DECAY_HUNDREDTHS_MS` (units of 0.01 ms),
  indexed by the post-key-scale `RATE = 4·R + Rks` (0..=63 — the
  same `RATE` produced by `Envelope::effective_rate`). The new
  helper `opll::decay_step_q16_per_sample(rate)` converts a table
  entry into a per-OPLL-sample Q16 envelope-level increment that
  traverses the 40-dB span in the tabulated time at the OPLL
  operator clock (49.7163 kHz). The page-13 footnote
  "Attenuation times of the release rate are the same as that of
  the decay rate" is honoured by reusing the same lookup on the
  Release phase. Round 16's `2^(rate-1)` Q16-units-per-sample
  monotonic ladder is gone from those three phases (it remains on
  Attack pending a separate landing for the 10 %–90 % attack-curve
  column). RATE 0..=3 are not tabulated by the manual and default
  to halt; the `R=0 → RATE=0` carve-out from §III-1-2 is
  upstream-honoured by `effective_rate`. 4 new unit tests cover
  the table: spot-checks against five (RM, RL) cells of the
  manual (`RM=15 RL=3 → 1.27 ms`, `RM=1 RL=0 → 20926.60 ms`,
  `RM=8 RL=0 → 163.49 ms`, `RM=12 RL=0 → 10.22 ms`,
  `RM=6 RL=3 → 375.98 ms`), the RATE-below-4 halt invariant
  with both table-zero and step-zero assertions, monotonicity of
  `decay_step_q16_per_sample` across RATE 4..=63, end-to-end
  traversal that `step × samples ≈ 40 dB` at RATE=32 (within
  ±2 %), and a Decay-vs-Release parity check that the per-sample
  level delta matches between the two phases per the page-13
  footnote.

  Note: the manual's own caveat "Likely transcription errors
  here, especially lower in the table" applies to two visible
  anomalies (`RM=9 RL=2` and `RM=3 RL=0`) in the unused 10 %–90 %
  column; the consumed 0–40 dB column is reproduced as printed.

- **OPLL §4 KSL byte base table — Yamaha YM2413 Application Manual
  Table III-5** (round 228): the §4 KSL attenuation byte base table,
  previously a zero scaffold for blocks 1..=7, is now sourced from
  the staged vendor application manual at
  `docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf` page 11
  (Table III-5 "Attenuation at each F-Number at 3 dB/OCT"; matching
  HTML transcription `ym2413-application-manual-smspower.html`
  modulo two PDF→HTML typos at `2.625`/`14.625` — the PDF is the
  authoritative source). The 8×16 manual matrix is staged as
  `opll::KSL_BASE_BYTE_TABLE` with each dB entry scaled by `16/3`
  so the §4 right-shift `(base) >> (3 - KSL)` recovers env-level
  units (8 levels = 3 dB per the §6 / andete envelope-level
  relation) directly at KSL=2 — the manual's tabulated 3 dB/OCT
  rate. KSL=1 (`>> 2`) matches the manual's "Half of the above
  data at 1.5 dB/oct" note; KSL=3 (`>> 0`) matches "Double of the
  above at 6 dB/oct". The §4 formula plumbing already lit up in
  round 17 (`OpllChannel::sample_with_test`,
  `ksl_attenuation_env_levels`) consumes the filled table without
  any call-site changes. Round 17's "block 0 row bit-exact /
  blocks 1..=7 zero scaffold" carve-out is now obsolete: all 128
  cells are bit-correct against the manual. 4 new unit tests
  cover Table III-5 row entries at six spot-check (block, fnum_hi)
  cells, the manual's 3 dB/oct block-doubling property across all
  8 OCT rows at F-Num=15, the §4 right-shift formula at the
  non-zero block-7 corner (`KSL=3 → 112`, `KSL=2 → 56`,
  `KSL=1 → 28`, `KSL=0 → 0`), and a channel-pipeline KSL=3 vs
  KSL=0 attenuation contrast at block=5 / fnum_hi=15 that pins
  the post-Table-III-5 audio difference. Round 17's
  `channel_blocks_one_through_seven_currently_match_block_zero`
  scaffold trip-wire is REPLACED by
  `channel_ksl_high_attenuates_versus_ksl_zero`.

  Followup (docs collaborator): the staging
  `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §4 prose
  still describes the KSL byte table as "intentionally NOT
  reproduced verbatim here" and points at the OPLx-decapsulated
  independent-RE article — predating this round's observation that
  Table III-5 in the staged Yamaha application manual already
  tabulates the per-(OCT, F-Number) attenuation in dB units. A docs
  update to point §4 at Table III-5 directly would tighten the
  staging's own paper trail.

- **VRC6 sawtooth 14-step cycle + E-clear accumulator zero** (round 223):
  the §"Sawtooth Channel" 14-clock cycle in
  `docs/audio/nsf/vrc6-audio-wiki.html` is now matched bit-for-bit
  against the walked §example for A=$08. Previously the saw step
  counter used a `& 0x0D` bit mask that produced a malformed
  1/2/3/8/9/12/13 sequence; the new modulo-14 cycle produces the
  documented step 0..13 walk so the accumulator climbs 0,0,8,8,16,
  16,24,24,32,32,40,40,48,48 and resets back to 0 on the 14th clock
  matching the §"after A has been added 6 times, on the 7th clock,
  instead of A being added, the internal accumulator is reset to
  zero" rule. The §footnote "If A is more than 42 (floor(255 / 6)),
  the accumulator will wrap, resulting in distorted sound" is also
  covered — A=43 wraps the 8-bit accumulator past 255 on the 6th
  add. The §"Sawtooth Channel" E-clear rule ("If E is clear, the
  accumulator is forced to zero until E is again set") now triggers
  on the falling edge of the `$B002` E bit: `Vrc6Saw::accum` +
  `Vrc6Saw::step` are zeroed, the frequency divider is preserved
  per the §note "Clearing E does not reset the frequency divider,
  however, so the first step of the reset saw may appear
  shortened". A ticked-while-disabled chip holds the accumulator at
  zero (no spurious ramping when the saw is muted). 12 new unit
  tests in `expansion::tests` cover: the §example A=$08 14-step
  walk, the A=$01 two-cycle 6-add-then-reset pattern, the §"Output"
  5-bit DAC contribution (accum >> 3) over one full cycle, the
  E-clear forces-zero rule, the §note that E-clear preserves the
  frequency divider, the disabled-tick holds-zero invariant, the
  A=43 wrap-around distortion footnote, the A=0 silence guarantee,
  the `$B000` 6-bit rate field masking the top two bits (`..AA
  AAAA` layout), the re-enable phase-at-step-0 reset, the
  period-zero per-cycle step advance, and the §"Frequency Control
  ($9003)" halt-overrides-everything rule across the saw walker.

- **MMC5 PCM Mode / IRQ register + read-mode write-by-read** (round 18):
  the `$5010` PCM Mode/IRQ register now decodes bit 7 (PCM IRQ enable)
  alongside the existing bit 0 (mode select) on writes, and `$5010`
  reads return the `(irqTrip AND irqEnable)` bit per the
  `docs/audio/nsf/mmc5-audio-wiki.html` §"IRQ operation" pseudocode
  while acknowledge-clearing the `irqTrip` flag. `$5011` writes in
  write mode now honour the documented `value == 0 → irqTrip = 1,
  DAC unchanged` side-effect (and the symmetric non-zero → DAC
  update + irqTrip clear) instead of dropping the byte. A new
  `Mmc5::observe_prg_read(byte)` and bus hook on the
  `$8000..=$FFFF` read path implements the "Write-by-read writes to
  this register in PCM read-mode" semantic from §"Raw PCM ($5011)";
  the bus restricts the side-effect to the inclusive `$8000..=$BFFF`
  window per §"PCM description"'s explicit `$8000-BFFF` window.
  `Mmc5::irq_line()` exposes the `(irqTrip AND irqEnable)` cart-IRQ
  line, `Expansion::irq_line()` ORs it into the chip-aggregate, and
  `Apu2A03::irq_line()` ORs that into the existing frame-counter /
  DMC sources so `NesBus::irq_line` is now a 4-way OR (frame-counter,
  DMC, NSF2 timer, MMC5 PCM). New public surface: `Mmc5::irq_enable`,
  `Mmc5::irq_trip`, `Mmc5::observe_prg_read`, `Mmc5::irq_line`,
  `Expansion::irq_line`, `Expansion::observe_prg_read`,
  `Apu2A03::observe_prg_read`; `Mmc5::read` widened to `&mut self`,
  `Expansion::read` widened to `&mut self`, `Apu2A03::read_expansion`
  widened to `&mut self`. 16 new unit + bus integration tests cover
  the `$5010` write/read bit layout (including the §"MMC5A default
  power-on read value = $01" bit-0-mirror semantic), `$5011`
  zero / non-zero in write mode, `$5011` write inert in read mode,
  irq-trip acknowledge-on-read, the full `(irqTrip, irqEnable)`
  truth table, `observe_prg_read` in / out of read-mode and the
  chip-disabled defence-in-depth gate, the bus-level routing through
  the four-way IRQ OR, the inclusive `$8000..=$BFFF` window for
  write-by-read, and the no-op for write-mode reads in the same
  window.

- **VRC7 OPLL §4 KSL formula scaffold + provenance scrub** (round 17):
  the per-operator KSL field (`$02`/`$03` D7..D6, range 0..=3) is now
  captured from `Vrc7Patch::mod_ksl` / `Vrc7Patch::car_ksl` onto
  `OpllChannel::mod_ksl` / `OpllChannel::car_ksl` on every
  `load_patch`, and the §4 formula `(base[block][fnum_hi]) >> (3 - KSL)`
  is wired through `OpllChannel::sample_with_test` for both operators
  per `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §4. New public
  helpers: `ksl_attenuation_env_levels(block, fnum_hi, ksl) -> u32`
  (full per-operator contribution honouring the §4 `KSL=0 → no
  contribution` carve-out and the `>> (3 - KSL)` per-octave scaling),
  `ksl_base_attenuation(block, fnum_hi) -> u32` (table indexing with
  the documented 3-bit-block + 4-bit-fnum_hi mask), and the constant
  `KSL_BASE_BYTE_TABLE: [[u32; 16]; 8]`. The base byte table is the
  §4 zero scaffold: row 0 (block 0) is bit-exact per the §4 schema's
  explicit "block 0: 0 0 0 0 0 0 0 0" — block 0 streams therefore
  flow through the new KSL pipeline producing the same samples as
  pre-round-17 (zero KSL contribution), and the §4-byte-base-table
  staging will be a single-cell edit to fill rows 1..=7 without any
  call-site touch. The trip-wire test
  `channel_blocks_one_through_seven_currently_match_block_zero` MUST
  fail once a non-zero base table is staged, signalling the channel
  pipeline path needs the per-block first-sample validation re-run.
  + 9 new unit tests covering: §6 peak-amplitude monotonicity across
  the full row (each volume's max amplitude ≤ the previous volume's),
  the §4 KSL=0 carve-out across all 128 (block, fnum_hi) cells, the
  §4 block-0-is-bit-exact-zero rule across all 64 (fnum_hi, KSL)
  combinations, the `(base) >> (3 - KSL)` formula arithmetic, the §4
  base-table block-0-row-all-zero invariant, input-bit masking
  (block masked to 3 bits, fnum_hi to 4 bits per §4 indexing), the
  channel-level KSL field capture from the patch, the block-0 KSL=3
  vs KSL=0 sample-equivalence (32-sample stream), and the cross-block
  scaffold invariant. The §4 byte base table (rows 1..=7), the §7
  per-rate envelope increment array, and the §7 AM/VIB LFO step
  arrays remain documented DOCS-GAP followups flagged
  provenance-pending in `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md`
  §"Provenance & non-emulator sourcing".

### Changed

- **opll module + crate README provenance prose** (round 17): scrubbed
  pre-existing enumerated denial prose in `src/opll.rs`, `README.md`,
  and `CHANGELOG.md` to neutral provenance-pending language. The
  staged §"Provenance & non-emulator sourcing" appendix carries the
  actual chain-of-custody; the module / README / CHANGELOG no longer
  enumerate non-consulted source trees.

- **VRC7 OPLL KSR (Key Scale of RATE)** (round 16): the per-operator
  `KSR` bit (`$00`/`$01` D4) on every channel now amplifies the
  envelope's per-stage RATE by the pitch-derived `Rks` offset per
  the YM2413 Application Manual §III-1-2 + Table III-2 in
  `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`.
  Each operator's `Envelope::ksr` is loaded from the patch's
  `mod_ksr` / `car_ksr` field on every `OpllChannel::load_patch`,
  and `Envelope::update_rks(block, fnum_msb)` computes the cached
  `Rks` offset: `KSR=0` → `Rks = block >> 1` (the D4=0 row reads
  `0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3` across the 16 (block,
  fnum-MSB) columns); `KSR=1` → `Rks = (block << 1) | fnum_msb`
  (D4=1 row reads `0..15`). The 4-bit per-stage R from the patch
  is widened to a 6-bit RATE via the manual's `RATE = 4·R + Rks`
  formula in the new `Envelope::effective_rate(r)`, with the
  explicit "Note that when R=0, RATE=0" carve-out honoured (any R
  field set to 0 still halts the envelope regardless of pitch).
  A pure pitch-only `$1X` / `$2X` register write (no patch or
  volume change) re-derives both operators' `Rks` via the new
  `OpllChannel::refresh_rks` so a glide-mid-note honours the new
  pitch's rate amplification on the very next envelope step. The
  Q16 step shift caps at 31 (RATE values beyond that saturate the
  envelope against `ENV_MAX_LEVEL` in one sample anyway). The
  per-RATE numeric increment table from the OPLx-decapsulated §7
  array remains the documented DOCS-GAP followup — KSR's
  contribution to per-rate amplification is now bit-correct, but
  the absolute per-RATE step magnitude is still the coarse
  `2^(rate-1)` Q16-units-per-sample approximation from round 14.
- 7 new unit tests covering: the §III-1-2 Table III-2 D4=0 row
  (`Rks = block >> 1` across all 16 (block, fnum_msb) columns),
  the D4=1 row (`Rks = (block << 1) | fnum_msb`), the
  `RATE = 4·R + Rks` formula with the `R=0 → RATE=0` halt
  carve-out and the 63-cap, the end-to-end behavioural check that
  KSR=1 at (block=7, fnum_msb=1) reaches the sustain level
  strictly faster than at (block=0, fnum_msb=0), the
  smaller-but-non-zero sensitivity of the KSR=0 row, the
  `OpllChannel::refresh_rks` per-operator KSR-bit selection, and
  the `OpllChannel::load_patch` path that picks up the patch's
  KSR bit and immediately re-derives Rks against the channel's
  current pitch. +2 new integration tests in `expansion.rs`
  covering the pitch-only `$2X` register write path that calls
  `refresh_from_regs` → `refresh_rks` on both operators, and the
  patch-swap path that updates Rks via `refresh_from_regs` →
  `load_patch` → `refresh_rks` (verified against the dumped
  `$A` "Vibes" preset whose `$00 = 0xB5` has the modulator KSR
  bit set and `$01 = 0x01` does not).

- **VRC7 test register `$0F` + per-channel sustain override + modulator
  release-disable + audio reset** (round 15): four fully-spec'd VRC7
  semantics from `docs/audio/nsf/vrc7-audio-wiki.html` land without
  needing any of the missing OPL-family numeric tables. (1) §"Test
  Register $0F" — the new `opll::TestRegister` decodes the low 4 bits
  (bit 0 envelopes-forced-zero / full volume, bit 1 LFO-phase-hold,
  bit 2 waveform-phase-hold, bit 3 LFO-speed override), the chip
  caches it as `Vrc7::test_register`, and per-operator sampling
  consults it via the new `OpllChannel::sample_with_test`. Bit 0
  bypasses the envelope's exp-offset on both modulator and carrier
  while envelopes keep ticking; bit 2 pins both operator phase
  accumulators at 0 (output goes silent without halting envelopes);
  bits 1 and 3 are recorded so a future LFO landing inherits the
  gate. (2) §Channels — `$2X.S` now drives both operators' release
  rate to `$5` (overriding the patch) via the new
  `OpllChannel::set_channel_sustain_override`, with revert-to-patch
  on clear. The patch-load path re-applies the override so a patch
  swap mid-sustain doesn't lose it. (3) §"Custom Patch" — the
  modulator's `$00.S` is dual-role per the wiki; in addition to
  EG-TYP it disables the release section of the modulator's
  envelope entirely. The new `Envelope::release_disabled` flag is
  set from `p.mod_sustain` on patch load (and explicitly cleared on
  the carrier per the wiki's "the carrier does not behave this
  way" carve-out); `Envelope::key_off` is a no-op when set. (4)
  §"Audio Reset ($E000)" — `Vrc7::write(0xE000, …)` now reads bit 6
  (R); setting it silences the chip (`latched_output` pinned to 0,
  no operator ticks), clears all registers + channel state, and
  blocks subsequent writes to `$9010` / `$9030`; clearing it
  restores writes. The §LFO clear qualifier is a no-op since the
  LFO isn't yet ticked (§7 DOCS-GAP). +14 new unit tests covering
  the TestRegister bitfield decode, bit-0-forces-full-volume + still
  ticks, bit-2-silences-via-phase-hold, channel sustain override
  swap + revert, modulator-S release-disable on key-off + carrier
  not affected, `$0F` indirect-port write updates the cached
  struct, chip-level phase-hold silences, `$2X.S` channel-level
  release-rate override, modulator-only release-disable, `$E000`
  bit 6 clears registers + blocks indirect-port writes, tick
  silenced during reset, and the bit-6-only-bit-matters check. The
  existing `channel_key_off_moves_envelopes_to_release` regression
  was updated to use a non-modulator-sustained patch so the
  modulator's new release-disable behaviour doesn't contaminate the
  unrelated carrier+modulator transition assertion.

## [0.0.2](https://github.com/OxideAV/oxideav-nsf/compare/v0.0.1...v0.0.2) - 2026-05-29

### Other

- real OPLL operator pipeline (round 14)
- patch table + per-channel patch selection (round 13)
- Sunsoft 5B: noise + envelope generators (round 12)
- per-channel timer accumulators (round 11)
- $4090..=$4097 read-register window (round 10)
- $4023.D1 master sound-enable / waveform-halt (round 9)
- $4083 bit 7 also halts the mod-table accumulator (round 8)
- volume + mod envelope ramp generators (round 8)
- wire the frequency-modulation unit into the wave pitch
- region-aware noise period table (NTSC + PAL)
- Round 5: Dendy region + NSFe mixe gain overrides + plst/psfx player API
- Round 4: NSFe extended chunks + APU IRQ wiring
- Round 3: NSF 2.x support — feature byte, IRQ timer, vector overlay, two-phase INIT
- drop committed Cargo.lock + relax oxideav-core to "0.1"
- nsf bus: add unit tests for flat load + bankswitching + RAM mirroring
- Round 2: full unofficial 6502 opcodes, DMC DMA, expansion chips, real-rip cross-check

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
  the §5 FB feedback π-multiple table are transcribed from the §3
  and §5 tables in `opll-ym2413-tables.md`. An `Operator` carries a 19-bit phase
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
  attenuation byte base table (§4, requires the OPL-family base
  table that the staging flags provenance-pending) and the per-rate
  envelope-increment numeric arrays (§7, same provenance reason)
  remain documented followups; rhythm-mode
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
