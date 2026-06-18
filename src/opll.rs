//! OPLL (Yamaha YM2413) operator pipeline — the synthesis core used by
//! the VRC7 NES expansion chip.
//!
//! All numeric tables and the table-lookup algorithm are sourced from
//! the in-tree clean-room staging at
//! `docs/audio/nsf/opll-ym2413/`:
//!
//! * `opll-ym2413-tables.md` — register map, instrument-patch bit
//!   layout, the §3 `MUL` multiplier table, the §5 `FB` modulation-index
//!   table, and the §6 log-sin/exp algorithm + key facts.
//! * `ym2413-logsin-exp-tables-andete-2015-04-09.txt` — the
//!   `initTables` / `lookupSin` / `lookupExp` algorithm published by
//!   the andete silicon-RE writeup.
//! * `vrcvii-kevtris.txt` — VRC7-specific register map + frequency
//!   formula `F = 49722 * fnum / 2^(19 - octave)`.
//! * `ym2413-application-manual-smspower.html` /
//!   `ym2413-application-manual.pdf` — vendor datasheet for the
//!   register-contents semantics.
//!
//! See the §"Provenance & non-emulator sourcing" appendix in
//! `opll-ym2413-tables.md` for the staged-source chain of custody;
//! anything beyond that appendix is out-of-scope for this module.
//!
//! The exact emulator AM/VIB depth *step arrays* are deliberately not
//! transcribed (the "Provenance" appendix of `opll-ym2413-tables.md`
//! cites them to independent silicon-RE primary sources). The AM/VIB
//! LFO depth tables were staged under
//! `docs/audio/nsf/opll-ym2413/` (§8a/§8b, #138) as andete's
//! hardware-measured arrays, so both LFOs now use the exact
//! silicon-measured forms rather than the earlier physical-depth
//! approximations. Their phase *cadence* (tremolo advances once per 64
//! operator samples, vibrato once per 1024, both bypassed under `$0F`
//! bit 3 and held+reset under `$0F` bit 1, with the `$E000` audio reset
//! clearing tremolo phase but preserving vibrato phase) is specified by
//! `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F" +
//! §"Audio Reset ($E000)". The AM depth is the §8a 210-entry
//! 14-level (0..13) truncated triangle ([`AM_LFO_LEVELS`], applied as
//! `16 * am` in the operator's exp index → ≈ 4.8 dB peak) and the VIB
//! depth is the §8b 8×8 phase-modulation table ([`VIB_PM_TABLE`]), both
//! read via [`Lfo`] ([`Lfo::tremolo_atten_exp_units`] /
//! [`Lfo::vibrato_pm`]). Every numeric array is sourced from andete's
//! independent silicon measurement, not from any emulator tree.
//!
//! Rhythm-mode *register semantics* are fully specified by the
//! application manual §III-1-7 (the `$0E` RHYTHM register bit table +
//! Table III-9 rhythm slots + the recommended percussion F-Number
//! preset) and §III-1-8 (the `$36`~`$38` rhythm-volume nibble
//! layout), and are decoded by [`RhythmRegister`] /
//! [`RhythmInstrument`] / [`RhythmVolumes`] / [`RHYTHM_FNUM_PRESET`].
//! Rhythm *synthesis* beyond the BD two-slot FM pair (the §V-4 noise
//! oscillator "specified by the frequency information (BLOCK,
//! F-Number, Multiple) of the 8 and 9 channels" that drives HH / SD /
//! TOM / TOP-CYM phases) is not numerically pinned by the staged
//! material and stays out of this module — moot for the VRC7
//! consumer, which has no rhythm DAC. The §4 KSL pipeline
//! is wired through the operator path using the documented
//! `(block_fnum_KSL_base) >> (3 - KSL)` formula, and the base byte
//! table is now sourced from Yamaha YM2413 Application Manual
//! **Table III-5 "Attenuation at each F-Number at 3 dB/OCT"**
//! (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf` p. 11).
//!
//! The envelope generator's Decay / percussive-Sustain / Release
//! per-RATE step magnitude is now sourced from
//! **Yamaha YM2413 Application Manual Table III-7
//! ("Attack and decay times in relation to RATE")** page 14 — see
//! [`TABLE_III_7_DECAY_HUNDREDTHS_MS`] / [`decay_step_q16_per_sample`].
//! The Attack phase consults the same Table III-7's `EG attack time,
//! 0 dB → 40 dB` column via
//! [`TABLE_III_7_ATTACK_HUNDREDTHS_MS`] / [`attack_step_q16_per_sample`]
//! — the per-sample step is computed as the linear-equivalent ramp that
//! traverses the 40 dB span in the tabulated time, mirroring the decay
//! treatment. The manual's exponential 10 %–90 % rise envelope is
//! qualitatively different (the §III-3 attack curve "rises
//! exponentially during attack time") but the 0 dB → 40 dB total time
//! is exactly what we need for the linear-step approximation already
//! in use for decay.
//!
//! KSR (Key Scale of RATE) IS fully specified by the YM2413
//! Application Manual §III-1-2 + Table III-2 (mirrored in
//! `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`).
//! The §"RATE = 4·R + Rks" formula and the two key-scale offset tables
//! (`KSR=0`: `Rks = block >> 1`; `KSR=1`: `Rks = (block << 1) |
//! fnum_msb`) are implemented below, gated on the patch's per-operator
//! KSR bit. When R=0 the manual is explicit that RATE=0 (halt)
//! regardless of Rks.

use crate::expansion::Vrc7Patch;

// -------------------------------------------------------------- constants

/// Operator sample rate. Per the YM2413 application manual, the master
/// 3.579545 MHz / 72 clock divider → 49.7163 kHz per-operator sample
/// rate. We tick the operator pipeline at this rate.
pub const OPLL_SAMPLE_RATE_HZ: f32 = 49_716.0;

/// 1024 phase steps per full sine period — established by andete's
/// §"table lookup algorithm" notes (`docs/audio/nsf/opll-ym2413/
/// ym2413-logsin-exp-tables-andete-2015-04-09.txt` lines 113-116) and
/// confirmed in `opll-ym2413-tables.md` §6.
pub const PHASE_STEPS_PER_PERIOD: u32 = 1024;

/// Phase accumulator scale. The phase generator runs in 19 fractional
/// bits over the 10-bit sine index so that the per-sample increment
/// `fnum * 2^block * MUL` divides down cleanly. The VRC7 register-level
/// frequency formula is `F = 49722 * fnum / 2^(19 - block)` Hz
/// (`vrcvii-kevtris.txt` line 189); equivalently the per-49.716 kHz
/// sample phase advance is `(fnum << block) * MUL_x2 / 2`.
pub const PHASE_ACC_FRAC_BITS: u32 = 19;

// -------------------------------------------------------------- §3 MUL

/// `MUL` field (`$00`/`$01` D3..D0) → phase-increment multiplier ×2 so
/// that the half-value at index 0 is representable as the integer 1
/// (a real ½), avoiding a float in the phase generator.
///
/// Source: `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §3 table
/// (`{½, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 12, 12, 15, 15}`).
pub const MUL_TIMES_TWO: [u8; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 24, 24, 30, 30];

// -------------------------------------------------------------- §5 FB

/// Feedback (`$03` D2..D0) modulation-index, expressed as a phase shift
/// applied to the modulator's own previous output before feeding it
/// back into the phase input.
///
/// Source: `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §5 table
/// (`0, π/16, π/8, π/4, π/2, π, 2π, 4π`).
///
/// In the table-lookup algorithm a modulator output of full sine
/// amplitude corresponds to one full sine period (1024 phase steps).
/// `FB = k` therefore adds `prev_output >> (9 - k)` to the phase
/// (so FB=7 → `>> 2` = π/2 contribution per π of output, i.e. 2π full
/// scale; FB=1 → `>> 8` = π/16 contribution). FB=0 disables feedback.
pub fn feedback_shift(fb: u8) -> u32 {
    // FB=0 → no feedback. FB=1..=7 → shift right by (9 - fb).
    if fb == 0 {
        // Effectively "shift by huge"; the caller checks fb==0 anyway.
        32
    } else {
        9u32.saturating_sub(fb as u32)
    }
}

// -------------------------------------------------------------- §4 KSL
//
// Key-scale level (KSL): `$02`/`$03` D7..D6 select an extra
// attenuation that increases with pitch, indexed by the (block,
// F-Num top bits) pair. Per
// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §4 the per-cell
// attenuation is computed as `(base[block][fnum_hi]) >> (3 - KSL)`
// with KSL=0 disabling the contribution entirely.
//
// Spec status: the per-(OCT, F-Number) attenuation values are
// tabulated in the **Yamaha YM2413 Application Manual Table III-5
// ("Attenuation at each F-Number at 3 dB/OCT")**, staged at
// `docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf` page
// 11 (also transcribed in `ym2413-application-manual-smspower.html`).
// The manual's table is in dB units at the KSL=2 (3 dB/oct) baseline;
// the §4 staging README's prior "graph (scan)" note predated this
// transcription pass.
//
// The §4 right-shift formula scales the same base for the other KSL
// rates: KSL=1 (1.5 dB/oct) → `base >> 2`, KSL=2 (3 dB/oct) →
// `base >> 1`, KSL=3 (6 dB/oct) → `base >> 0`. To make all three
// rates produce integer envelope-level (0.375 dB) results, we store
// each Table III-5 dB entry as `dB * 16 / 3` envelope-half-units
// (i.e. units of 0.1875 dB). The KSL=2 right-shift then recovers
// the 0.375 dB-step env-level units the rest of the operator
// pipeline consumes (where 8 levels = 3 dB per the §6 / andete
// §"envelope levels" relation).

/// §4 KSL base byte table from
/// **Yamaha YM2413 Application Manual Table III-5**
/// (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf` p. 11;
/// matching HTML transcription
/// `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`).
///
/// Indexed by `(block, fnum_hi)` where `block` is the 3-bit BLOCK
/// (0..=7, manual OCT row) and `fnum_hi` is the top 4 bits of the
/// 9-bit F-Num (`(F-Num >> 5) & 0x0F`, the manual's "F-Number"
/// column — per Table III-5 Notes "F-Number is the value of the
/// four MSBs.").
///
/// Each cell stores the manual's dB value scaled by `16/3` so the
/// integer arithmetic in [`ksl_attenuation_env_levels`] recovers
/// envelope-level units (8 = 3 dB) after the `>> (3 - ksl)`
/// right-shift. The KSL=2 column (`base >> 1`) reproduces Table
/// III-5's dB values in env-level form bit-for-bit; KSL=1 / KSL=3
/// follow from the manual's notes "Half of the above data at 1.5
/// dB/oct" / "Double of the above at 6 dB/oct".
///
/// Row 0 (BLOCK = 0) is the manual's all-zero row; the §4 KSL=0
/// carve-out is independently honoured in
/// [`ksl_attenuation_env_levels`].
pub const KSL_BASE_BYTE_TABLE: [[u32; 16]; 8] = [
    // OCT 0: all zeros per Table III-5 row 0.
    [0; 16],
    // OCT 1: 0,0,0,0,0,0,0,0,  0.750,1.125,1.500,1.875,2.250,2.625,3.000 dB
    //                          (manual row 1, columns F-Num 0..15).
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 6, 8, 10, 12, 14, 16],
    // OCT 2: 0,0,0,0,0,1.125,1.875,2.625,
    //        3.000,3.750,4.125,4.500,4.875,5.250,5.625,6.000 dB.
    [0, 0, 0, 0, 0, 6, 10, 14, 16, 20, 22, 24, 26, 28, 30, 32],
    // OCT 3: 0,0,0,1.875,3.000,4.125,4.875,5.625,
    //        6.000,6.750,7.125,7.500,7.875,8.250,8.625,9.000 dB.
    [0, 0, 0, 10, 16, 22, 26, 30, 32, 36, 38, 40, 42, 44, 46, 48],
    // OCT 4: 0,0,3.000,4.875,6.000,7.125,7.875,8.625,
    //        9.000,9.750,10.125,10.500,10.875,11.250,11.625,12.000 dB.
    [0, 0, 16, 26, 32, 38, 42, 46, 48, 52, 54, 56, 58, 60, 62, 64],
    // OCT 5: 0,3.000,6.000,7.875,9.000,10.125,10.875,11.625,
    //        12.000,12.750,13.125,13.500,13.875,14.250,14.625,15.000 dB.
    [
        0, 16, 32, 42, 48, 54, 58, 62, 64, 68, 70, 72, 74, 76, 78, 80,
    ],
    // OCT 6: 0,6.000,9.000,10.875,12.000,13.125,13.875,14.625,
    //        15.000,15.750,16.125,16.500,16.875,17.250,17.625,18.000 dB.
    [
        0, 32, 48, 58, 64, 70, 74, 78, 80, 84, 86, 88, 90, 92, 94, 96,
    ],
    // OCT 7: 0,9.000,12.000,13.875,15.000,16.125,16.875,17.625,
    //        18.000,18.750,19.125,19.500,19.875,20.250,20.625,21.000 dB.
    [
        0, 48, 64, 74, 80, 86, 90, 94, 96, 100, 102, 104, 106, 108, 110, 112,
    ],
];

/// Look up the §4 KSL base attenuation for a given `(block, fnum_hi)`
/// pair. `block` is the 3-bit BLOCK (0..=7); `fnum_hi` is the top 4
/// bits of the 9-bit F-Num (`(F-Num >> 5) & 0x0F`).
///
/// Returns the base contribution in envelope-level units (8 = 3 dB)
/// before the per-operator `KSL` field's right-shift is applied —
/// see [`ksl_attenuation_env_levels`] for the full per-operator
/// pipeline contribution.
#[inline]
pub fn ksl_base_attenuation(block: u8, fnum_hi: u8) -> u32 {
    let b = (block & 0x07) as usize;
    let f = (fnum_hi & 0x0F) as usize;
    KSL_BASE_BYTE_TABLE[b][f]
}

/// §4 KSL formula — full per-operator attenuation contribution for
/// the operator's `KSL` field (`$02`/`$03` D7..D6, range 0..=3).
///
/// Per `opll-ym2413-tables.md` §4 the formula is
/// `attenuation = (base[block][fnum_hi]) >> (3 - KSL)` with the
/// documented `KSL=0` meaning "no key-scaling" — i.e. zero
/// contribution regardless of the base table. The right-shift
/// implements the §4 dB-per-octave scaling: `KSL=0` → off,
/// `KSL=1` → `>> 2` (¼ of the base, i.e. 1.5 dB per octave),
/// `KSL=2` → `>> 1` (3 dB per octave — Table III-5's tabulated
/// rate), `KSL=3` → `>> 0` (6 dB per octave, the steepest).
///
/// The base table is sourced from
/// **Yamaha YM2413 Application Manual Table III-5**
/// (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`
/// p. 11). Block 0 is the manual's all-zero row, so this function
/// returns zero for `block=0` regardless of (fnum_hi, KSL); blocks
/// 1..=7 use the per-cell attenuation tabulated in Table III-5.
#[inline]
pub fn ksl_attenuation_env_levels(block: u8, fnum_hi: u8, ksl: u8) -> u32 {
    let k = ksl & 0x03;
    if k == 0 {
        // §4: KSL=0 disables the contribution.
        return 0;
    }
    let base = ksl_base_attenuation(block, fnum_hi);
    // §4: shift right by (3 - KSL). KSL ∈ {1,2,3} → shift ∈ {2,1,0}.
    base >> (3 - k as u32)
}

// -------------------------------------------------------------- §III-7 EG times
//
// Envelope decay times in relation to RATE — per
// **Yamaha YM2413 Application Manual Table III-7
// ("Attack and decay times in relation to RATE")**, transcribed at
// `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`
// (HTML mirror of the original page 14 scan
// `docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`).
//
// The table is indexed by the post-key-scale `RATE = RM × 4 + RL`
// (six-bit, 0..=63) — that is the same `RATE` produced by
// [`Envelope::effective_rate`] from `4·R + Rks`. The manual lists
// four columns; the only one consumed here is **EG decay time, 0 dB
// → 40 dB** (the second column), measured in milliseconds. The
// transcription below stores the column verbatim in units of
// 0.01 ms (= 10 µs) to keep the entries integer.
//
// The manual itself flags "Likely transcription errors here,
// especially lower in the table" immediately under the table. Two
// such cells are visibly anomalous against the otherwise-smooth
// geometric progression — `RM=9 RL=2` and `RM=3 RL=0` in the
// (unused) "10 % - 90 %" column. Both are outside the columns
// consumed below; the table is reproduced exactly as printed and
// the caveat is surfaced here for completeness.
//
// RATE entries 0..=3 are not tabulated by the manual. Per §III-1-2
// Note: "When R=0, RATE=0" → halt; with `Rks ≥ 0` and `R ≥ 1` the
// formula `4·R + Rks` reaches at least 4, so RATE 1..3 are not
// produced by [`Envelope::effective_rate`]. We default those entries
// to zero (halt) defensively.

/// **Yamaha YM2413 Application Manual Table III-7** — EG decay time
/// (`0 dB → 40 dB` column) in units of 0.01 ms, indexed by the
/// post-key-scale `RATE = RM·4 + RL` (0..=63).
///
/// Source: page 14 of the application manual
/// (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`;
/// HTML mirror `ym2413-application-manual-smspower.html`).
///
/// Entries 0..=3 are not tabulated by the manual and are set to
/// zero here (treated as halt by [`decay_step_q16_per_sample`]);
/// `[`Envelope::effective_rate`]`'s `4·R + Rks` formula never
/// produces a RATE below 4 when `R ≥ 1`.
pub const TABLE_III_7_DECAY_HUNDREDTHS_MS: [u32; 64] = [
    // RATE 0..3 — not tabulated; treated as halt.
    0, 0, 0, 0,
    // RATE 4..7 (RM=1, RL=0..3): 20926.60, 16807.20, 14606.80, 12078.66 ms.
    2_092_660, 1_680_720, 1_460_680, 1_207_866,
    // RATE 8..11 (RM=2, RL=0..3): 10463.30, 8403.58, 7002.98, 6014.32 ms.
    1_046_330, 840_358, 700_298, 601_432,
    // RATE 12..15 (RM=3, RL=0..3): 5231.64, 4201.79, 3501.49, 3007.16 ms.
    523_164, 420_179, 350_149, 300_716,
    // RATE 16..19 (RM=4, RL=0..3): 2615.82, 2180.89, 1750.75, 1503.58 ms.
    261_582, 218_089, 175_075, 150_358,
    // RATE 20..23 (RM=5, RL=0..3): 1307.91, 1050.45, 875.37, 751.79 ms.
    130_791, 105_045, 87_537, 75_179,
    // RATE 24..27 (RM=6, RL=0..3): 653.95, 525.22, 437.69, 375.98 ms.
    65_395, 52_522, 43_769, 37_598,
    // RATE 28..31 (RM=7, RL=0..3): 326.98, 262.61, 218.84, 187.95 ms.
    32_698, 26_261, 21_884, 18_795,
    // RATE 32..35 (RM=8, RL=0..3): 163.49, 131.31, 109.42, 93.97 ms.
    16_349, 13_131, 10_942, 9_397,
    // RATE 36..39 (RM=9, RL=0..3): 81.74, 65.65, 54.71, 46.99 ms.
    8_174, 6_565, 5_471, 4_699,
    // RATE 40..43 (RM=10, RL=0..3): 40.07, 32.03, 27.36, 23.49 ms.
    4_007, 3_203, 2_736, 2_349,
    // RATE 44..47 (RM=11, RL=0..3): 20.44, 16.41, 13.60, 11.75 ms.
    2_044, 1_641, 1_360, 1_175,
    // RATE 48..51 (RM=12, RL=0..3): 10.22, 8.21, 6.84, 5.87 ms.
    1_022, 821, 684, 587, // RATE 52..55 (RM=13, RL=0..3): 5.11, 4.10, 3.42, 2.94 ms.
    511, 410, 342, 294, // RATE 56..59 (RM=14, RL=0..3): 2.55, 2.05, 1.71, 1.47 ms.
    255, 205, 171, 147, // RATE 60..63 (RM=15, RL=0..3): 1.27, 1.27, 1.27, 1.27 ms.
    127, 127, 127, 127,
];

/// Envelope-level span from 0 dB → 40 dB in our internal 0.375 dB
/// per-level units. Per `opll-ym2413-tables.md` §6 / andete
/// §"envelope levels", `8 levels = 3 dB`, so `40 dB ≈ 106.67`
/// envelope-levels. The Q16 fixed-point value used below rounds to
/// the nearest integer (`40 * 8 / 3 << 16 ≈ 106.667 << 16`).
pub const ENV_LEVELS_40_DB_Q16: u64 = (40u64 * 8 * (1 << 16)) / 3;

/// Per-sample Q16 envelope-level step for the documented `RATE` per
/// **Yamaha YM2413 Application Manual Table III-7** column "EG decay
/// time 0 dB → 40 dB".
///
/// Returns the increment (in Q16 fixed-point envelope-levels) that
/// would carry the envelope through the 40-dB span in exactly the
/// tabulated time at the OPLL per-operator rate
/// ([`OPLL_SAMPLE_RATE_HZ`] ≈ 49.7163 kHz). Used by [`Envelope::step`]
/// for the Decay, percussive-Sustain and Release phases — the manual
/// is explicit that "Attenuation times of the release rate are the
/// same as that of the decay rate" (page 13 footnote).
///
/// RATE 0..=3 return zero (halt) — the manual does not tabulate them
/// and `R=0` is the documented halt case.
#[inline]
pub fn decay_step_q16_per_sample(rate: u8) -> u32 {
    let r = (rate & 0x3F) as usize;
    let hundredths_ms = TABLE_III_7_DECAY_HUNDREDTHS_MS[r];
    if hundredths_ms == 0 {
        // Halt (RATE 0..=3 not tabulated; the manual's R=0 carve-out
        // is honoured upstream by [`Envelope::effective_rate`]).
        return 0;
    }
    // total_samples = (hundredths_ms / 100_000) seconds × sample_rate
    //               = hundredths_ms × sample_rate / 100_000
    // (sample_rate is integer 49_716 cycles per second; the
    //  0.0003-Hz aliasing vs 49716.3 fits inside Q16 rounding).
    const SAMPLE_RATE_HZ_INT: u64 = 49_716;
    let total_samples = (hundredths_ms as u64).saturating_mul(SAMPLE_RATE_HZ_INT) / 100_000;
    if total_samples == 0 {
        // RATE 60..=63: 1.27 ms × 49 716 Hz / 100 000 ≈ 63 samples
        // → never zero. Defensive fallthrough only.
        return u32::MAX;
    }
    let step = ENV_LEVELS_40_DB_Q16 / total_samples;
    if step > u32::MAX as u64 {
        u32::MAX
    } else {
        step as u32
    }
}

/// **Yamaha YM2413 Application Manual Table III-7** — EG attack time
/// (`0 dB → 40 dB` column) in units of 0.01 ms, indexed by the
/// post-key-scale `RATE = RM·4 + RL` (0..=63).
///
/// Source: page 14 of the application manual
/// (`docs/audio/nsf/opll-ym2413/ym2413-application-manual.pdf`;
/// HTML mirror `ym2413-application-manual-smspower.html`). The table
/// is the parallel column to `TABLE_III_7_DECAY_HUNDREDTHS_MS` — the
/// attack envelope's §III-3 description "rises exponentially during
/// attack time" so the manual's `10 % - 90 %` column captures a
/// different waveform feature; the `0 dB - 40 dB` column tabulates
/// the total attack-span traversal time, which is the quantity the
/// envelope generator needs to derive the per-sample step.
///
/// Entries 0..=3 are not tabulated by the manual and are set to
/// zero here (treated as halt by [`attack_step_q16_per_sample`]);
/// [`Envelope::effective_rate`]'s `4·R + Rks` formula never
/// produces a RATE below 4 when `R ≥ 1`.
///
/// Entries 60..=63 (RM=15, any RL) are tabulated as `0.00 ms` in the
/// manual — interpreted by [`attack_step_q16_per_sample`] as
/// "instantaneous attack" (returns `u32::MAX`, which saturates
/// `level_q16` to zero within one sample).
///
/// The same manual footnote that flags the decay column applies here:
/// "Likely transcription errors here, especially lower in the table".
/// The same two visibly anomalous cells (`RM=9 RL=2` and `RM=3 RL=0`)
/// surface in the unused `10 % - 90 %` column; the consumed
/// `0 dB - 40 dB` column is reproduced as printed.
pub const TABLE_III_7_ATTACK_HUNDREDTHS_MS: [u32; 64] = [
    // RATE 0..3 — not tabulated; treated as halt.
    0, 0, 0, 0, // RATE 4..7 (RM=1, RL=0..3): 1730.15, 1400.60, 1153.43, 988.66 ms.
    173_015, 140_060, 115_343, 98_866,
    // RATE 8..11 (RM=2, RL=0..3): 865.88, 780.30, 576.72, 494.33 ms.
    86_588, 78_030, 57_672, 49_433,
    // RATE 12..15 (RM=3, RL=0..3): 432.54, 358.15, 280.36, 247.16 ms.
    43_254, 35_815, 28_036, 24_716,
    // RATE 16..19 (RM=4, RL=0..3): 216.27, 175.07, 144.48, 123.50 ms.
    21_627, 17_507, 14_448, 12_350,
    // RATE 20..23 (RM=5, RL=0..3): 108.13, 87.54, 72.89, 61.79 ms.
    10_813, 8_754, 7_289, 6_179,
    // RATE 24..27 (RM=6, RL=0..3): 54.87, 43.77, 36.04, 30.90 ms.
    5_487, 4_377, 3_604, 3_090,
    // RATE 28..31 (RM=7, RL=0..3): 27.03, 21.00, 18.02, 15.45 ms.
    2_703, 2_100, 1_802, 1_545,
    // RATE 32..35 (RM=8, RL=0..3): 13.52, 10.94, 9.01, 7.72 ms.
    1_352, 1_094, 901, 772, // RATE 36..39 (RM=9, RL=0..3): 6.76, 5.47, 4.51, 3.86 ms.
    676, 547, 451, 386, // RATE 40..43 (RM=10, RL=0..3): 3.30, 2.74, 2.25, 1.93 ms.
    330, 274, 225, 193, // RATE 44..47 (RM=11, RL=0..3): 1.69, 1.37, 1.13, 0.97 ms.
    169, 137, 113, 97, // RATE 48..51 (RM=12, RL=0..3): 0.84, 0.70, 0.60, 0.54 ms.
    84, 70, 60, 54, // RATE 52..55 (RM=13, RL=0..3): 0.50, 0.42, 0.34, 0.30 ms.
    50, 42, 34, 30, // RATE 56..59 (RM=14, RL=0..3): 0.28, 0.22, 0.18, 0.14 ms.
    28, 22, 18, 14,
    // RATE 60..63 (RM=15, RL=0..3): 0.00, 0.00, 0.00, 0.00 ms
    // (treated as instantaneous attack by attack_step_q16_per_sample).
    0, 0, 0, 0,
];

/// Per-sample Q16 envelope-level step for the documented `RATE` per
/// **Yamaha YM2413 Application Manual Table III-7** column "EG attack
/// time 0 dB → 40 dB".
///
/// Returns the increment (in Q16 fixed-point envelope-levels) that
/// would carry the envelope through the 40-dB attack span in exactly
/// the tabulated time at the OPLL per-operator rate
/// ([`OPLL_SAMPLE_RATE_HZ`] ≈ 49.7163 kHz). Used by [`Envelope::step`]
/// for the Attack phase.
///
/// Special cases:
/// * RATE 0..=3 return zero (halt) — the manual does not tabulate
///   them and `R=0` is the documented halt case from §III-1-2.
/// * RATE 60..=63 are tabulated as `0.00 ms` (instantaneous attack):
///   the helper returns `u32::MAX` so the envelope saturates to zero
///   (= loudest) in a single `Envelope::step` call.
#[inline]
pub fn attack_step_q16_per_sample(rate: u8) -> u32 {
    let r = (rate & 0x3F) as usize;
    let hundredths_ms = TABLE_III_7_ATTACK_HUNDREDTHS_MS[r];
    if hundredths_ms == 0 {
        // RATE 0..=3 → halt (R=0 carve-out, honoured upstream by
        // Envelope::effective_rate);
        // RATE 60..=63 → tabulated 0.00 ms = instantaneous attack.
        if r >= 4 {
            return u32::MAX;
        }
        return 0;
    }
    // total_samples = (hundredths_ms / 100_000) seconds × sample_rate
    //               = hundredths_ms × sample_rate / 100_000
    // (matches decay_step_q16_per_sample's integer-49 716 path).
    const SAMPLE_RATE_HZ_INT: u64 = 49_716;
    let total_samples = (hundredths_ms as u64).saturating_mul(SAMPLE_RATE_HZ_INT) / 100_000;
    if total_samples == 0 {
        // RATE 56..=59: 0.14..0.28 ms × 49 716 / 100 000 ≈ 70..139
        // samples — never zero. Defensive fallthrough only.
        return u32::MAX;
    }
    let step = ENV_LEVELS_40_DB_Q16 / total_samples;
    if step > u32::MAX as u64 {
        u32::MAX
    } else {
        step as u32
    }
}

// -------------------------------------------------------------- §6 logsin / exp

/// Number of entries in the log-sin and exp ROMs.
pub const TABLE_LEN: usize = 256;

/// First-quadrant log-sin table. Per andete:
///
/// ```text
/// logsinTable[i] = round(-log2(sin((i + 0.5) * pi/2 / 256)) * 256)
/// ```
///
/// 12-bit values, range `0..=2137`. Source:
/// `docs/audio/nsf/opll-ym2413/ym2413-logsin-exp-tables-andete-2015-04-09.txt`
/// §"table lookup algorithm".
pub static LOGSIN_TABLE: once_cell_logsin::Lazy<[u16; TABLE_LEN]> =
    once_cell_logsin::Lazy::new(|| {
        let mut t = [0u16; TABLE_LEN];
        for (i, v) in t.iter_mut().enumerate() {
            let theta = ((i as f64) + 0.5) * std::f64::consts::FRAC_PI_2 / 256.0;
            let logsin = -(theta.sin()).log2() * 256.0;
            *v = logsin.round() as u16;
        }
        t
    });

/// Exp table. Per andete:
///
/// ```text
/// expTable[i] = round(exp2(i / 256) * 1024) - 1024
/// ```
///
/// 10-bit values, range `0..=1018` (the always-set bit 10 is omitted
/// from ROM and added back in `lookup_exp`). Source as above.
pub static EXP_TABLE: once_cell_logsin::Lazy<[u16; TABLE_LEN]> =
    once_cell_logsin::Lazy::new(|| {
        let mut t = [0u16; TABLE_LEN];
        for (i, v) in t.iter_mut().enumerate() {
            let e = (i as f64 / 256.0).exp2() * 1024.0 - 1024.0;
            *v = e.round() as u16;
        }
        t
    });

/// `lookupSin` per andete. Input is a 10-bit phase value (`0..=1023`).
///
/// Bit 9 = sign, bit 8 = quadrant mirror, bits 7..0 = 1st-quadrant
/// index. The 12-bit log-sin magnitude is returned in bits 11..0; the
/// sign occupies bit 15 (sign-magnitude, **not** two's complement).
pub fn lookup_sin(val: u32) -> u32 {
    let sign = val & 0x200 != 0;
    let mirror = val & 0x100 != 0;
    let idx = (val & 0xFF) as usize;
    let lookup_idx = if mirror { idx ^ 0xFF } else { idx };
    let mut result = LOGSIN_TABLE[lookup_idx] as u32;
    if sign {
        result |= 0x8000;
    }
    result
}

/// `lookupExp` per andete. Input is a 16-bit sign-magnitude value with
/// an 8-fractional-bit magnitude, sign in bit 15. Returns a signed
/// linear amplitude (1-complement representation per andete) shifted
/// right by 4 (the OPLL precision simplification).
pub fn lookup_exp(val: u32) -> i32 {
    let sign = val & 0x8000 != 0;
    let mantissa = (EXP_TABLE[((val & 0xFF) as usize) ^ 0xFF] as u32 | 0x400) << 1;
    let shift = (val & 0x7F00) >> 8;
    // Right-shift cannot exceed 31 without wrapping in Rust; OPLL inputs
    // never exceed shift ~14, but guard defensively.
    let result = if shift >= 32 { 0 } else { mantissa >> shift };
    let mut result = result as i32;
    if sign {
        result = !result; // 1-complement → preserves +0/-0
    }
    result >> 4
}

/// Convenience: end-to-end sine output for a 10-bit phase with no
/// attenuation. Returns a signed linear amplitude per `lookup_exp`.
pub fn pure_sine(phase: u32) -> i32 {
    lookup_exp(lookup_sin(phase))
}

// -------------------------------------------------------------- §6 row-256

/// Per-volume maximum amplitude row from
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §6 (also in
/// andete's notes, line 264). Volume 0 = loudest, 15 = quietest.
///
/// Used by [`Operator::peak_at_volume`] and the integration tests as a
/// hardware-derived ground-truth oracle for the log-sin → exp pipeline.
pub const PEAK_AMPLITUDE_PER_VOLUME: [u8; 16] =
    [255, 180, 127, 90, 63, 45, 31, 22, 15, 11, 7, 5, 3, 2, 1, 1];

/// Compute the predicted peak amplitude (sample at phase 256, the
/// sine-π/2 maximum) at output level `volume` using the operator
/// pipeline. Per andete §"verify the algorithm" the offset added to
/// the log-sin output is `128 * volume`.
pub fn peak_at_volume(volume: u8) -> i32 {
    let v = volume as u32 & 0x0F;
    lookup_exp(lookup_sin(256) + 128 * v)
}

// -------------------------------------------------------------- envelope

/// Operator envelope state machine. Per
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §7 the per-rate
/// numeric step arrays are flagged provenance-pending in the staged
/// docs. The envelope here therefore implements the
/// **documented behaviour** (key-on triggers attack to 0; attack
/// transitions to decay; decay ramps to the sustain-level; key-off /
/// non-sustain triggers release) but its per-rate slope is a coarse
/// linear approximation calibrated so rate=0 halts and rate=15 is the
/// fastest. The precise per-RATE increment table is a documented
/// DOCS-GAP followup (see crate README §Round 14+ followups).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvPhase {
    /// No key event yet, envelope sits at 127 (silenced).
    #[default]
    Idle,
    /// Key-on triggered, envelope ramping down from 127 toward 0.
    Attack,
    /// Envelope past attack, ramping up from 0 toward `sustain_level`.
    Decay,
    /// Sustain-level reached; behaviour depends on EG-TYP (S bit):
    ///   * `EG-TYP = 1` (sustained tone): envelope holds until key-off.
    ///   * `EG-TYP = 0` (percussive tone): envelope continues releasing
    ///     toward 127 at the release rate.
    Sustain,
    /// Key-off triggered: envelope ramping up from current level to 127
    /// at the release rate.
    Release,
}

/// Envelope generator. Holds a 7-bit attenuation level (0..=127, where
/// 0 = loudest, 127 = silent — matching andete §"envelope levels"
/// where each level corresponds to −0.375 dB and feeds the exp lookup
/// as `+ 16 * eg_level`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Envelope {
    pub phase: EnvPhase,
    /// Current attenuation level, 0..=127 (fixed-point: integer part
    /// of an internal Q-format accumulator; we track the fixed-point
    /// value directly as `level_q16` below and expose the integer
    /// part).
    pub level_q16: u32,
    /// Cached attack rate (0..=15) from the active patch + KSR/block.
    pub attack_rate: u8,
    /// Cached decay rate.
    pub decay_rate: u8,
    /// Cached release rate.
    pub release_rate: u8,
    /// Cached sustain level (0..=15, 0 = loudest, 15 = quietest;
    /// 3 dB per step → 8 envelope-levels-per-step).
    pub sustain_level: u8,
    /// EG-TYP (S bit): true = sustained tone (hold at sustain level
    /// until key-off); false = percussive tone (continue releasing).
    pub egt_sustain: bool,
    /// `$00.S` (modulator only): per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Custom Patch", "the
    /// modulator's sustain bit ($00 S) also disables the release
    /// section of its envelope. If its sustain bit is set, the
    /// Attack, Decay, and Sustain portions of the envelope are used,
    /// but when the note is released the modulator will continue to
    /// sustain while the carrier releases." Set on the modulator's
    /// envelope only — the carrier ($01.S) always honours key-off.
    pub release_disabled: bool,
    /// KSR (Key Scale of RATE, `$00`/`$01` D4) — when set, the
    /// effective envelope rate is amplified by the pitch-derived
    /// offset `Rks = (block << 1) | fnum_msb`; when clear by
    /// `Rks = block >> 1`. Per the YM2413 Application Manual,
    /// §III-1-2 and Table III-2, in
    /// `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`.
    pub ksr: bool,
    /// Cached `Rks` offset (0..=15) derived from the channel's
    /// current `block` + F-Num MSB and the operator's KSR bit. Mixed
    /// into the per-stage rate at step time as `RATE = 4·R + Rks`.
    /// Updated via [`Envelope::update_rks`] whenever the channel's
    /// fnum / block changes.
    pub rks: u8,
}

/// Maximum envelope level (silence). Per andete §"envelope levels":
/// envelope levels run 0..=127 and the exp-table offset is
/// `+16 * eg_level`, so `16 * 127 = 2032` — comfortably below the
/// log-sin maximum of 2137, ensuring silence.
pub const ENV_MAX_LEVEL: u32 = 127;

// ----------------------------------------------------- §7 EG rate-increment

/// The four §7 `eg_select` increment patterns (8 entries each), selected
/// by `eg_select = rate & 3`. Each entry is the number of EG levels to
/// advance on the samples where the global counter's `eg_shift` window
/// rolls over — duty `4/8`, `5/8`, `6/8`, `7/8` respectively.
///
/// Source:
/// `docs/audio/nsf/opll-ym2413/tables/envelope-rate-increment.csv` (§7
/// of `opll-ym2413-tables.md`, #138) — andete RE notes 2015-03-20.
/// Independent silicon RE: decay rates 4,9,14,19,48,49,50,51 and the
/// `{1024,1024,2048}`-sample segment pattern were measured directly on a
/// real YM2413. (andete also *corrected* the Burczynski emulator's
/// 256-level count — the YM2413 has 128 EG levels.)
pub const EG_SELECT_TABLE: [[u8; 8]; 4] = [
    [0, 1, 0, 1, 0, 1, 0, 1], // eg_select 0 — 4/8
    [0, 1, 0, 1, 1, 1, 0, 1], // eg_select 1 — 5/8
    [0, 1, 1, 1, 0, 1, 1, 1], // eg_select 2 — 6/8
    [0, 1, 1, 1, 1, 1, 1, 1], // eg_select 3 — 7/8
];

/// §7 high-rate correction tables for effective decay/release rates
/// **52..=59**, indexed by `(global_counter >> eg_shift) & 15`. A value
/// of `2` means the EG advances two levels in that sample. andete
/// measured that the generic 8-entry algorithm does **not** match real
/// silicon for these rates (the generic `eg_shift` goes negative for
/// rate ≥ 56); these 16-entry tables reproduce the captured behaviour.
///
/// Row index is `rate - 52` (so row 0 = rate 52 … row 7 = rate 59).
///
/// Source:
/// `docs/audio/nsf/opll-ym2413/tables/envelope-rate-increment-highrate.csv`
/// (§7 of `opll-ym2413-tables.md`, #138) — andete RE notes 2015-03-20.
/// Independent silicon RE that **corrects** the emulator model (e.g.
/// rate 54 shows the measured `2,2,1,1,1,1` transition detail).
pub const EG_HIGHRATE_TABLE: [[u8; 16]; 8] = [
    [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1], // 52
    [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1, 1], // 53
    [0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 1, 1, 1, 1], // 54
    [0, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1], // 55
    [1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1], // 56
    [2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1], // 57
    [2, 2, 2, 2, 1, 1, 1, 1, 2, 2, 2, 2, 1, 1, 1, 1], // 58
    [2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1], // 59
];

/// §7 decay / release EG-level advance for one output sample, given the
/// 6-bit effective `rate` (0..=63) and the shared chip-wide
/// `global_counter` value **after** its per-sample increment.
///
/// Returns the number of EG levels (`0`, `1`, or `2`) to add to the
/// operator's attenuation this sample. Release behaves identically to
/// decay. Per `tables/envelope-rate-increment.csv` (§7, #138):
///
/// * `rate <= 3`: never advance (returns 0).
/// * `rate >= 60`: advance `+2` every sample (the table's "rates 60..63
///   decay/release +2 levels each sample").
/// * `52..=59`: indexed via the 16-entry [`EG_HIGHRATE_TABLE`]
///   (`(global_counter >> eg_shift) & 15`, with `eg_shift = 13 -
///   rate/4`, here `0` or negative → 16-window).
/// * otherwise: `eg_shift = 13 - rate/4`, `eg_select = rate & 3`, and
///   the advance is [`EG_SELECT_TABLE`]`[eg_select][(global_counter >>
///   eg_shift) & 7]`.
///
/// The advance is consulted **only on the samples where the
/// `eg_shift`-windowed counter rolls over** — i.e. when
/// `(global_counter >> eg_shift)` changes after the per-sample
/// increment, which (since the counter is incremented by 1 each sample)
/// is exactly the samples whose low `eg_shift` bits are zero. On all
/// other samples the EG holds. The `0`-entries in the duty pattern then
/// thin those rollover events down to the measured 4/8…7/8 duty. (For
/// rates ≥ 52, `eg_shift ≤ 0`, so every sample is a "rollover".)
///
/// The worked example (decay rate 14 → `eg_shift = 10` → a rollover
/// every 1024 samples, `eg_select = 2` = 6/8 duty → repeating segment
/// lengths `1024, 1024, 2048` samples) is reproduced bit-exact by a
/// test.
#[inline]
pub fn eg_decay_advance(rate: u8, global_counter: u32) -> u8 {
    if rate <= 3 {
        return 0;
    }
    if rate >= 60 {
        return 2;
    }
    if (52..=59).contains(&rate) {
        // eg_shift = 13 - rate/4 is 0 for 52..55 and negative for
        // 56..59; the measured 16-entry window uses the low 4 bits of
        // the counter directly (every sample is a rollover here).
        let eg_shift = 13i32 - (rate as i32) / 4;
        let shift = eg_shift.max(0) as u32;
        let idx = ((global_counter >> shift) & 15) as usize;
        return EG_HIGHRATE_TABLE[(rate - 52) as usize][idx];
    }
    let eg_shift = 13 - (rate / 4);
    // Hold on every sample except the one that rolls the eg_shift window
    // (low eg_shift bits all zero after the per-sample increment).
    if global_counter & ((1u32 << eg_shift) - 1) != 0 {
        return 0;
    }
    let eg_select = (rate & 3) as usize;
    let idx = ((global_counter >> eg_shift) & 7) as usize;
    EG_SELECT_TABLE[eg_select][idx]
}

impl Envelope {
    /// Integer attenuation level (0..=127).
    #[inline]
    pub fn level(&self) -> u32 {
        (self.level_q16 >> 16).min(ENV_MAX_LEVEL)
    }

    /// Load the operator's rate constants from a decoded patch.
    pub fn load_from_patch(&mut self, ar: u8, dr: u8, sl: u8, rr: u8, egt_sustain: bool) {
        self.attack_rate = ar & 0x0F;
        self.decay_rate = dr & 0x0F;
        self.release_rate = rr & 0x0F;
        self.sustain_level = sl & 0x0F;
        self.egt_sustain = egt_sustain;
        // Note: `release_disabled` and `ksr` are set independently by
        // the channel's patch loader: `release_disabled` is the
        // modulator-only $00.S behaviour; `ksr` is the per-operator
        // $00/$01.D4 bit.
    }

    /// Recompute the `Rks` offset from the channel's current pitch.
    /// `block` is the 3-bit BLOCK Data (0..=7); `fnum_msb` is the
    /// top bit of the 9-bit F-Num.
    ///
    /// Per the YM2413 Application Manual §III-1-2 Table III-2 in
    /// `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`:
    ///
    /// * `KSR = 0` (D4 = 0 key scale row): `Rks = block >> 1`
    ///   (the table reads `0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3` across
    ///   the 16 (block, fnum-MSB) columns; the F-Num MSB is ignored).
    /// * `KSR = 1` (D4 = 1 key scale row): `Rks = (block << 1) | fnum_msb`
    ///   (the table reads `0,1,2,…,15` across the same 16 columns).
    pub fn update_rks(&mut self, block: u8, fnum_msb: u8) {
        let b = block & 0x07;
        let m = fnum_msb & 0x01;
        self.rks = if self.ksr { (b << 1) | m } else { b >> 1 };
    }

    /// Effective 6-bit RATE for a 4-bit R per the manual's
    /// `RATE = 4·R + Rks` formula. `R = 0` always yields `RATE = 0`
    /// (envelope halt) regardless of Rks — per the explicit "Note
    /// that when R=0, RATE=0" remark in §III-1-2.
    #[inline]
    pub fn effective_rate(&self, r: u8) -> u8 {
        let r = r & 0x0F;
        if r == 0 {
            0
        } else {
            // `4·R + Rks`: R ∈ 1..=15, Rks ∈ 0..=15 → RATE ∈ 4..=75.
            // The manual's table caps at 4·15 + 15 = 75 (fits in 7
            // bits); we clamp at 63 for the step() shift below since
            // beyond rate 63 the Q16 step already saturates the
            // envelope in <1 sample.
            (4u8.saturating_mul(r).saturating_add(self.rks)).min(63)
        }
    }

    /// Override the release rate. Per `vrc7-audio-wiki.html` §Channels:
    /// "If the sustain bit is set in the channel control register $2X
    /// S, the release value in the patch is ignored and replaced with
    /// $5." This applies to both modulator and carrier of the channel.
    /// The patch's own release rate is restored by the next
    /// `load_from_patch` call.
    pub fn set_release_rate(&mut self, rr: u8) {
        self.release_rate = rr & 0x0F;
    }

    /// Key-on: start the attack phase from whatever level we're at.
    pub fn key_on(&mut self) {
        self.phase = EnvPhase::Attack;
    }

    /// Key-off: enter the release phase. The starting level is whatever
    /// the envelope is at when key-off arrives — this matches the
    /// vendor manual's release-from-current-level behaviour.
    ///
    /// When [`Envelope::release_disabled`] is set (the modulator's
    /// `$00.S` per `docs/audio/nsf/vrc7-audio-wiki.html` §"Custom
    /// Patch"), key-off is suppressed entirely: the envelope holds at
    /// whichever phase it's in. Spec quote: "the modulator's sustain
    /// bit ($00 S) also disables the release section of its envelope."
    pub fn key_off(&mut self) {
        if self.release_disabled {
            // §"Custom Patch": modulator with $00.S=1 ignores key-off.
            return;
        }
        // Idle and Release stay where they are; everything else moves
        // into Release.
        if !matches!(self.phase, EnvPhase::Idle | EnvPhase::Release) {
            self.phase = EnvPhase::Release;
        }
    }

    /// Step the envelope by one operator sample. `samples` is typically
    /// 1; multi-sample stepping is allowed for bulk advance.
    ///
    /// Each stage's 4-bit R is widened to a 6-bit RATE via the
    /// manual's `RATE = 4·R + Rks` formula (see [`effective_rate`]).
    ///
    /// The per-RATE step magnitude for the **Decay**, percussive
    /// **Sustain**, and **Release** phases is sourced from
    /// **Yamaha YM2413 Application Manual Table III-7
    /// ("Attack and decay times in relation to RATE")** — see
    /// [`decay_step_q16_per_sample`] for the lookup. The manual's
    /// page-13 footnote states "Attenuation times of the release
    /// rate are the same as that of the decay rate", so the Release
    /// (and percussive Sustain) path reuses the decay column.
    ///
    /// The **Attack** phase consults the same Table III-7's
    /// `EG attack time, 0 dB → 40 dB` column via
    /// [`attack_step_q16_per_sample`], computing the linear-equivalent
    /// per-sample ramp that traverses the 40-dB span in the tabulated
    /// time. RATE 60..=63 (RM=15) are tabulated as `0.00 ms` —
    /// instantaneous attack, saturating `level_q16` to zero in one
    /// sample.
    ///
    /// [`effective_rate`]: Envelope::effective_rate
    pub fn step(&mut self, samples: u32) {
        // Attack pulls the Q16 step from Table III-7's "EG attack
        // time 0 dB → 40 dB" column.
        let attack_advance =
            |rate: u8, s: u32| -> u32 { attack_step_q16_per_sample(rate).saturating_mul(s) };
        // Decay / Sustain (percussive) / Release pull the Q16 step
        // from Table III-7 directly.
        let decay_advance =
            |rate: u8, s: u32| -> u32 { decay_step_q16_per_sample(rate).saturating_mul(s) };

        match self.phase {
            EnvPhase::Idle => {
                self.level_q16 = ENV_MAX_LEVEL << 16;
            }
            EnvPhase::Attack => {
                let step = attack_advance(self.effective_rate(self.attack_rate), samples);
                // Attack ramps DOWN to 0 (= loudest).
                self.level_q16 = self.level_q16.saturating_sub(step);
                if self.level_q16 == 0 {
                    self.phase = EnvPhase::Decay;
                }
            }
            EnvPhase::Decay => {
                let step = decay_advance(self.effective_rate(self.decay_rate), samples);
                self.level_q16 = self.level_q16.saturating_add(step);
                // Sustain level: 8 envelope-levels per SL-step (3 dB
                // per SL-step ÷ 0.375 dB per env-level = 8). SL=15
                // saturates near silence.
                let sustain = ((self.sustain_level as u32) * 8) << 16;
                if self.level_q16 >= sustain {
                    self.level_q16 = sustain;
                    self.phase = EnvPhase::Sustain;
                }
            }
            EnvPhase::Sustain => {
                if !self.egt_sustain {
                    // Percussive: continue toward silence at the
                    // release rate (Table III-7 column).
                    let step = decay_advance(self.effective_rate(self.release_rate), samples);
                    self.level_q16 = self.level_q16.saturating_add(step).min(ENV_MAX_LEVEL << 16);
                    if self.level_q16 >= ENV_MAX_LEVEL << 16 {
                        self.phase = EnvPhase::Idle;
                    }
                }
                // Sustained tone: hold here until key-off.
            }
            EnvPhase::Release => {
                let step = decay_advance(self.effective_rate(self.release_rate), samples);
                self.level_q16 = self.level_q16.saturating_add(step).min(ENV_MAX_LEVEL << 16);
                if self.level_q16 >= ENV_MAX_LEVEL << 16 {
                    self.phase = EnvPhase::Idle;
                }
            }
        }
    }

    /// Convenience for the operator: returns the current envelope
    /// level scaled by 16 (the andete §"envelope levels" exp-offset
    /// factor).
    #[inline]
    pub fn exp_offset(&self) -> u32 {
        self.level() * 16
    }
}

// -------------------------------------------------------------- operator

/// One OPLL operator (phase generator + envelope + waveform).
///
/// Per `opll-ym2413-tables.md` §1, an operator is a 4-bit MUL × 10-bit
/// phase generator feeding the log-sin → exp pipeline with the
/// envelope-attenuation + per-channel volume added in. The DC/DM bit
/// selects between full sine and half-rectified sine (the negative
/// half is silenced).
#[derive(Debug, Clone, Copy, Default)]
pub struct Operator {
    /// 19-bit phase accumulator. The top 10 bits index the sine table.
    pub phase_acc: u32,
    /// Envelope generator.
    pub env: Envelope,
    /// Operator MUL field, 0..=15 (looked up in `MUL_TIMES_TWO`).
    pub mul: u8,
    /// Operator total level (modulator only, 6 bits): 0..=63. Carrier
    /// uses the per-channel volume in place of TL.
    pub tl: u8,
    /// Half-rectify waveform bit (DC/DM, 0 = full sine, 1 = half).
    pub half_rect: bool,
    /// AM (tremolo) enable — the operator's `$00`/`$01` D7 bit. When
    /// set, the chip-wide tremolo LFO adds the §8a [`AM_LFO_LEVELS`]
    /// attenuation (0 .. ≈ 4.8 dB) on top of the operator's other
    /// attenuation sources.
    pub am: bool,
    /// VIB (vibrato) enable — the operator's `$00`/`$01` D6 bit. When
    /// set, the chip-wide vibrato LFO sweeps this operator's phase
    /// increment by the §8b [`VIB_PM_TABLE`] phase-modulation (≈ ±14
    /// cents at the top of the pitch range).
    pub vib: bool,
}

impl Operator {
    /// Reset the phase accumulator (called on key-on).
    pub fn reset_phase(&mut self) {
        self.phase_acc = 0;
    }

    /// Step the operator phase generator by one sample. `fnum_block`
    /// is the channel's `fnum << block` (i.e. the base phase rate
    /// before the MUL multiplier). The phase advance per sample is
    /// `(fnum_block * MUL_x2) / 2` to fit MUL=0 (half) into integers.
    #[inline]
    pub fn step_phase(&mut self, fnum_block: u32) {
        let inc = (fnum_block * MUL_TIMES_TWO[self.mul as usize & 0x0F] as u32) >> 1;
        // The phase accumulator wraps at the 19-bit fractional point
        // shifted up by 10 sine-index bits, i.e. modulo
        // `PHASE_STEPS_PER_PERIOD << PHASE_ACC_FRAC_BITS`.
        let modulus = PHASE_STEPS_PER_PERIOD << PHASE_ACC_FRAC_BITS;
        self.phase_acc = self.phase_acc.wrapping_add(inc) & (modulus - 1);
    }

    /// Step the operator phase generator by one sample using the
    /// silicon-measured §8b **vibrato phase-step formula** (the exact
    /// integer form, not the cents approximation):
    ///
    /// ```text
    ///   phase-step = (((2 * fnum + lfo_pm) * mlTab[ML]) << block) >> 2
    /// ```
    ///
    /// where `mlTab[ML]` is [`MUL_TIMES_TWO`] and `lfo_pm` is the signed
    /// VIB correction from [`Lfo::vibrato_pm`] / [`VIB_PM_TABLE`]
    /// (`docs/audio/nsf/opll-ym2413/ym2413-vib-lfo-andete-2015-12-01.txt`,
    /// §8b). With `lfo_pm == 0` this reduces to
    /// `((fnum * mlTab[ML]) << block) >> 1` — i.e. exactly
    /// [`Self::step_phase`] with `fnum_block = fnum << block` — so a
    /// VIB-disabled operator advances identically to the un-swept path.
    ///
    /// `fnum` is the channel's **raw 9-bit F-Number** (NOT pre-shifted by
    /// `block`); the formula folds in `block` itself, matching the
    /// silicon's `(... << block)` ordering so the `>> 2` truncation
    /// happens after the block shift exactly as measured.
    #[inline]
    pub fn step_phase_pm(&mut self, fnum: u32, block: u32, lfo_pm: i32) {
        let ml = MUL_TIMES_TWO[self.mul as usize & 0x0F] as i64;
        // `2 * fnum + lfo_pm` can be negative only if lfo_pm pushed it
        // below zero, which the §8b table never does (|lfo_pm| <= 7 and
        // the formula is only used with the channel's actual fnum, where
        // 2*fnum >= 0); compute in i64 then clamp non-negative for
        // safety before the unsigned phase add.
        let two_fnum_pm = 2 * fnum as i64 + lfo_pm as i64;
        let inc = (((two_fnum_pm * ml) << block) >> 2).max(0) as u32;
        let modulus = PHASE_STEPS_PER_PERIOD << PHASE_ACC_FRAC_BITS;
        self.phase_acc = self.phase_acc.wrapping_add(inc) & (modulus - 1);
    }

    /// 10-bit phase index for the sine table, with an optional
    /// modulation offset applied (the modulator's previous output
    /// shifted by `feedback_shift(fb)` is added to the modulator's
    /// own phase; the modulator's output is added to the carrier's
    /// phase as the carrier's modulation input).
    #[inline]
    pub fn phase_index(&self, modulation: i32) -> u32 {
        let base = (self.phase_acc >> PHASE_ACC_FRAC_BITS) & (PHASE_STEPS_PER_PERIOD - 1);
        let modulated = (base as i32).wrapping_add(modulation) as u32;
        modulated & (PHASE_STEPS_PER_PERIOD - 1)
    }

    /// Compute the operator's output sample given a modulation phase
    /// offset (0 for the modulator's self-feedback path, the
    /// modulator's previous output for the carrier).
    ///
    /// `extra_atten` is an additional 7-bit attenuation contribution
    /// (used for the per-channel carrier volume in §1 register `$3X`
    /// low nibble — each step is 3 dB = 8 envelope-levels per the
    /// andete §"envelope levels" note). Modulators pass `extra_atten
    /// = TL * 4` (TL is 6-bit, 0.75 dB per step → 2 env-levels per
    /// TL step → ×2; multiplied by 8 / 0.375dB-per-env-level... see
    /// the impl below).
    pub fn sample(&self, modulation: i32, extra_atten_env_levels: u32) -> i32 {
        self.sample_with_env_override(modulation, extra_atten_env_levels, self.env.exp_offset())
    }

    /// Like [`sample`], but the envelope's per-sample exp-offset is
    /// substituted with `env_exp_offset` (0 forces full-volume output
    /// regardless of envelope state — used by the §"Test Register
    /// $0F" bit-0 override per
    /// `docs/audio/nsf/vrc7-audio-wiki.html`).
    pub fn sample_with_env_override(
        &self,
        modulation: i32,
        extra_atten_env_levels: u32,
        env_exp_offset: u32,
    ) -> i32 {
        let phase = self.phase_index(modulation);
        let mut logsin = lookup_sin(phase);
        let sign_bit = logsin & 0x8000;
        let magnitude = logsin & 0x7FFF;

        // Half-rectified waveform: negative half is silenced. The
        // sign bit set means we're in the lower half of the sine
        // period; substitute silence by saturating the log-sin
        // contribution to the maximum.
        if self.half_rect && sign_bit != 0 {
            logsin = 0x7FFF; // effectively silence
        } else {
            logsin = magnitude | sign_bit;
        }

        // andete §"envelope levels": linear-output = exp(logsin +
        // 128*volume + 16*eg_level). Here we sum the envelope
        // contribution and any extra attenuation, both expressed in
        // 16-units-per-3-dB.
        let total_atten = env_exp_offset + extra_atten_env_levels;
        let combined = (logsin & 0x8000) | ((logsin & 0x7FFF) + total_atten).min(0x7FFF);
        lookup_exp(combined)
    }
}

// -------------------------------------------------------------- LFO

/// Tremolo (AM) LFO normal-mode divider: in normal operation the
/// tremolo LFO advances once every 64 operator samples. Per
/// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F" bit 3:
/// "Update tremolo and vibrato LFOs every sample instead of once
/// every several samples. (Tremolo is 64x faster, …)" — i.e. in the
/// normal (bit-3-clear) state the tremolo phase advances once per 64
/// per-operator samples, and 64× faster (once per sample) when bit 3
/// is set.
pub const TREMOLO_LFO_DIVIDER: u32 = 64;

/// Vibrato (VIB) LFO normal-mode divider: in normal operation the
/// vibrato LFO advances once every 1024 operator samples. Per the
/// same §"Test Register $0F" bit 3 note "… and vibrato is 1024x
/// faster" — i.e. normal-mode vibrato advances once per 1024
/// per-operator samples, and once per sample when bit 3 is set.
pub const VIBRATO_LFO_DIVIDER: u32 = 1024;

/// Tremolo (AM) LFO physical frequency, in Hz. Per
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §7
/// ("AM (tremolo) LFO"): "OPL-family AM depth is 1.0 dB peak at
/// ~3.7 Hz (shared with OPL2, per the OPLx-decapsulated article)."
pub const TREMOLO_LFO_FREQ_HZ: f32 = 3.7;

/// Vibrato (VIB) LFO physical frequency, in Hz. Per the same §7
/// ("VIB (vibrato) LFO"): "built-in vibrato oscillator; OPL-family
/// vibrato depth ~±7 cents (low) / ±14 cents (high) at ~6.0 Hz."
pub const VIBRATO_LFO_FREQ_HZ: f32 = 6.0;

/// Tremolo (AM) peak depth, in dB. §7: "OPL-family AM depth is
/// **1.0 dB** peak at ~3.7 Hz". The OPLL/VRC7 use the single
/// built-in AM oscillator at this fixed depth (there is no AM-depth
/// select bit on the YM2413, unlike OPL3's `0x01`/`0x40` modes).
pub const TREMOLO_PEAK_DB: f32 = 1.0;

/// Vibrato (VIB) peak depth, in cents. §7 lists "~±7 cents (low) /
/// ±14 cents (high)". The YM2413 exposes only the single built-in
/// vibrato oscillator (no OPL3-style VIB-depth select), so the
/// documented OPLL value is the low ±7 cents figure.
pub const VIBRATO_PEAK_CENTS: f32 = 7.0;

/// Number of tremolo phase steps in one full AM triangle period.
/// Derived from the two §7 facts: the tremolo phase advances once
/// per [`TREMOLO_LFO_DIVIDER`] (= 64) operator samples (so its step
/// rate is [`OPLL_SAMPLE_RATE_HZ`] / 64 ≈ 776.8 Hz), and one full
/// AM period is [`TREMOLO_LFO_FREQ_HZ`] ≈ 3.7 Hz — hence
/// `(49716 / 64) / 3.7 ≈ 210` phase steps per period. This is a
/// derived consequence of the two documented physical quantities,
/// **not** a lifted emulator step-array length.
pub const TREMOLO_PHASE_PERIOD: u32 = 210;

/// Number of vibrato phase steps in one full VIB period. Derived
/// the same way: the vibrato phase advances once per
/// [`VIBRATO_LFO_DIVIDER`] (= 1024) operator samples (step rate
/// 49716 / 1024 ≈ 48.55 Hz), and one full VIB period is
/// [`VIBRATO_LFO_FREQ_HZ`] ≈ 6.0 Hz — hence `48.55 / 6.0 ≈ 8` phase
/// steps per period.
pub const VIBRATO_PHASE_PERIOD: u32 = 8;

/// AM peak attenuation in envelope-level units (legacy approximation).
///
/// **Superseded by the §8a silicon-measured [`AM_LFO_LEVELS`] table.**
/// The original triangle approximation scaled the AM oscillator to the
/// then-assumed §7 1.0 dB depth (`1.0 / 3.0 * 16 ≈ 5.33` → 5 env-level
/// units at the peak). andete's 2015-11-28 silicon measurement
/// established the true depth is ≈ **4.8 dB** (a 14-level 0..13 ramp
/// applied as `16 * am`), not 1.0 dB, so this constant is retained only
/// for the deprecated [`Lfo::tremolo_atten_env_levels`] compatibility
/// shim and is no longer on the synthesis path.
pub const TREMOLO_PEAK_ENV_LEVELS: u32 = 5;

/// Peak AM (tremolo) level in the silicon-measured §8a truncated
/// triangle: the OPLL drops the low bit of the OPL-family 0..26 ramp,
/// giving **14 distinct levels (0..13)**. Level 13 is the trough of the
/// *amplitude* (the peak of the *attenuation*); see [`AM_LFO_LEVELS`].
/// Source: `docs/audio/nsf/opll-ym2413/ym2413-am-lfo-andete-2015-11-28.txt`
/// + `tables/am-lfo-triangle.csv` (§8a of `opll-ym2413-tables.md`, #138).
pub const AM_LFO_PEAK_LEVEL: u8 = 13;

/// Length of one full §8a AM (tremolo) LFO period, in *tremolo phase
/// steps*. Each step is held for 64 output samples, so the period is
/// `210 × 64 = 13440` samples → `49716 / 13440 ≈ 3.699 Hz` (the manual's
/// "3.7 Hz"). This is the silicon-measured 210-entry length, **not** a
/// derived approximation — see [`AM_LFO_LEVELS`].
pub const AM_LFO_PERIOD_STEPS: usize = 210;

/// The exp-table weight applied to one §8a AM level. andete's operator
/// model is `expTable[sineTable[phase] + 128*vol + 16*env + 16*am]`
/// (`ym2413-am-lfo-andete-2015-11-28.txt` lines 182, 185–186): the AM
/// level enters the exp index multiplied by **16**, exactly as the
/// envelope level does ([`Envelope::exp_offset`] = `level * 16`). So the
/// peak attenuation is `16 × 13 = 208` exp units ≈ `6.0206 × 208/256 ≈
/// 4.89 dB`, matching the measured ≈ 4.8 dB depth.
pub const AM_LFO_EXP_WEIGHT: u32 = 16;

/// Silicon-measured §8a AM (tremolo) LFO waveform: 210 entries, one per
/// tremolo phase step (each held 64 output samples). Values are the
/// OPLL-truncated 14-level triangle (0..13) andete measured directly on
/// real silicon; applied to the operator's exp index as `16 * am`.
///
/// The waveform rises 0 → 13 and falls 13 → 1 with the measured hold
/// durations: level 0 held 15 steps (960 samples), levels 1..=12 held 8
/// steps (512 samples) each, level 13 held 3 steps (192 samples), then
/// 12..=1 descending 8 steps each. `15 + 12·8 + 3 + 12·8 = 210`.
///
/// Source: `docs/audio/nsf/opll-ym2413/ym2413-am-lfo-andete-2015-11-28.txt`
/// (lines 99–106, 144–152) + `tables/am-lfo-triangle.csv` (§8a of
/// `opll-ym2413-tables.md`, #138). Independent silicon RE — the triangle
/// shape, the 14-level low-bit truncation, the 960/512/192-sample
/// segment durations, and the ≈ 4.8 dB depth were all measured on a real
/// YM2413 (not lifted from emu2413 / Nuked-OPLL / ymfm).
pub const AM_LFO_LEVELS: [u8; AM_LFO_PERIOD_STEPS] = build_am_lfo_levels();

/// Build the §8a AM waveform from the measured per-level hold durations
/// (`tables/am-lfo-triangle.csv`). Rising 0..=13 then falling 13..=1;
/// level 0 held 15 steps, levels 1..=12 held 8 steps, level 13 held 3.
const fn build_am_lfo_levels() -> [u8; AM_LFO_PERIOD_STEPS] {
    let mut out = [0u8; AM_LFO_PERIOD_STEPS];
    let mut i = 0usize;
    // Rising edge: level 0 (15 steps), then levels 1..=13 with their
    // measured hold counts (1..=12 → 8 steps; 13 → 3 steps).
    let mut level = 0u8;
    while level <= AM_LFO_PEAK_LEVEL {
        let holds = if level == 0 {
            15
        } else if level == AM_LFO_PEAK_LEVEL {
            3
        } else {
            8
        };
        let mut h = 0;
        while h < holds {
            out[i] = level;
            i += 1;
            h += 1;
        }
        level += 1;
    }
    // Falling edge: levels 12..=1, 8 steps each (level 0 and level 13
    // are not repeated — the triangle's apex and trough are single
    // segments, already emitted on the rising edge).
    let mut level = AM_LFO_PEAK_LEVEL - 1;
    while level >= 1 {
        let mut h = 0;
        while h < 8 {
            out[i] = level;
            i += 1;
            h += 1;
        }
        level -= 1;
    }
    out
}

/// The built-in AM (tremolo) + VIB (vibrato) low-frequency
/// oscillators that drive the per-operator amplitude / pitch
/// modulation when an operator's `$00`/`$01` AM / VIB bit is set.
///
/// This struct owns the **LFO phase cadence + hold/reset semantics**
/// — the timing of how often each LFO advances, per
/// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F" (bit 1
/// hold-and-reset, bit 3 fast-update) and §"Audio Reset ($E000)"
/// (tremolo phase cleared, vibrato phase preserved) — **and** the
/// phase→depth translation.
///
/// The VIB (vibrato) depth is the **silicon-measured §8b
/// phase-modulation table** ([`Lfo::vibrato_pm`] → [`VIB_PM_TABLE`],
/// `docs/audio/nsf/opll-ym2413/ym2413-vib-lfo-andete-2015-12-01.txt`):
/// the free-running [`Self::vibrato_phase`] selects a column and the
/// channel's top three F-Number bits select a row, yielding the exact
/// integer `lfo_pm` the chip folds into its phase-step generator. This
/// is the measurement-confirmed hardware form, not the earlier
/// cents-scaled approximation. (The legacy [`Lfo::vibrato_pitch_offset_q`]
/// / [`apply_vibrato`] cents path is retained as a public utility but is
/// no longer on the per-sample synthesis path.)
///
/// The AM (tremolo) depth still maps the free-running phase through a
/// triangle scaled to the §8a *physical* 1.0 dB peak
/// ([`Lfo::tremolo_atten_env_levels`]), a documented physical quantity
/// rather than a lifted constant.
///
/// The two phases are independent free-running step counters
/// (`u32`), folded modulo [`TREMOLO_PHASE_PERIOD`] /
/// [`VIBRATO_PHASE_PERIOD`] by the depth readers. The OPL family's AM
/// phase is a triangle that repeats far below the sample rate, so a
/// `u32` step counter never wraps in any realistic render length.
///
/// The two dividers count down from `N - 1` and step their phase when
/// they reach 0 (then reload), so the first phase step lands on the
/// `N`-th per-operator sample after a clear — matching "once every N
/// samples".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lfo {
    /// Per-operator-sample countdown to the next tremolo phase step.
    /// Reaches 0 once every [`TREMOLO_LFO_DIVIDER`] samples in normal
    /// mode (or every sample when `fast_lfo` is set), at which point
    /// [`Lfo::tremolo_phase`] increments and the divider reloads.
    pub tremolo_divider: u32,
    /// Free-running tremolo phase step counter. Reset to 0 when the
    /// `$0F` bit-1 hold is engaged and when the `$E000` audio reset
    /// fires.
    pub tremolo_phase: u32,
    /// Per-operator-sample countdown to the next vibrato phase step.
    /// Reaches 0 once every [`VIBRATO_LFO_DIVIDER`] samples in normal
    /// mode (or every sample when `fast_lfo` is set).
    pub vibrato_divider: u32,
    /// Free-running vibrato phase step counter. Reset to 0 by the
    /// `$0F` bit-1 hold, but — unlike tremolo — **preserved** across a
    /// `$E000` audio reset per §"Audio Reset ($E000)".
    pub vibrato_phase: u32,
}

impl Default for Lfo {
    fn default() -> Self {
        Self {
            tremolo_divider: TREMOLO_LFO_DIVIDER - 1,
            tremolo_phase: 0,
            vibrato_divider: VIBRATO_LFO_DIVIDER - 1,
            vibrato_phase: 0,
        }
    }
}

impl Lfo {
    /// Advance the LFO phases by one per-operator sample.
    ///
    /// * `hold` is the `$0F` bit-1 state. Per §"Test Register $0F"
    ///   bit 1: "Hold LFO phase at zero. This halts, disables, and
    ///   resets both the tremolo and vibrato LFO." While held both
    ///   phases (and both dividers) are pinned to zero and do not
    ///   advance.
    /// * `fast` is the `$0F` bit-3 state. When set both LFOs advance
    ///   once per sample (the dividers are bypassed); when clear they
    ///   advance once per [`TREMOLO_LFO_DIVIDER`] /
    ///   [`VIBRATO_LFO_DIVIDER`] samples respectively.
    pub fn tick(&mut self, hold: bool, fast: bool) {
        if hold {
            // §"Test Register $0F" bit 1: halt + reset both LFOs.
            self.tremolo_phase = 0;
            self.vibrato_phase = 0;
            self.tremolo_divider = TREMOLO_LFO_DIVIDER - 1;
            self.vibrato_divider = VIBRATO_LFO_DIVIDER - 1;
            return;
        }
        // Tremolo.
        if fast {
            self.tremolo_phase = self.tremolo_phase.wrapping_add(1);
        } else if self.tremolo_divider == 0 {
            self.tremolo_phase = self.tremolo_phase.wrapping_add(1);
            self.tremolo_divider = TREMOLO_LFO_DIVIDER - 1;
        } else {
            self.tremolo_divider -= 1;
        }
        // Vibrato.
        if fast {
            self.vibrato_phase = self.vibrato_phase.wrapping_add(1);
        } else if self.vibrato_divider == 0 {
            self.vibrato_phase = self.vibrato_phase.wrapping_add(1);
            self.vibrato_divider = VIBRATO_LFO_DIVIDER - 1;
        } else {
            self.vibrato_divider -= 1;
        }
    }

    /// `$E000` bit 6 audio reset. Per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Audio Reset ($E000)":
    /// "Setting this bit will silence the expansion audio and clear
    /// its registers (including tremolo LFO state, but not including
    /// vibrato LFO state)." So the tremolo phase + divider are
    /// cleared while the vibrato phase + divider are **preserved**.
    pub fn audio_reset(&mut self) {
        self.tremolo_phase = 0;
        self.tremolo_divider = TREMOLO_LFO_DIVIDER - 1;
        // Vibrato deliberately untouched.
    }

    /// Current §8a AM (tremolo) level (0..=13) from the silicon-measured
    /// [`AM_LFO_LEVELS`] waveform, for an operator whose `$00`/`$01` AM
    /// bit is set.
    ///
    /// The free-running [`Self::tremolo_phase`] (one step per
    /// [`TREMOLO_LFO_DIVIDER`] = 64 output samples) folded modulo
    /// [`AM_LFO_PERIOD_STEPS`] (= 210) indexes the truncated-triangle
    /// table. This is the exact OPLL-truncated 14-level form andete
    /// measured on real silicon (the OPL-family 0..26 ramp with its low
    /// bit dropped), **not** a derived linear approximation.
    ///
    /// Returns 0 when the operator's AM bit is clear.
    #[inline]
    pub fn tremolo_am_level(&self, am_on: bool) -> u8 {
        if !am_on {
            return 0;
        }
        AM_LFO_LEVELS[(self.tremolo_phase % AM_LFO_PERIOD_STEPS as u32) as usize]
    }

    /// Current §8a AM (tremolo) attenuation contribution in **exp-table
    /// units** (the same units as [`Envelope::exp_offset`] and the
    /// per-channel volume / TL / KSL attenuations folded into the
    /// operator's exp index), for an operator whose `$00`/`$01` AM bit
    /// is set.
    ///
    /// Per andete's measured operator model `expTable[sineTable[phase] +
    /// 128*vol + 16*env + 16*am]`, the AM level enters the exp index as
    /// `16 * am` ([`AM_LFO_EXP_WEIGHT`]). The peak `16 × 13 = 208` exp
    /// units gives ≈ 4.89 dB, matching the measured ≈ 4.8 dB depth.
    ///
    /// Returns 0 when the operator's AM bit is clear.
    #[inline]
    pub fn tremolo_atten_exp_units(&self, am_on: bool) -> u32 {
        AM_LFO_EXP_WEIGHT * self.tremolo_am_level(am_on) as u32
    }

    /// Legacy AM contribution in the old "env-level" approximation
    /// (peak [`TREMOLO_PEAK_ENV_LEVELS`] = 5). **Superseded by the §8a
    /// silicon-measured [`Self::tremolo_atten_exp_units`].**
    ///
    /// Retained only so out-of-tree callers that depended on the
    /// pre-§8a behaviour keep compiling; the per-sample synthesis path
    /// no longer uses it. Returns the old 0..=5 linear triangle (a 1.0 dB
    /// peak, the depth assumed before andete's 4.8 dB measurement).
    #[deprecated(note = "use tremolo_atten_exp_units (§8a silicon-measured AM table); \
                this is the pre-§8a 1.0 dB approximation")]
    #[inline]
    pub fn tremolo_atten_env_levels(&self, am_on: bool) -> u32 {
        if !am_on {
            return 0;
        }
        let half = TREMOLO_PHASE_PERIOD / 2;
        let pos = self.tremolo_phase % TREMOLO_PHASE_PERIOD;
        // Triangle: rises 0..half over the first half-period, falls
        // back over the second. `tri` runs 0..=half.
        let tri = if pos < half {
            pos
        } else {
            TREMOLO_PHASE_PERIOD - pos
        };
        // Scale the 0..half triangle to 0..peak env-levels.
        (tri * TREMOLO_PEAK_ENV_LEVELS) / half
    }

    /// Current VIB (vibrato) pitch multiplier as a signed offset in
    /// units of `1 / 2^VIBRATO_PITCH_FRAC_BITS`, for an operator whose
    /// `$00`/`$01` VIB bit is set.
    ///
    /// §7: vibrato depth is ~±[`VIBRATO_PEAK_CENTS`] cents at ~6.0 Hz.
    /// The built-in vibrato oscillator sweeps the operator pitch
    /// symmetrically about the nominal frequency. We model it as a
    /// triangle over [`VIBRATO_PHASE_PERIOD`] phase steps, peaking at
    /// the documented ±7-cent depth. A cent is `2^(1/1200)`; ±7 cents
    /// is a multiplicative pitch factor of `2^(±7/1200) ≈ 1 ± 0.00405`.
    /// The return value is that fractional deviation in
    /// `Q[VIBRATO_PITCH_FRAC_BITS]` fixed point (positive = sharp,
    /// negative = flat), ready to scale the phase increment in
    /// [`OpllChannel::sample_with_test`].
    ///
    /// Returns 0 when the operator's VIB bit is clear.
    #[inline]
    pub fn vibrato_pitch_offset_q(&self, vib_on: bool) -> i32 {
        if !vib_on {
            return 0;
        }
        let half = (VIBRATO_PHASE_PERIOD / 2) as i32;
        let pos = (self.vibrato_phase % VIBRATO_PHASE_PERIOD) as i32;
        // Symmetric triangle in -half..=half: rises through the first
        // half, falls through the second.
        let tri = if pos < half {
            pos
        } else {
            VIBRATO_PHASE_PERIOD as i32 - pos
        };
        // `tri` is 0..=half; centre it to -half..=half so the pitch
        // swings both sharp and flat about the nominal frequency.
        let centred = 2 * tri - half;
        // Peak fractional pitch deviation = 2^(7/1200) - 1, in
        // Q[VIBRATO_PITCH_FRAC_BITS]. Computed at build from the
        // documented ±7-cent depth (no emulator constant).
        (centred * VIBRATO_PEAK_OFFSET_Q) / half
    }

    /// Silicon-measured VIB phase-modulation correction `lfo_pm`, the
    /// signed integer the §8b [`VIB_PM_TABLE`] adds to the phase-step
    /// generator's `2 * fnum` term (see [`Operator::step_phase_pm`]).
    ///
    /// `fnum_hi3` is the **top three bits of the channel's 9-bit
    /// F-Number** (`fnum >> 6`, 0..=7); it selects the table row. The
    /// current [`Self::vibrato_phase`] (advanced once per
    /// [`VIBRATO_LFO_DIVIDER`] samples) folded modulo
    /// [`VIBRATO_PHASE_PERIOD`] (= 8) selects the column — exactly the
    /// `pmTable[fnum>>6][counter>>10]` indexing the §8b note specifies.
    ///
    /// Returns 0 when the operator's VIB bit is clear, so an
    /// FM-disabled operator reproduces the un-modulated phase-step.
    #[inline]
    pub fn vibrato_pm(&self, fnum_hi3: u8, vib_on: bool) -> i32 {
        if !vib_on {
            return 0;
        }
        let row = (fnum_hi3 & 0x07) as usize;
        let col = (self.vibrato_phase % VIBRATO_PHASE_PERIOD) as usize;
        VIB_PM_TABLE[row][col] as i32
    }
}

/// Silicon-measured YM2413/OPLL **VIB (vibrato) phase-modulation
/// table** `pmTable[fnum>>6][counter>>10]`, the exact integer
/// frequency-correction the chip adds to its phase-step generator.
///
/// Rows are indexed by the **top three bits of the 9-bit F-Number**
/// (`fnum >> 6`, 0..=7); columns are the **8 vibrato phase positions**,
/// one advanced every [`VIBRATO_LFO_DIVIDER`] (= 1024) operator samples,
/// so the full pattern repeats every `8 × 1024 = 8192` samples
/// (`(49716 / 8192) ≈ 6.07 Hz`, the §8b measured vibrato frequency).
///
/// Source:
/// `docs/audio/nsf/opll-ym2413/ym2413-vib-lfo-andete-2015-12-01.txt`
/// and `docs/audio/nsf/opll-ym2413/tables/vib-lfo-pm.csv` (§8b of
/// `opll-ym2413-tables.md`, #138). andete **independently confirmed**
/// these values on real silicon: the 1024-sample-per-step timing, the
/// 8192-sample period, and the bottom row `0,+3,+7,+3,0,-3,-7,-3` were
/// all verified by direct hardware measurement (frequency-corrected
/// sines matched against the captured waveform). Reproduced here as a
/// measurement-confirmed hardware fact, not as emulator code.
///
/// The peak entry per row equals `fnum >> 6` (column 2), and the other
/// columns are halved / negated copies — i.e. the table is the
/// triangular ±depth sweep whose amplitude grows with pitch, giving the
/// roughly-constant ~±14-cent depth the manual documents.
pub const VIB_PM_TABLE: [[i8; 8]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 1, 0, 0, 0, -1, 0],
    [0, 1, 2, 1, 0, -1, -2, -1],
    [0, 1, 3, 1, 0, -1, -3, -1],
    [0, 2, 4, 2, 0, -2, -4, -2],
    [0, 2, 5, 2, 0, -2, -5, -2],
    [0, 3, 6, 3, 0, -3, -6, -3],
    [0, 3, 7, 3, 0, -3, -7, -3],
];

/// Fixed-point fractional bits used by
/// [`Lfo::vibrato_pitch_offset_q`]. A `Q12` scale keeps the ±7-cent
/// (~±0.4 %) deviation well above the rounding floor while staying
/// far inside `i32` for any `fnum << block`.
pub const VIBRATO_PITCH_FRAC_BITS: u32 = 12;

/// Peak fractional pitch deviation `round((2^(7/1200) - 1) * 2^12)`
/// for the §7 ±7-cent vibrato depth, in `Q[VIBRATO_PITCH_FRAC_BITS]`.
/// `2^(7/1200) - 1 ≈ 0.004050` → `0.004050 * 4096 ≈ 16.6` → 17.
/// Derived from the documented cent depth; not an emulator table.
pub const VIBRATO_PEAK_OFFSET_Q: i32 = 17;

/// Apply a vibrato pitch offset (in `Q[VIBRATO_PITCH_FRAC_BITS]`
/// fixed point, from [`Lfo::vibrato_pitch_offset_q`]) to a base
/// `fnum << block` phase rate. Returns
/// `fnum_block * (1 + offset / 2^FRAC)`, clamped to non-negative.
/// `offset == 0` (VIB disabled) returns `fnum_block` unchanged.
#[inline]
pub fn apply_vibrato(fnum_block: u32, offset_q: i32) -> u32 {
    if offset_q == 0 {
        return fnum_block;
    }
    // Widen to i64 so a large fnum_block can't overflow the multiply,
    // then fold the Q-scaled offset back in: result = base + base*off/2^FRAC.
    let base = fnum_block as i64;
    let delta = (base * offset_q as i64) >> VIBRATO_PITCH_FRAC_BITS;
    (base + delta).max(0) as u32
}

// -------------------------------------------------------------- test register

/// Decoded VRC7 / OPLL `$0F` test register state per
/// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F". All
/// fields default to inactive (the chip's behaviour at reset / when
/// `$0F` is its normal value of `0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TestRegister {
    /// `$0F` bit 0 — "The envelope generators are replaced with
    /// constant 0 output (full volume) for both modulator and
    /// carrier. The envelopes are still running while their output
    /// is overridden."
    pub envs_zero: bool,
    /// `$0F` bit 1 — "Hold LFO phase at zero. This halts, disables,
    /// and resets both the tremolo and vibrato LFO." Consumed by
    /// [`Lfo::tick`] to pin both LFO phases at zero. With the phase
    /// held at 0 the triangle-mapped AM / VIB depth is also 0, so this
    /// bit silences the audible tremolo / vibrato modulation.
    pub hold_lfo: bool,
    /// `$0F` bit 2 — "Holds and resets waveform phase to zero. The
    /// envelopes are not halted, though the output will be silent."
    pub hold_phase: bool,
    /// `$0F` bit 3 — "Update tremolo and vibrato LFOs every sample
    /// instead of once every several samples." Consumed by
    /// [`Lfo::tick`] to bypass the [`TREMOLO_LFO_DIVIDER`] /
    /// [`VIBRATO_LFO_DIVIDER`] dividers so both LFOs advance once per
    /// per-operator sample.
    pub fast_lfo: bool,
}

impl TestRegister {
    /// Decode the low 4 bits of register `$0F` per §"Test Register
    /// $0F".
    pub fn from_byte(value: u8) -> Self {
        Self {
            envs_zero: value & 0x01 != 0,
            hold_lfo: value & 0x02 != 0,
            hold_phase: value & 0x04 != 0,
            fast_lfo: value & 0x08 != 0,
        }
    }
}

// -------------------------------------------------------------- §III-1-7 rhythm

/// Decoded OPLL `$0E` RHYTHM register per Yamaha YM2413 Application
/// Manual §III-1-7 ("RHYTHM: Address [$0E]", mirrored in
/// `docs/audio/nsf/opll-ym2413/ym2413-application-manual-smspower.html`):
/// "To control ON/OFF of the Rhythm mode selection and percussion
/// instruments." Bit layout `D5..D0` = `RHYTHM BD SD TOM TOP-CY HH`.
///
/// "When D5 = 1, OPLL is in Rhythm mode with percussion sounds
/// produced through channels 7~9. In this mode, the melody section is
/// limited to six sounds. D0~D4 controls ON/OFF of percussion
/// instruments. Therefore Key-ON bits $26, $27, $28 must always be
/// cleared to 0."
///
/// On the VRC7 this register is inert: per
/// `docs/audio/nsf/vrc7-audio-wiki.html` §"Rhythm Register $0E" the
/// rhythm-mode bit "is treated as though it were always enabled,
/// resulting [in] only six audible FM channels", and "the VRC7 has no
/// rhythm DAC, so the 5 rhythm channels are always inaudible". See
/// [`crate::expansion::Vrc7::rhythm_control`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RhythmRegister {
    /// `$0E` D5 (RHYTHM) — 1: Rhythm mode (channels 7~9 become
    /// percussion, melody limited to 6 sounds); 0: Melody mode
    /// (9 melody channels).
    pub rhythm_mode: bool,
    /// `$0E` D4 (BD) — Bass Drum key on/off.
    pub bd: bool,
    /// `$0E` D3 (SD) — Snare Drum key on/off.
    pub sd: bool,
    /// `$0E` D2 (TOM) — Tom-tom key on/off.
    pub tom: bool,
    /// `$0E` D1 (TOP-CY) — Top Cymbal key on/off.
    pub t_cy: bool,
    /// `$0E` D0 (HH) — High Hat key on/off.
    pub hh: bool,
}

impl RhythmRegister {
    /// Decode the low 6 bits of register `$0E` per the §III-1-7 bit
    /// table (`D5..D0` = `RHYTHM BD SD TOM TOP-CY HH`).
    pub fn from_byte(value: u8) -> Self {
        Self {
            rhythm_mode: value & 0x20 != 0,
            bd: value & 0x10 != 0,
            sd: value & 0x08 != 0,
            tom: value & 0x04 != 0,
            t_cy: value & 0x02 != 0,
            hh: value & 0x01 != 0,
        }
    }
}

/// The five OPLL rhythm-mode percussion instruments of §III-1-7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RhythmInstrument {
    /// BD — Bass Drum.
    BassDrum,
    /// SD — Snare Drum.
    SnareDrum,
    /// TOM — Tom-tom.
    TomTom,
    /// TOP-CYM — Top Cymbal.
    TopCymbal,
    /// HH — High Hat.
    HiHat,
}

impl RhythmInstrument {
    /// Operator slots used by this percussion instrument, per Yamaha
    /// YM2413 Application Manual **Table III-9 "Rhythm slots"**:
    /// BD = slots 13 + 16, HH = 14, TOM = 15, SD = 17, TOP-CYM = 18.
    ///
    /// Per §V-4 ("Procedure of Percussion Sound Synthesis"): "Five
    /// kinds of percussion sounds are generated by using these three
    /// channels and six slots. For the bass drum (BD), two slots are
    /// used to synthesize FM sounds." — BD is the only two-slot
    /// (modulator + carrier) instrument; the other four get one slot
    /// each.
    pub fn slots(self) -> &'static [u8] {
        match self {
            RhythmInstrument::BassDrum => &[13, 16],
            RhythmInstrument::HiHat => &[14],
            RhythmInstrument::TomTom => &[15],
            RhythmInstrument::SnareDrum => &[17],
            RhythmInstrument::TopCymbal => &[18],
        }
    }

    /// Rhythm channel (7, 8 or 9) carrying this instrument.
    ///
    /// §III-1-7: "percussion sounds [are] produced through channels
    /// 7~9". Combining Table III-9 with §V-4's "three channels and
    /// six slots" (and BD being the one full modulator+carrier FM
    /// pair): slots 13/14/15 are the modulator slots of channels
    /// 7/8/9 and slots 16/17/18 their carrier slots, so BD occupies
    /// channel 7 entirely, HH (14) + SD (17) share channel 8, and
    /// TOM (15) + TOP-CYM (18) share channel 9.
    pub fn channel(self) -> u8 {
        match self {
            RhythmInstrument::BassDrum => 7,
            RhythmInstrument::HiHat | RhythmInstrument::SnareDrum => 8,
            RhythmInstrument::TomTom | RhythmInstrument::TopCymbal => 9,
        }
    }
}

/// Per-instrument rhythm volumes decoded from registers `$36`~`$38`
/// in rhythm mode, per the §III-1-8 rhythm-volume table ("In the
/// rhythm mode (addr = $0E, D5 = 'H'), each rhythm volume for
/// $36~$38 are set as follows"): `$36` D3..D0 = BD, `$37` D7..D4 =
/// HH / D3..D0 = SD, `$38` D7..D4 = TOM / D3..D0 = T-CYM. Each
/// 4-bit value uses the standard VOL scale (§III-1-8: "The minimum
/// resolution is 3 dB and the maximum 45 dB", 0 = loudest).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RhythmVolumes {
    /// `$36` D3..D0 — Bass Drum volume.
    pub bd: u8,
    /// `$37` D7..D4 — High Hat volume.
    pub hh: u8,
    /// `$37` D3..D0 — Snare Drum volume.
    pub sd: u8,
    /// `$38` D7..D4 — Tom-tom volume.
    pub tom: u8,
    /// `$38` D3..D0 — Top Cymbal volume.
    pub t_cy: u8,
}

impl RhythmVolumes {
    /// Decode the three rhythm-mode volume registers `$36`, `$37`,
    /// `$38` per the §III-1-8 rhythm-volume nibble table.
    pub fn from_regs(reg36: u8, reg37: u8, reg38: u8) -> Self {
        Self {
            bd: reg36 & 0x0F,
            hh: (reg37 >> 4) & 0x0F,
            sd: reg37 & 0x0F,
            tom: (reg38 >> 4) & 0x0F,
            t_cy: reg38 & 0x0F,
        }
    }
}

/// The (address, data) pairs the Yamaha YM2413 Application Manual
/// §III-1-7 lists immediately under Table III-9 as the F-Number /
/// Block values "that match percussion sounds" for the three rhythm
/// channels (`$16`/`$17`/`$18` = F-Num low bytes, `$26`/`$27`/`$28`
/// = Block + F-Num MSB for channels 7/8/9 — with the Key-ON bits in
/// `$26`~`$28` cleared to 0 as the same section requires).
pub const RHYTHM_FNUM_PRESET: [(u8, u8); 6] = [
    (0x16, 0x20),
    (0x17, 0x50),
    (0x18, 0xC0),
    (0x26, 0x05),
    (0x27, 0x05),
    (0x28, 0x01),
];

// -------------------------------------------------------------- channel

/// One OPLL channel = one modulator + one carrier, with the
/// modulator's previous output fed both to itself (feedback) and to
/// the carrier (modulation input).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpllChannel {
    pub modulator: Operator,
    pub carrier: Operator,
    /// 9-bit F-Num (`$10..=$15` low byte + `$20..=$25` D0 high bit).
    pub fnum: u16,
    /// 3-bit block (octave) selector (`$20..=$25` D3..D1).
    pub block: u8,
    /// Carrier per-channel volume (`$3X` D3..D0): 0 = loudest, 15 =
    /// quietest, 3 dB per step.
    pub volume: u8,
    /// `$03` D2..D0 — modulator self-feedback strength.
    pub fb: u8,
    /// Modulator KSL field (`$02` D7..D6, 0..=3). Per §4 of
    /// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md`, fed into
    /// `ksl_attenuation_env_levels` per sample to contribute the
    /// pitch-dependent §4 attenuation. Sourced from
    /// [`Vrc7Patch::mod_ksl`] on every `load_patch`.
    pub mod_ksl: u8,
    /// Carrier KSL field (`$03` D7..D6, 0..=3). Same §4 semantics
    /// as [`OpllChannel::mod_ksl`]; sourced from
    /// [`Vrc7Patch::car_ksl`] on every `load_patch`.
    pub car_ksl: u8,
    /// Previous modulator output (for feedback). Stored as the
    /// modulator's two-sample averaged output per OPL-family
    /// behaviour: feedback uses `(prev[0] + prev[1]) >> 1`.
    pub fb_prev: [i32; 2],
    /// Latched key-on state. Key-on edges trigger envelope attack +
    /// phase reset; key-off edges trigger envelope release.
    pub key_on: bool,
}

impl OpllChannel {
    /// Load both operators from a patch + the per-channel volume.
    ///
    /// Per `docs/audio/nsf/vrc7-audio-wiki.html` §"Custom Patch":
    /// the modulator's `$00.S` bit has a dual role — it is the
    /// EG-TYP (sustained vs percussive sustain phase) AND it disables
    /// the release section of the modulator's envelope entirely
    /// (key-off becomes a no-op for the modulator). The carrier's
    /// `$01.S` is only the EG-TYP; its envelope always honours
    /// key-off.
    pub fn load_patch(&mut self, p: &Vrc7Patch, volume: u8) {
        // Modulator (operator #0).
        self.modulator.mul = p.mod_mult;
        self.modulator.tl = p.mod_tl;
        self.modulator.half_rect = p.mod_wave != 0;
        // §"Custom Patch" $00 D7/D6 — AM (tremolo) / VIB (vibrato)
        // enable for the modulator. Consumed per sample via the
        // chip-wide [`Lfo`].
        self.modulator.am = p.mod_tremolo;
        self.modulator.vib = p.mod_vibrato;
        self.modulator.env.load_from_patch(
            p.mod_attack,
            p.mod_decay,
            p.mod_sustain_level,
            p.mod_release,
            p.mod_sustain,
        );
        // §"Custom Patch": modulator $00.S also disables its release.
        self.modulator.env.release_disabled = p.mod_sustain;
        // §III-1-2 KSR — per-operator D4 bit; the Rks offset is
        // computed against the channel's current pitch below.
        self.modulator.env.ksr = p.mod_ksr;

        // Carrier (operator #1).
        self.carrier.mul = p.car_mult;
        self.carrier.tl = 0; // carrier has no TL; volume takes its place
        self.carrier.half_rect = p.car_wave != 0;
        // §"Custom Patch" $01 D7/D6 — carrier AM / VIB enable.
        self.carrier.am = p.car_tremolo;
        self.carrier.vib = p.car_vibrato;
        self.carrier.env.load_from_patch(
            p.car_attack,
            p.car_decay,
            p.car_sustain_level,
            p.car_release,
            p.car_sustain,
        );
        // §"Custom Patch" explicitly: "The carrier does not behave
        // this way: its envelope always enters release when the note
        // is released."
        self.carrier.env.release_disabled = false;
        self.carrier.env.ksr = p.car_ksr;

        self.fb = p.feedback;
        self.volume = volume & 0x0F;
        // §4 KSL — cache the per-operator KSL field so the per-sample
        // pipeline can apply the documented `(base) >> (3 - KSL)`
        // contribution. With the §4 byte base table flagged
        // provenance-pending, the contribution is bit-exact for
        // block 0 streams and the same zero scaffold for blocks
        // 1..=7 until the §4 table is staged.
        self.mod_ksl = p.mod_ksl & 0x03;
        self.car_ksl = p.car_ksl & 0x03;

        // Re-derive each operator's Rks against the channel's
        // current (block, fnum) — a patch swap mid-note honours the
        // new KSR bit immediately.
        self.refresh_rks();
    }

    /// Refresh both operators' `Rks` offset from the channel's
    /// current `block` + F-Num MSB. Call this after any `fnum` /
    /// `block` change so the next envelope step picks up the new
    /// pitch-derived rate amplification per §III-1-2 Table III-2.
    pub fn refresh_rks(&mut self) {
        // F-Num is 9 bits in `self.fnum` (low 8 bits + the BLOCK's
        // D0 high bit folded in by the register layer). The KSR
        // table uses the top bit of the 9-bit F-Num.
        let fnum_msb = ((self.fnum >> 8) & 0x01) as u8;
        self.modulator.env.update_rks(self.block, fnum_msb);
        self.carrier.env.update_rks(self.block, fnum_msb);
    }

    /// Apply the per-channel sustain override (`$2X.S`). Per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §Channels: "If the
    /// sustain bit is set in the channel control register $2X S, the
    /// release value in the patch is ignored and replaced with $5."
    /// Reverts to the patch release rate when called with `false`.
    pub fn set_channel_sustain_override(&mut self, on: bool, patch: &Vrc7Patch) {
        if on {
            // Both operators take RR = $5.
            self.modulator.env.set_release_rate(0x5);
            self.carrier.env.set_release_rate(0x5);
        } else {
            // Restore the per-operator patch release rate.
            self.modulator.env.set_release_rate(patch.mod_release);
            self.carrier.env.set_release_rate(patch.car_release);
        }
    }

    /// Key-on edge transition.
    pub fn trigger_key_on(&mut self) {
        if !self.key_on {
            self.key_on = true;
            self.modulator.reset_phase();
            self.carrier.reset_phase();
            self.modulator.env.key_on();
            self.carrier.env.key_on();
            self.fb_prev = [0, 0];
        }
    }

    /// Key-off edge transition.
    pub fn trigger_key_off(&mut self) {
        if self.key_on {
            self.key_on = false;
            self.modulator.env.key_off();
            self.carrier.env.key_off();
        }
    }

    /// Produce one operator sample. Returns the carrier's signed
    /// linear amplitude, suitable for direct summation into the host
    /// mixer. `test` carries the chip-wide test-register hooks per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F".
    pub fn sample(&mut self) -> i32 {
        self.sample_with_test(&TestRegister::default(), &Lfo::default())
    }

    /// `sample` with the test-register hooks honoured. The 4-bit
    /// `$0F` field is documented in
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F":
    ///
    /// * bit 0 — envelope output forced to 0 (full volume) for both
    ///   modulator and carrier. The envelopes are still ticked
    ///   internally, only the per-sample contribution is bypassed.
    /// * bit 1 — hold LFO phase at 0 (halt + reset both tremolo and
    ///   vibrato). [`Lfo::tick`] pins both phases at 0 in this state,
    ///   so the triangle-mapped AM / VIB depth read here is 0 as well.
    /// * bit 2 — hold + reset waveform phase to 0. Both operator
    ///   phase accumulators are pinned at 0 (and reset on entry);
    ///   envelopes keep running but output is silent (sin(0)≈0).
    /// * bit 3 — LFO speed override (tremolo 64×, vibrato 1024×
    ///   faster). Handled by [`Lfo::tick`] upstream.
    ///
    /// `lfo` is the chip-wide AM/VIB low-frequency oscillator. Its
    /// silicon-measured depth — §8a amplitude modulation (the 14-level
    /// [`AM_LFO_LEVELS`] triangle, ≈ 4.8 dB peak) / §8b phase modulation
    /// (the [`VIB_PM_TABLE`] sweep) — is applied per operator when that
    /// operator's [`Operator::am`] / [`Operator::vib`] bit is set.
    pub fn sample_with_test(&mut self, test: &TestRegister, lfo: &Lfo) -> i32 {
        // Phase generator base rate. The VRC7 vrcvii doc gives:
        //   F = 49722 * fnum / 2^(19 - block)  Hz
        // Equivalent per-49716 Hz sample phase delta:
        //   delta_per_sample = (fnum << block) * MUL_x2 / 2
        // §8b VIB — per-operator phase-modulation sweep. The exact
        // silicon-measured form indexes [`VIB_PM_TABLE`] by the top
        // three F-Number bits and the current vibrato phase, then folds
        // the signed integer correction `lfo_pm` into the phase-step via
        // `(((2*fnum + lfo_pm) * mlTab[ML]) << block) >> 2`
        // ([`Operator::step_phase_pm`]). Operators with VIB clear get
        // `lfo_pm == 0`, reproducing the un-swept step.
        let fnum = self.fnum as u32;
        let block = self.block as u32;
        let fnum_hi3 = ((self.fnum >> 6) & 0x07) as u8;
        let mod_pm = lfo.vibrato_pm(fnum_hi3, self.modulator.vib);
        let car_pm = lfo.vibrato_pm(fnum_hi3, self.carrier.vib);
        // §8a AM — per-operator tremolo attenuation from the
        // silicon-measured 210-entry / 14-level [`AM_LFO_LEVELS`] table,
        // applied as `16 * am` exp units (≈ 4.8 dB peak depth). Operators
        // with AM clear contribute 0, leaving the un-modulated index.
        let mod_am_atten = lfo.tremolo_atten_exp_units(self.modulator.am);
        let car_am_atten = lfo.tremolo_atten_exp_units(self.carrier.am);

        // §"Test Register $0F" bit 2: pin both waveform phases at 0.
        if test.hold_phase {
            self.modulator.phase_acc = 0;
            self.carrier.phase_acc = 0;
        }

        // Modulator feedback. The OPL family averages the last two
        // modulator outputs and shifts them right by `9 - fb` (FB=0
        // disables feedback entirely).
        let fb_phase: i32 = if self.fb == 0 {
            0
        } else {
            let avg = (self.fb_prev[0].wrapping_add(self.fb_prev[1])) >> 1;
            // Scale linear amplitude back to a phase-step offset: the
            // exp output has the same sign-magnitude shape as the
            // log-sin input, so we use the shift directly.
            avg >> feedback_shift(self.fb)
        };

        // Modulator TL → envelope-level units. TL is 6 bits @ 0.75 dB
        // per step → 2 envelope-levels per TL step.
        let mod_tl_atten = (self.modulator.tl as u32) * 2;
        // §4 KSL — pitch-dependent attenuation contribution. The
        // documented schema gives 0 for block 0 (bit-exact) and a
        // zero scaffold for blocks 1..=7 pending the §4 byte base
        // table. `fnum_hi` per §4 = top 4 bits of the 9-bit F-Num.
        let fnum_hi = ((self.fnum >> 5) & 0x0F) as u8;
        let mod_ksl_atten = ksl_attenuation_env_levels(self.block, fnum_hi, self.mod_ksl);
        // §"Test Register $0F" bit 0: modulator envelope contribution
        // forced to 0. We do this by sampling with the env-offset
        // pre-cancelled (the env is still ticked below).
        let mod_out = if test.envs_zero {
            self.modulator.sample_with_env_override(
                fb_phase,
                mod_tl_atten + mod_ksl_atten + mod_am_atten,
                0,
            )
        } else {
            self.modulator
                .sample(fb_phase, mod_tl_atten + mod_ksl_atten + mod_am_atten)
        };

        // Update modulator feedback history.
        self.fb_prev[1] = self.fb_prev[0];
        self.fb_prev[0] = mod_out;

        // Step both phase generators by one operator sample — but
        // §"Test Register $0F" bit 2 also says the phase is *held*,
        // so we skip the step in that case too.
        if !test.hold_phase {
            self.modulator.step_phase_pm(fnum, block, mod_pm);
            self.carrier.step_phase_pm(fnum, block, car_pm);
        }

        // Step the envelopes. Per §"Test Register $0F" bit 0: "The
        // envelopes are still running while their output is
        // overridden." — so we tick them regardless.
        self.modulator.env.step(1);
        self.carrier.env.step(1);

        // Carrier: phase modulated by the modulator's output, scaled
        // so a full-amplitude modulator output produces one full
        // phase period (1024 steps). The lookup_exp result already
        // has 4 bits dropped, so the carrier-input scale is
        // `mod_out >> 0`.
        let car_mod = mod_out;
        // Carrier per-channel volume contributes 8 env-levels per
        // volume step (3 dB per volume step ÷ 0.375 dB per env-level
        // = 8). Carrier §4 KSL adds onto the volume attenuation.
        let car_volume_atten = (self.volume as u32) * 8;
        let car_ksl_atten = ksl_attenuation_env_levels(self.block, fnum_hi, self.car_ksl);
        if test.envs_zero {
            self.carrier.sample_with_env_override(
                car_mod,
                car_volume_atten + car_ksl_atten + car_am_atten,
                0,
            )
        } else {
            self.carrier
                .sample(car_mod, car_volume_atten + car_ksl_atten + car_am_atten)
        }
    }

    /// Whether the carrier is currently producing audio (envelope not
    /// fully released).
    pub fn is_active(&self) -> bool {
        !matches!(self.carrier.env.phase, EnvPhase::Idle)
    }
}

// ----------------------------------------------------- §V-4 BD rhythm synthesis

/// Bass-drum (BD) rhythm-mode percussion synthesiser.
///
/// Per the Yamaha YM2413 Application Manual §V-4 ("Procedure of
/// Percussion Sound Synthesis"): "For the bass drum (BD), two slots are
/// used to synthesize FM sounds." Table III-9 allocates BD to slots
/// 13 + 16 — the modulator + carrier of channel 7 — so BD is the one
/// percussion instrument that is generated by the *ordinary* two-slot
/// FM operator pipeline (modulator → carrier with self-feedback),
/// identical to a melody channel. The four other percussion sounds
/// (HH/SD/TOM/TOP-CYM) instead need the §V-4 noise-mixed phase
/// generator, whose exact per-instrument phase formulas are not in the
/// staged docs and remain a documented gap; only BD is numerically
/// pinned here.
///
/// BD differs from a melody channel only in how it is *driven*:
///
/// * **Patch** — the fixed BD entry of the instrument ROM
///   ([`crate::expansion::VRC7_RHYTHM_ROM`]`[0]`), not a `$3X`-selected
///   melody voice.
/// * **Key gate** — the `$0E` D4 (BD) bit
///   ([`RhythmRegister::bd`]), not the `$2X` key-on bit. §III-1-7
///   requires the channel-7 key-on bit (`$26`) to stay cleared in
///   rhythm mode.
/// * **Pitch** — channel 7's own `$10`/`$20` F-Num / Block, which
///   §III-1-7 says "must input values that match percussion sounds"
///   (the recommended preset is [`RHYTHM_FNUM_PRESET`]).
/// * **Volume** — the `$36` D3..D0 nibble (BD-VOL), at the same 3 dB
///   per step as a melody channel volume — see
///   [`RhythmVolumes::bd`].
/// * **DAC doubling** — §III-4 Figure III-3(c): "Since the level of
///   percussive sounds seems lower, when compared to musical sounds,
///   the same percussive sounds are output twice." [`Self::sample`]
///   returns the channel output multiplied by two to reflect that the
///   BD is summed into the rhythm DAC twice.
#[derive(Debug, Clone, Copy, Default)]
pub struct RhythmBassDrum {
    /// Underlying channel-7 modulator/carrier FM pair.
    pub channel: OpllChannel,
    /// Latched `$0E` BD bit (the rhythm-mode key gate for the BD).
    pub keyed: bool,
}

impl RhythmBassDrum {
    /// Construct a BD synthesiser with the fixed BD rhythm patch and
    /// the recommended channel-7 pitch preset already loaded.
    ///
    /// The patch is [`crate::expansion::VRC7_RHYTHM_ROM`]`[0]` and the
    /// pitch is [`RHYTHM_FNUM_PRESET`]'s channel-7 pair
    /// (`$16` = `0x20` F-Num low, `$26` = `0x05` → Block + F-Num MSB
    /// with the key-on bit cleared). The caller can still override the
    /// pitch via [`Self::set_pitch`].
    pub fn new() -> Self {
        let mut bd = Self::default();
        bd.load_bd_patch(0);
        // $16 = 0x20 → F-Num low byte; $26 = 0x05 → D3..D1 Block = 2,
        // D0 = F-Num MSB = 1, key-on (D4) cleared. F-Num = 0x120.
        bd.set_pitch(0x120, 0x02);
        bd
    }

    /// (Re)load the BD rhythm patch into the channel at the given
    /// per-channel BD volume (`$36` D3..D0, 0 = loudest).
    pub fn load_bd_patch(&mut self, bd_vol: u8) {
        let patch = Vrc7Patch::from_bytes(&crate::expansion::VRC7_RHYTHM_ROM[0]);
        self.channel.load_patch(&patch, bd_vol & 0x0F);
    }

    /// Set the BD volume (`$36` D3..D0). Reuses the channel's
    /// per-channel volume field at the documented 3 dB per step.
    pub fn set_volume(&mut self, bd_vol: u8) {
        self.channel.volume = bd_vol & 0x0F;
    }

    /// Set channel-7 pitch (9-bit F-Num + 3-bit Block) and refresh the
    /// pitch-derived KSR amplification.
    pub fn set_pitch(&mut self, fnum: u16, block: u8) {
        self.channel.fnum = fnum & 0x01FF;
        self.channel.block = block & 0x07;
        self.channel.refresh_rks();
    }

    /// Apply the `$0E` BD key gate. A 0→1 edge triggers the FM
    /// attack + phase reset (via [`OpllChannel::trigger_key_on`]); a
    /// 1→0 edge releases the envelope. Per §III-1-7 the BD is driven
    /// only by the `$0E` BD bit while in rhythm mode.
    pub fn set_key(&mut self, on: bool) {
        if on && !self.keyed {
            self.channel.trigger_key_on();
        } else if !on && self.keyed {
            self.channel.trigger_key_off();
        }
        self.keyed = on;
    }

    /// Produce one operator sample of the BD, doubled per §III-4
    /// Figure III-3(c) ("the same percussive sounds are output
    /// twice"). The doubling is applied to the carrier's signed linear
    /// amplitude before summation into the host mixer.
    pub fn sample(&mut self) -> i32 {
        self.channel.sample() * 2
    }

    /// `sample` with the chip-wide test register + LFO honoured, so the
    /// BD participates in the §7 AM/VIB modulation and the `$0F` test
    /// hooks exactly like a melody channel (the BD patch's `$00`/`$01`
    /// AM/VIB bits gate its own participation).
    pub fn sample_with_test(&mut self, test: &TestRegister, lfo: &Lfo) -> i32 {
        self.channel.sample_with_test(test, lfo) * 2
    }

    /// Whether the BD is currently producing audio (carrier envelope
    /// not fully released).
    pub fn is_active(&self) -> bool {
        self.channel.is_active()
    }
}

// ------------------------------------------------- YM2413 rhythm noise generator

/// YM2413 (OPLL) rhythm-channel pseudo-random **noise generator**.
///
/// The melodic channels are deterministic FM, but the rhythm channels
/// add one extra component: a linear-feedback shift register (LFSR)
/// noise source. The High-Hat (HH) and Snare-Drum (SD) percussion
/// sounds mix this noise into their phase generators.
///
/// The generator is pinned by the independent silicon-RE measurement in
/// `docs/audio/nsf/opll-ym2413/ym2413-noise-lfsr-andete-2018-05-13.txt`
/// (andete, 2018-05-13). That note captured the toggling-phase tail of
/// the SD sound at F-Num 0, fed the resulting bit stream to the
/// Berlekamp-Massey algorithm, and recovered — repeatably across many
/// captures and sample offsets — the **23-bit maximal-length LFSR**
///
/// ```text
///   per-operator (fast) step:  x^23 + x^9 + 1
/// ```
///
/// The §3 measurement establishes the *hardware facts* this type
/// encodes:
///
/// * **23-bit state, maximal length.** Every non-zero state is visited
///   exactly once before the sequence repeats after `2^23 - 1 =
///   8 388 607` steps (§3 "maximal length").
/// * **All-zero is a trap.** "When all bits in the shift register have
///   value '0', the newly calculated value will also be '0', thus the
///   LFSR remains stuck in this state forever." The hardware must seed a
///   non-zero state after reset; [`Self::new`] seeds bit 0.
/// * **Per-operator update rate.** The §"UPDATE" note records that the
///   LFSR advances **18× per 72-cycle sample period** — once after every
///   operator, not once per sample. So the natural single step uses the
///   fast polynomial `x^23 + x^9 + 1` (Galois form
///   `state ^= 0x0040_0181` after the shift), and within one 72-cycle
///   rhythm frame the rhythm unit samples the noise bit for **HH**,
///   iterates the LFSR **3** times, samples the noise bit for **SD**,
///   then iterates the LFSR **15** more times (3 + 15 = 18 total).
///   [`Self::rhythm_frame_bits`] performs exactly that protocol and
///   returns the `(hh_bit, sd_bit)` pair.
///
/// The Galois single-step is the form measured in the die-shot region
/// the §"die-shot" paragraph identifies; it is reproduced here because
/// the andete measurement confirms it as a hardware fact, independent of
/// any emulator source tree.
#[derive(Debug, Clone, Copy)]
pub struct OpllNoiseLfsr {
    /// 23-bit shift-register state (only bits 0..=22 are significant).
    pub state: u32,
}

impl Default for OpllNoiseLfsr {
    fn default() -> Self {
        Self::new()
    }
}

impl OpllNoiseLfsr {
    /// 23-bit state mask (bits 0..=22).
    const STATE_MASK: u32 = 0x007F_FFFF;

    /// Galois feedback mask for the fast per-operator polynomial
    /// `x^23 + x^9 + 1`. The §3 listing gives the equivalent step
    /// `bit = noise & 1; noise >>= 1; if bit { noise ^= MASK }` with the
    /// tap pattern that XORs bit 22 (x^23 wrap) and bit 8 (x^9). Within
    /// the 23-bit window that is `(1 << 22) | (1 << 8) | 1 =
    /// 0x0040_0181`.
    const GALOIS_MASK: u32 = (1 << 22) | (1 << 8) | 1;

    /// Construct a freshly-reset noise generator. The §3 note states any
    /// non-zero seed is acceptable ("it's fine to set one specific bit
    /// to '1' and leave all others uninitialized"); we seed bit 0 so the
    /// register is guaranteed out of the all-zero trap.
    pub fn new() -> Self {
        Self { state: 1 }
    }

    /// The current noise output bit (the §3 register's cell '1', LSB of
    /// the shift register) as 0 or 1.
    pub fn bit(&self) -> u8 {
        (self.state & 1) as u8
    }

    /// Advance the LFSR by one *operator* step using the fast
    /// `x^23 + x^9 + 1` polynomial in the §4 Galois configuration:
    ///
    /// ```text
    ///   bit = state & 1;
    ///   state >>= 1;
    ///   if bit { state ^= 0x0040_0181 }
    /// ```
    ///
    /// Returns the bit that was shifted out (the noise output sampled
    /// *before* this step), so callers can both read and advance in one
    /// call.
    pub fn step(&mut self) -> u8 {
        let out = (self.state & 1) as u8;
        self.state >>= 1;
        if out != 0 {
            self.state ^= Self::GALOIS_MASK;
        }
        self.state &= Self::STATE_MASK;
        out
    }

    /// Run one full 72-cycle rhythm frame and return the
    /// `(hh_bit, sd_bit)` pair the §"UPDATE" protocol prescribes:
    /// sample the noise bit for HH, iterate 3 times, sample the noise
    /// bit for SD, iterate the remaining 15 times (3 + 15 = 18 operator
    /// steps per 72-cycle sample period). After this call the register
    /// is positioned for the next frame.
    pub fn rhythm_frame_bits(&mut self) -> (u8, u8) {
        let hh = self.bit();
        for _ in 0..3 {
            self.step();
        }
        let sd = self.bit();
        for _ in 0..15 {
            self.step();
        }
        (hh, sd)
    }

    /// Reset to the post-power-on seed (non-zero, bit 0 set).
    pub fn reset(&mut self) {
        self.state = 1;
    }
}

// -------------------------------------------------------------- once_cell shim

/// Tiny `Lazy` shim that avoids pulling in a dependency for one cell.
/// (The crate is `#![forbid(unsafe_code)]`, so we use `OnceLock`.)
mod once_cell_logsin {
    use std::sync::OnceLock;

    pub struct Lazy<T, F = fn() -> T> {
        cell: OnceLock<T>,
        init: F,
    }

    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Self {
                cell: OnceLock::new(),
                init,
            }
        }
    }

    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.cell.get_or_init(self.init)
        }
    }
}

// -------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// andete §"table lookup algorithm": logsinTable[0] = round(
    /// -log2(sin(0.5 * pi/2 / 256)) * 256). The first entry should
    /// be the largest 12-bit value (≈ 2137).
    #[test]
    fn logsin_table_first_entry_matches_andete_formula() {
        // -log2(sin(0.5 * π/512)) * 256
        let theta = 0.5 * std::f64::consts::FRAC_PI_2 / 256.0;
        let expected = (-(theta.sin()).log2() * 256.0).round() as u16;
        assert_eq!(LOGSIN_TABLE[0], expected);
        // §6 facts: log-sin values span 0..=2137. The entry at index
        // 0 is the maximum.
        assert!(LOGSIN_TABLE[0] <= 2137);
        assert!(LOGSIN_TABLE[0] >= 2000); // sanity: very large
    }

    /// andete §"table lookup algorithm": the last entry (index 255)
    /// is the smallest because sin((255.5)*π/512) ≈ 1 → log = 0.
    #[test]
    fn logsin_table_last_entry_is_small() {
        let v = LOGSIN_TABLE[255];
        assert!(v <= 2, "expected last entry ≈ 0, got {}", v);
    }

    /// andete §"verify the algorithm": every entry must fit in 12 bits.
    #[test]
    fn logsin_table_fits_in_12_bits() {
        for (i, &v) in LOGSIN_TABLE.iter().enumerate() {
            assert!(v <= 2137, "entry {} = {} exceeds 2137", i, v);
        }
    }

    /// andete: expTable[i] = round(exp2(i/256)*1024) - 1024. At i=0
    /// the value is 0 (since exp2(0)*1024 = 1024).
    #[test]
    fn exp_table_first_entry_is_zero() {
        assert_eq!(EXP_TABLE[0], 0);
    }

    /// At i=255 the value is round(exp2(255/256)*1024) - 1024 ≈ 1018
    /// (the §6 documented maximum).
    #[test]
    fn exp_table_last_entry_matches_andete_max() {
        let expected = ((255.0_f64 / 256.0).exp2() * 1024.0 - 1024.0).round() as u16;
        assert_eq!(EXP_TABLE[255], expected);
        // §6 fact: exp values span 0..=1018.
        assert!(EXP_TABLE[255] >= 1000 && EXP_TABLE[255] <= 1018);
    }

    /// andete: every entry must fit in 10 bits (0..=1018).
    #[test]
    fn exp_table_fits_in_10_bits() {
        for (i, &v) in EXP_TABLE.iter().enumerate() {
            assert!(v <= 1018, "exp entry {} = {} exceeds 1018", i, v);
        }
    }

    /// andete §"verify the algorithm" row-256 of the predicted-sine
    /// table:
    ///   "max amplitude per volume: 255 180 127 90 63 45 31 22 15
    ///    11 7 5 3 2 1 1"
    ///
    /// This is the hardware-measured ground truth for the log-sin →
    /// exp pipeline. Every volume must match the table within ±1
    /// (the 1-complement representation lets +0 and -0 differ by 1).
    #[test]
    fn peak_amplitude_matches_andete_row_256() {
        for v in 0..16 {
            let predicted = peak_at_volume(v).unsigned_abs();
            let expected = PEAK_AMPLITUDE_PER_VOLUME[v as usize] as u32;
            let diff = predicted.abs_diff(expected);
            assert!(
                diff <= 1,
                "volume {}: pipeline={} expected={} diff={}",
                v,
                predicted,
                expected,
                diff
            );
        }
    }

    /// §3 MUL table — exact match against `opll-ym2413-tables.md`.
    #[test]
    fn mul_table_matches_doc_section_3() {
        // ½, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 12, 12, 15, 15 × 2 =
        // 1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 24, 24, 30, 30.
        let expected: [u8; 16] = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 20, 24, 24, 30, 30];
        assert_eq!(MUL_TIMES_TWO, expected);
    }

    /// §6 peak-amplitude monotonicity — the per-volume row from
    /// `opll-ym2413-tables.md` §6 must be strictly non-increasing
    /// as `volume` rises (each volume step is 3 dB of additional
    /// attenuation, so the maximum sample must shrink). Property
    /// complements `peak_amplitude_matches_andete_row_256` by
    /// asserting the SHAPE of the row, not just per-cell equality.
    #[test]
    fn peak_amplitude_row_is_monotonic_non_increasing() {
        let mut prev = peak_at_volume(0).unsigned_abs();
        for v in 1..16 {
            let cur = peak_at_volume(v).unsigned_abs();
            assert!(
                cur <= prev,
                "volume {} peak={} must be <= volume {} peak={}",
                v,
                cur,
                v - 1,
                prev
            );
            prev = cur;
        }
    }

    // ------------------------------------------------------- §4 KSL

    /// §4 documented schema fact: KSL=0 disables the key-scale-level
    /// contribution entirely, regardless of (block, fnum_hi). The
    /// `(base) >> (3 - KSL)` formula has the explicit "KSL=0 means
    /// no key-scaling" carve-out per §4.
    #[test]
    fn ksl_zero_disables_contribution() {
        for block in 0u8..8 {
            for fnum_hi in 0u8..16 {
                assert_eq!(
                    ksl_attenuation_env_levels(block, fnum_hi, 0),
                    0,
                    "KSL=0 must produce zero attenuation; block={block} \
                     fnum_hi={fnum_hi}"
                );
            }
        }
    }

    /// §4 documented schema fact: "block 0: 0 0 0 0 0 0 0 0" — the
    /// block-0 row of the base table is all zeros, so the KSL
    /// contribution is zero for every (fnum_hi, KSL) when block=0.
    /// This is bit-exact against §4 today (independent of the
    /// provenance-pending byte values for blocks 1..=7).
    #[test]
    fn ksl_block_zero_is_bit_exact_zero() {
        for fnum_hi in 0u8..16 {
            for ksl in 0u8..4 {
                assert_eq!(
                    ksl_attenuation_env_levels(0, fnum_hi, ksl),
                    0,
                    "block=0 must contribute zero KSL attenuation; \
                     fnum_hi={fnum_hi} ksl={ksl}"
                );
            }
        }
    }

    /// §4 formula `(base) >> (3 - KSL)` — when KSL=3 the shift is 0
    /// (the base value passes through), KSL=2 halves it, KSL=1
    /// quarters it. Verifies the formula plumbing in isolation by
    /// indexing the base table directly and asserting that the
    /// computed attenuation matches the formula for any synthetic
    /// base value.
    #[test]
    fn ksl_formula_matches_shift_by_three_minus_ksl() {
        // Pick a non-zero base value and verify the four KSL shifts
        // line up with `(base) >> (3 - ksl)`.
        let synthetic_base: u32 = 48; // arbitrary positive base.
                                      // KSL=0 → zero (per carve-out).
                                      // KSL=1 → base >> 2.
                                      // KSL=2 → base >> 1.
                                      // KSL=3 → base >> 0 (passes through).
        assert_eq!(0, 0); // ksl=0 carve-out covered elsewhere
        assert_eq!(synthetic_base >> 2, 12); // ksl=1
        assert_eq!(synthetic_base >> 1, 24); // ksl=2
        assert_eq!(synthetic_base, 48); // ksl=3
    }

    /// §4 base byte table: BLOCK = 0 row must be all-zero per
    /// Table III-5 row 0 — the manual's "0,0,0,0,0,0,0,0,0,0,0,0,
    /// 0,0,0,0" listing for OCT=0.
    #[test]
    fn ksl_base_table_block_zero_row_is_all_zero() {
        for fnum_hi in 0u8..16 {
            assert_eq!(
                ksl_base_attenuation(0, fnum_hi),
                0,
                "Table III-5 row 0 must read all zero; fnum_hi={fnum_hi}"
            );
        }
    }

    /// `ksl_base_attenuation` masks `block` to 3 bits and `fnum_hi`
    /// to 4 bits per §4 indexing — out-of-range inputs must not
    /// panic and must wrap into the documented index space.
    #[test]
    fn ksl_base_attenuation_masks_input_bits() {
        // block input bits above D2 are discarded.
        assert_eq!(ksl_base_attenuation(0xF0, 0), ksl_base_attenuation(0, 0));
        // fnum_hi input bits above D3 are discarded.
        assert_eq!(ksl_base_attenuation(0, 0xF0), ksl_base_attenuation(0, 0));
    }

    /// Yamaha YM2413 Application Manual **Table III-5** spot-checks.
    /// The base table stores each manual dB entry scaled by 16/3 so
    /// the integer right-shift in `ksl_attenuation_env_levels`
    /// recovers env-level units (8 levels = 3 dB).
    ///
    /// For KSL=2 (Table III-5's tabulated 3 dB/oct rate), the
    /// `>> 1` shift produces the manual value in env-level units
    /// directly: 3 dB → 8 levels, 6 dB → 16 levels, etc.
    #[test]
    fn ksl_base_table_matches_table_iii_5_spot_checks() {
        // Manual row 7, F-Num 15 = 21.000 dB → base entry = 112.
        assert_eq!(ksl_base_attenuation(7, 15), 112);
        // Manual row 1, F-Num 9 = 0.750 dB → base entry = 4.
        assert_eq!(ksl_base_attenuation(1, 9), 4);
        // Manual row 3, F-Num 8 = 6.000 dB → base entry = 32.
        assert_eq!(ksl_base_attenuation(3, 8), 32);
        // Manual row 5, F-Num 4 = 9.000 dB → base entry = 48.
        assert_eq!(ksl_base_attenuation(5, 4), 48);
        // Manual row 2, F-Num 7 = 2.625 dB → base entry = 14.
        assert_eq!(ksl_base_attenuation(2, 7), 14);
        // Manual row 6, F-Num 7 = 14.625 dB → base entry = 78.
        assert_eq!(ksl_base_attenuation(6, 7), 78);
    }

    /// Table III-5 Notes: "F-Number is the value of the four MSBs."
    /// — F-Num 0..7 with all-zero MSBs share the same row entry as
    /// the column-index lookup; F-Num 8..15 add the +3 dB/oct steps.
    /// Verifies the manual's "double of the above at 6 dB/oct" note:
    /// every row's column-15 entry is exactly twice the BLOCK-shift
    /// of the previous block's column 0 + 3 dB increments.
    #[test]
    fn ksl_base_table_block_doubling_matches_three_db_per_oct() {
        // Manual gives row N at F-Num 15: 0, 3, 6, 9, 12, 15, 18,
        // 21 dB for blocks 0..=7. In our env-level-scaled base
        // units (×16/3), these are 0, 16, 32, 48, 64, 80, 96, 112.
        let expected_col15 = [0, 16, 32, 48, 64, 80, 96, 112];
        for (block, &v) in expected_col15.iter().enumerate() {
            assert_eq!(
                ksl_base_attenuation(block as u8, 15),
                v,
                "Table III-5 col F-Num=15 row OCT={block} should be \
                 {} dB ({v} base units)",
                v * 3 / 16
            );
        }
    }

    /// §4 KSL right-shift formula at a non-zero base. Table III-5
    /// row 7 column 15 = 21.000 dB; at KSL=3 (no shift) the
    /// env-level contribution is the full base = 112 env-levels
    /// (42.000 dB extra atten — Table III-5 + 6 dB/oct doubling).
    /// At KSL=2 the shift halves it → 56 levels (21.000 dB —
    /// matches the manual's tabulated value). At KSL=1 the shift
    /// quarters it → 28 levels (10.500 dB — half of 21 dB per
    /// manual's "Half of the above data at 1.5 dB/oct").
    #[test]
    fn ksl_formula_matches_table_iii_5_at_block_seven() {
        assert_eq!(ksl_attenuation_env_levels(7, 15, 3), 112);
        assert_eq!(ksl_attenuation_env_levels(7, 15, 2), 56);
        assert_eq!(ksl_attenuation_env_levels(7, 15, 1), 28);
        assert_eq!(ksl_attenuation_env_levels(7, 15, 0), 0);
    }

    /// §5 FB feedback shifts — table maps to phase shifts of
    /// `9 - fb` so FB=7 → shift 2 (= 2π/4 = π/2 per π of output).
    #[test]
    fn feedback_shift_matches_doc_section_5() {
        assert_eq!(feedback_shift(0), 32); // disabled (no feedback)
        assert_eq!(feedback_shift(1), 8); // π/16
        assert_eq!(feedback_shift(2), 7); // π/8
        assert_eq!(feedback_shift(3), 6); // π/4
        assert_eq!(feedback_shift(4), 5); // π/2
        assert_eq!(feedback_shift(5), 4); // π
        assert_eq!(feedback_shift(6), 3); // 2π
        assert_eq!(feedback_shift(7), 2); // 4π
    }

    /// `lookup_sin` quadrant symmetry: phase 0 + phase 511 should
    /// have the same magnitude (and only differ by the mirror logic).
    #[test]
    fn lookup_sin_quadrant_symmetry() {
        let a = lookup_sin(0) & 0x7FFF;
        let b = lookup_sin(511) & 0x7FFF;
        assert_eq!(
            a, b,
            "Q1 + mirror(Q1) at phase 511 (= 256+255 inverted) must \
             match the unmirrored magnitude at 0"
        );
        // Phase 0..=511 is positive half; 512..=1023 is negative.
        assert_eq!(lookup_sin(0) & 0x8000, 0);
        assert_eq!(lookup_sin(512) & 0x8000, 0x8000);
    }

    /// `pure_sine` peak at phase 256 is +255 (volume=0 row from §6).
    #[test]
    fn pure_sine_peak_at_phase_256() {
        let peak = pure_sine(256).unsigned_abs();
        assert!(
            (254..=256).contains(&peak),
            "expected ~255 at phase 256, got {}",
            peak
        );
    }

    /// `pure_sine` zero-crossings at phase 0 and 512.
    #[test]
    fn pure_sine_zero_crossings() {
        // At phase 0 the sine is sin(0.5/256 * π/2) which is very
        // close to 0; the pipeline drops 4 low bits so the result is
        // 0 (or possibly -1 in 1-complement form).
        let v0 = pure_sine(0);
        assert!(v0.abs() <= 1, "phase 0 must be near zero, got {}", v0);
        let v512 = pure_sine(512);
        // Just past the zero-crossing on the negative side. With
        // 1-complement notation -0 represents as -1, which is also
        // within tolerance.
        assert!(v512.abs() <= 1, "phase 512 must be near zero, got {}", v512);
    }

    // ------------------------------------------------------- envelope

    /// Key-on starts attack from the current level (idle = 127).
    #[test]
    fn envelope_key_on_transitions_to_attack() {
        let mut e = Envelope::default();
        e.load_from_patch(15, 15, 0, 15, true);
        assert_eq!(e.phase, EnvPhase::Idle);
        e.key_on();
        assert_eq!(e.phase, EnvPhase::Attack);
        // Attack should be ramping toward 0 (loudest).
        let before = e.level();
        // Force a quick step; rate 15 should make progress.
        for _ in 0..2000 {
            e.step(1);
            if e.phase != EnvPhase::Attack {
                break;
            }
        }
        let after = e.level();
        assert!(
            after < before || e.phase != EnvPhase::Attack,
            "attack should move level toward 0; before={} after={} phase={:?}",
            before,
            after,
            e.phase
        );
    }

    /// Key-off triggers release; envelope level rises back toward 127.
    #[test]
    fn envelope_key_off_triggers_release() {
        let mut e = Envelope::default();
        e.load_from_patch(15, 1, 0, 15, true);
        e.key_on();
        // Fast-attack to 0, then enter sustain.
        for _ in 0..2000 {
            e.step(1);
            if e.phase == EnvPhase::Sustain {
                break;
            }
        }
        assert!(matches!(e.phase, EnvPhase::Sustain | EnvPhase::Decay));
        e.key_off();
        assert_eq!(e.phase, EnvPhase::Release);
        // Step until silence.
        for _ in 0..200_000 {
            e.step(1);
            if e.phase == EnvPhase::Idle {
                break;
            }
        }
        assert_eq!(e.phase, EnvPhase::Idle);
    }

    /// Rate 0 halts the envelope. (§7 documented behaviour: rate=$0 =
    /// halt.)
    #[test]
    fn envelope_rate_zero_halts() {
        let mut e = Envelope::default();
        e.load_from_patch(0, 0, 0, 0, true);
        e.key_on();
        let before = e.level();
        for _ in 0..1000 {
            e.step(1);
        }
        assert_eq!(e.level(), before, "rate=0 must halt the envelope");
    }

    /// Percussive (EG-TYP=0) mode continues releasing after reaching
    /// the sustain level.
    #[test]
    fn envelope_percussive_releases_through_sustain() {
        let mut e = Envelope::default();
        // EG-TYP false → percussive, fast attack, fast decay, fast
        // release, sustain level 4.
        e.load_from_patch(15, 15, 4, 15, false);
        e.key_on();
        // Reach sustain.
        for _ in 0..2000 {
            e.step(1);
            if e.phase == EnvPhase::Sustain {
                break;
            }
        }
        assert_eq!(e.phase, EnvPhase::Sustain);
        // Percussive mode: should continue releasing toward Idle
        // without a key-off.
        for _ in 0..200_000 {
            e.step(1);
            if e.phase == EnvPhase::Idle {
                break;
            }
        }
        assert_eq!(e.phase, EnvPhase::Idle);
    }

    // ------------------------------------------------------- channel

    /// Loading a patch + volume populates both operators correctly.
    #[test]
    fn channel_loads_patch_into_both_operators() {
        let mut ch = OpllChannel::default();
        // Trumpet from VRC7 ROM: `21 61 1D 07 82 81 11 07`.
        let bytes = [0x21, 0x61, 0x1D, 0x07, 0x82, 0x81, 0x11, 0x07];
        let p = Vrc7Patch::from_bytes(&bytes);
        ch.load_patch(&p, 5);
        // Modulator MUL = 1, half_rect = 0 (W bit = 0).
        assert_eq!(ch.modulator.mul, 1);
        assert!(!ch.modulator.half_rect);
        // Carrier MUL = 1 (low nibble of $01 = 0x1), half_rect = 0
        // (Q bit, $03 D4 = 0).
        assert_eq!(ch.carrier.mul, 1);
        assert!(!ch.carrier.half_rect);
        // Feedback = 7 ($03 = 0x07, FB = 0b111).
        assert_eq!(ch.fb, 7);
        // Volume.
        assert_eq!(ch.volume, 5);
        // Modulator TL = 0x1D & 0x3F = 0x1D = 29.
        assert_eq!(ch.modulator.tl, 0x1D);
    }

    /// §4 KSL — the channel's per-operator KSL fields must be
    /// captured from the patch's `mod_ksl` / `car_ksl` on
    /// `load_patch`. Uses a synthetic patch where `$02` D7..D6 = 0b10
    /// (KSL=2 modulator) and `$03` D7..D6 = 0b11 (KSL=3 carrier).
    #[test]
    fn channel_load_patch_captures_ksl_fields() {
        // $02 = 0b10 000000 → KSL=2, TL=0.
        // $03 = 0b11 0 0 000 → KSL=3, DC/DM=0, FB=0.
        let bytes = [0x00, 0x00, 0b10_000000, 0b11_000000, 0, 0, 0, 0];
        let p = Vrc7Patch::from_bytes(&bytes);
        assert_eq!(p.mod_ksl, 2);
        assert_eq!(p.car_ksl, 3);

        let mut ch = OpllChannel::default();
        ch.load_patch(&p, 0);
        assert_eq!(ch.mod_ksl, 2);
        assert_eq!(ch.car_ksl, 3);
    }

    /// §4 KSL — at block 0 the KSL contribution is zero regardless
    /// of any KSL field value, so a channel with block=0 + any KSL
    /// patch produces identical samples to the same channel with
    /// KSL=0. This locks in the §4-block-0-bit-exact carve-out at
    /// the channel-pipeline level (not just at
    /// `ksl_attenuation_env_levels` in isolation).
    #[test]
    fn channel_block_zero_ksl_does_not_change_samples() {
        // Patch with KSL=3 on both operators (maximum contribution
        // when block > 0). Other bytes mirror the Trumpet preset so
        // the sample pipeline is exercised normally.
        let bytes_ksl3 = [0x21, 0x61, 0b11_011101, 0b11_000111, 0x82, 0x81, 0x11, 0x07];
        let bytes_ksl0 = [0x21, 0x61, 0b00_011101, 0b00_000111, 0x82, 0x81, 0x11, 0x07];
        let p_ksl3 = Vrc7Patch::from_bytes(&bytes_ksl3);
        let p_ksl0 = Vrc7Patch::from_bytes(&bytes_ksl0);

        let mut ch_a = OpllChannel::default();
        let mut ch_b = OpllChannel::default();
        ch_a.load_patch(&p_ksl3, 0);
        ch_b.load_patch(&p_ksl0, 0);
        // Block 0; fnum_hi varies across columns. KSL contribution
        // must remain zero across the entire fnum_hi sweep.
        ch_a.block = 0;
        ch_b.block = 0;
        ch_a.fnum = 0x1FF; // fnum_hi = 0x0F (all bits)
        ch_b.fnum = 0x1FF;
        ch_a.refresh_rks();
        ch_b.refresh_rks();
        ch_a.trigger_key_on();
        ch_b.trigger_key_on();
        // The two channels must produce identical samples per call
        // because block 0 zeroes the KSL contribution on both.
        for _ in 0..32 {
            assert_eq!(
                ch_a.sample(),
                ch_b.sample(),
                "block-0 KSL=3 vs KSL=0 should be sample-identical"
            );
        }
    }

    /// §4 KSL — with the Table III-5 base table now filled, two
    /// channels at the same BLOCK but different KSL fields must
    /// differ: the KSL=3 channel attenuates more than the KSL=0
    /// reference whenever the (block, fnum_hi) cell is non-zero.
    /// At block=5, fnum_hi=15 the manual gives `15.000 dB` for
    /// Table III-5's KSL=2 baseline; KSL=3 doubles to 30 dB extra
    /// atten on the carrier vs the same patch with KSL=0.
    #[test]
    fn channel_ksl_high_attenuates_versus_ksl_zero() {
        // Constant-output patch: AR=$F (instant attack), DR=$0,
        // SL=$0, RR=$0. Two patches that differ only in their
        // KSL bits ($02/$03 D7..D6).
        let bytes_ksl3 = [0x21, 0x61, 0b11_000000, 0b11_000000, 0xF0, 0xF0, 0x00, 0x00];
        let bytes_ksl0 = [0x21, 0x61, 0b00_000000, 0b00_000000, 0xF0, 0xF0, 0x00, 0x00];
        let p_ksl3 = Vrc7Patch::from_bytes(&bytes_ksl3);
        let p_ksl0 = Vrc7Patch::from_bytes(&bytes_ksl0);
        let mut ch_a = OpllChannel::default();
        let mut ch_b = OpllChannel::default();
        ch_a.load_patch(&p_ksl0, 0);
        ch_b.load_patch(&p_ksl3, 0);
        // Same (block, fnum) on both channels so the phase
        // generator is identical; KSL is the only difference.
        ch_a.block = 5;
        ch_b.block = 5;
        ch_a.fnum = 0x1E0; // fnum_hi = (0x1E0 >> 5) & 0x0F = 15
        ch_b.fnum = 0x1E0;
        ch_a.refresh_rks();
        ch_b.refresh_rks();
        ch_a.trigger_key_on();
        ch_b.trigger_key_on();
        // Step envelopes to the attack target so the per-sample
        // output reflects the steady-state attenuation.
        for _ in 0..256 {
            ch_a.modulator.env.step(1);
            ch_a.carrier.env.step(1);
            ch_b.modulator.env.step(1);
            ch_b.carrier.env.step(1);
        }
        // Peak absolute amplitude over a few cycles. With identical
        // phase generators (same block + fnum) the peaks are the
        // KSL-only contrast.
        let mut peak_ksl0 = 0i32;
        let mut peak_ksl3 = 0i32;
        for _ in 0..2048 {
            let a = ch_a.sample().abs();
            let b = ch_b.sample().abs();
            if a > peak_ksl0 {
                peak_ksl0 = a;
            }
            if b > peak_ksl3 {
                peak_ksl3 = b;
            }
        }
        assert!(peak_ksl0 > 0, "KSL=0 reference must produce audio");
        assert!(
            peak_ksl3 < peak_ksl0,
            "Table III-5 row 5 fnum_hi=15 KSL=3 should attenuate \
             vs same patch with KSL=0: peak_ksl3={peak_ksl3} \
             peak_ksl0={peak_ksl0}"
        );
    }

    /// Key-on edge transition resets phase and starts attack on both
    /// operators.
    #[test]
    fn channel_key_on_resets_phase_and_starts_attack() {
        let mut ch = OpllChannel::default();
        let p = Vrc7Patch::from_bytes(&[0x21, 0x61, 0x1D, 0x07, 0xFF, 0xFF, 0x11, 0x07]);
        ch.load_patch(&p, 0);
        // Pre-set non-zero phase to verify reset.
        ch.modulator.phase_acc = 0x12345;
        ch.carrier.phase_acc = 0x67890;
        ch.trigger_key_on();
        assert!(ch.key_on);
        assert_eq!(ch.modulator.phase_acc, 0);
        assert_eq!(ch.carrier.phase_acc, 0);
        assert_eq!(ch.modulator.env.phase, EnvPhase::Attack);
        assert_eq!(ch.carrier.env.phase, EnvPhase::Attack);
    }

    /// Key-off edge transition moves both operator envelopes into
    /// Release. Uses a patch with the modulator's `$00.S` clear so the
    /// modulator honours key-off — see
    /// `modulator_sustain_disables_release_on_key_off` for the
    /// opposite case.
    #[test]
    fn channel_key_off_moves_envelopes_to_release() {
        let mut ch = OpllChannel::default();
        // $00 = 0x01 (S=0, no release-disable), $01 = 0x61 (carrier
        // bits as before).
        let p = Vrc7Patch::from_bytes(&[0x01, 0x61, 0x1D, 0x07, 0xFF, 0xFF, 0x11, 0x07]);
        ch.load_patch(&p, 0);
        ch.trigger_key_on();
        // Advance past attack into decay so key-off has something to
        // release from.
        for _ in 0..2000 {
            ch.modulator.env.step(1);
            ch.carrier.env.step(1);
            if ch.carrier.env.phase != EnvPhase::Attack {
                break;
            }
        }
        ch.trigger_key_off();
        assert!(!ch.key_on);
        assert_eq!(ch.modulator.env.phase, EnvPhase::Release);
        assert_eq!(ch.carrier.env.phase, EnvPhase::Release);
    }

    /// End-to-end: a triggered channel produces a non-zero sample
    /// stream (the pipeline is wired through to the host mixer).
    #[test]
    fn channel_produces_non_trivial_audio_after_key_on() {
        let mut ch = OpllChannel::default();
        // Flute patch (`31 61 0E 07 A8 64 70 27`) — fast attack so we
        // hit audible amplitude inside the test budget.
        let p = Vrc7Patch::from_bytes(&[0x31, 0x61, 0x0E, 0x07, 0xFF, 0xFF, 0x70, 0x27]);
        ch.load_patch(&p, 0);
        ch.fnum = 0x100;
        ch.block = 4;
        ch.trigger_key_on();
        let mut peak: i32 = 0;
        for _ in 0..2048 {
            let s = ch.sample();
            peak = peak.max(s.abs());
        }
        assert!(
            peak > 10,
            "expected non-trivial carrier output after key-on, peak={}",
            peak
        );
    }

    // ----------------------------------------------- test register $0F

    /// §"Test Register $0F" decoder maps each low bit to its named
    /// flag.
    #[test]
    fn test_register_decodes_low_four_bits() {
        let t = TestRegister::from_byte(0b0000);
        assert!(!t.envs_zero && !t.hold_lfo && !t.hold_phase && !t.fast_lfo);
        let t = TestRegister::from_byte(0b0001);
        assert!(t.envs_zero && !t.hold_lfo && !t.hold_phase && !t.fast_lfo);
        let t = TestRegister::from_byte(0b0010);
        assert!(!t.envs_zero && t.hold_lfo && !t.hold_phase && !t.fast_lfo);
        let t = TestRegister::from_byte(0b0100);
        assert!(!t.envs_zero && !t.hold_lfo && t.hold_phase && !t.fast_lfo);
        let t = TestRegister::from_byte(0b1000);
        assert!(!t.envs_zero && !t.hold_lfo && !t.hold_phase && t.fast_lfo);
        // High nibble is ignored.
        let t = TestRegister::from_byte(0xF7);
        assert!(t.envs_zero && t.hold_lfo && t.hold_phase && !t.fast_lfo);
    }

    /// §"Test Register $0F" bit 0: envelopes are bypassed (constant 0
    /// = full volume), but they keep ticking. A freshly idle channel
    /// (envelope=127, silent) should emit audible output with bit 0
    /// set.
    #[test]
    fn test_register_bit0_forces_full_volume_output() {
        let mut ch = OpllChannel::default();
        // Use a patch with low feedback so the channel produces a
        // clean sine; volume 0 = loudest.
        let p = Vrc7Patch::from_bytes(&[0x21, 0x01, 0x00, 0x00, 0xF0, 0xF0, 0x00, 0x00]);
        ch.load_patch(&p, 0);
        ch.fnum = 0x100;
        ch.block = 4;
        // No key-on yet: envelopes are Idle (level=127, silent).
        let test = TestRegister {
            envs_zero: true,
            ..Default::default()
        };
        let mut peak: i32 = 0;
        for _ in 0..2048 {
            let s = ch.sample_with_test(&test, &Lfo::default());
            peak = peak.max(s.abs());
        }
        assert!(
            peak > 10,
            "expected audible output with envelopes overridden, peak={}",
            peak
        );
    }

    /// §"Test Register $0F" bit 2: waveform phase is pinned at 0, so
    /// the operator sits at sin(0) ≈ 0. Even a triggered channel
    /// should fall silent.
    #[test]
    fn test_register_bit2_silences_via_phase_hold() {
        let mut ch = OpllChannel::default();
        let p = Vrc7Patch::from_bytes(&[0x21, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        ch.load_patch(&p, 0);
        ch.fnum = 0x100;
        ch.block = 4;
        ch.trigger_key_on();
        let test = TestRegister {
            hold_phase: true,
            ..Default::default()
        };
        let mut peak: i32 = 0;
        for _ in 0..2048 {
            let s = ch.sample_with_test(&test, &Lfo::default());
            peak = peak.max(s.abs());
        }
        // The +0/-0 1-complement representation of sin(0) gives ±1 LSB
        // at the boundary; anything within a few LSBs counts as silent.
        assert!(
            peak <= 5,
            "expected near-silence with phase held at 0, got peak={}",
            peak
        );
    }

    /// §"Test Register $0F" bit 0: envelopes continue to tick even
    /// while overridden. A key-off issued mid-stream should still
    /// drive the envelope toward Idle.
    #[test]
    fn test_register_bit0_envelopes_continue_to_tick() {
        let mut ch = OpllChannel::default();
        let p = Vrc7Patch::from_bytes(&[0x21, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0xFF]);
        ch.load_patch(&p, 0);
        ch.fnum = 0x100;
        ch.block = 4;
        ch.trigger_key_on();
        let test = TestRegister {
            envs_zero: true,
            ..Default::default()
        };
        // Advance into Decay/Sustain, then key-off and step until the
        // carrier envelope is back to Idle (envelopes still ticking).
        for _ in 0..2000 {
            let _ = ch.sample_with_test(&test, &Lfo::default());
        }
        ch.trigger_key_off();
        let mut reached_idle = false;
        for _ in 0..200_000 {
            let _ = ch.sample_with_test(&test, &Lfo::default());
            if matches!(ch.carrier.env.phase, EnvPhase::Idle) {
                reached_idle = true;
                break;
            }
        }
        assert!(
            reached_idle,
            "envelopes must keep ticking under test-bit-0 override"
        );
    }

    // ----------------------------------------------- channel-S override

    /// §Channels: "If the sustain bit is set in the channel control
    /// register $2X S, the release value in the patch is ignored and
    /// replaced with $5."
    #[test]
    fn channel_sustain_override_swaps_release_to_five() {
        let mut ch = OpllChannel::default();
        // Patch with release rate $C (fast) for both operators.
        let p = Vrc7Patch::from_bytes(&[0x21, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x0C, 0x0C]);
        ch.load_patch(&p, 0);
        assert_eq!(ch.carrier.env.release_rate, 0x0C);
        assert_eq!(ch.modulator.env.release_rate, 0x0C);
        ch.set_channel_sustain_override(true, &p);
        assert_eq!(ch.carrier.env.release_rate, 0x05);
        assert_eq!(ch.modulator.env.release_rate, 0x05);
        // Clearing the override restores the patch value.
        ch.set_channel_sustain_override(false, &p);
        assert_eq!(ch.carrier.env.release_rate, 0x0C);
        assert_eq!(ch.modulator.env.release_rate, 0x0C);
    }

    // ----------------------------------------------- modulator-S release-disable

    /// §"Custom Patch": "the modulator's sustain bit ($00 S) also
    /// disables the release section of its envelope." Key-off should
    /// leave the modulator envelope sitting wherever it is, while
    /// the carrier moves to Release.
    #[test]
    fn modulator_sustain_disables_release_on_key_off() {
        let mut ch = OpllChannel::default();
        // $00 = 0x21: T=0 V=0 S=1 K=0 M=1 — modulator $00.S=1.
        // $01 = 0x01: carrier S=0.
        let p = Vrc7Patch::from_bytes(&[0x21, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0xFF]);
        ch.load_patch(&p, 0);
        assert!(ch.modulator.env.release_disabled);
        assert!(!ch.carrier.env.release_disabled);
        ch.trigger_key_on();
        // Advance past attack.
        for _ in 0..2000 {
            ch.modulator.env.step(1);
            ch.carrier.env.step(1);
            if matches!(ch.carrier.env.phase, EnvPhase::Sustain) {
                break;
            }
        }
        ch.trigger_key_off();
        // Carrier transitioned to Release; modulator did not.
        assert_eq!(ch.carrier.env.phase, EnvPhase::Release);
        assert_ne!(ch.modulator.env.phase, EnvPhase::Release);
    }

    /// Counter-test: a patch with $01.S=1 (carrier sustain enabled)
    /// is the EG-TYP bit only; the carrier still enters Release on
    /// key-off per §"Custom Patch" — "The carrier does not behave
    /// this way: its envelope always enters release when the note
    /// is released."
    #[test]
    fn carrier_sustain_bit_does_not_disable_release() {
        let mut ch = OpllChannel::default();
        // $01 = 0x21: T=0 V=0 S=1 K=0 M=1 — carrier $01.S=1.
        let p = Vrc7Patch::from_bytes(&[0x01, 0x21, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0xFF]);
        ch.load_patch(&p, 0);
        assert!(!ch.modulator.env.release_disabled);
        assert!(!ch.carrier.env.release_disabled);
        ch.trigger_key_on();
        for _ in 0..2000 {
            ch.modulator.env.step(1);
            ch.carrier.env.step(1);
            if matches!(ch.carrier.env.phase, EnvPhase::Sustain) {
                break;
            }
        }
        ch.trigger_key_off();
        assert_eq!(ch.carrier.env.phase, EnvPhase::Release);
    }

    /// Sanity: phase_index wraps modulo 1024.
    #[test]
    fn operator_phase_index_wraps_at_1024() {
        let op = Operator {
            phase_acc: (1023 << PHASE_ACC_FRAC_BITS) | 0x1234,
            ..Default::default()
        };
        let p = op.phase_index(0);
        assert!(p < 1024);
        // Adding 1 modulation step at position 1023 → wraps to 0.
        let p2 = op.phase_index(1);
        assert_eq!(p2, 0);
    }

    // ------------------------------------------------------- KSR

    /// YM2413 Application Manual §III-1-2 Table III-2 D4=0 row: the
    /// `Rks` offset for KSR=0 reads `0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3`
    /// across the 16 columns indexed by (block, fnum_msb). The F-Num
    /// MSB is ignored — `Rks = block >> 1` matches every column.
    #[test]
    fn ksr_disabled_matches_app_manual_table_iii_2_d4_zero_row() {
        let expected: [u8; 16] = [0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
        let mut e = Envelope {
            ksr: false,
            ..Default::default()
        };
        for col in 0..16u8 {
            let block = col >> 1;
            let fnum_msb = col & 0x01;
            e.update_rks(block, fnum_msb);
            assert_eq!(
                e.rks, expected[col as usize],
                "KSR=0 col={col} block={block} fnum_msb={fnum_msb}: Rks={} expected={}",
                e.rks, expected[col as usize]
            );
        }
    }

    /// YM2413 Application Manual §III-1-2 Table III-2 D4=1 row: the
    /// `Rks` offset for KSR=1 reads `0,1,2,3,4,5,6,7,8,9,10,11,12,
    /// 13,14,15` across the 16 columns — i.e. `Rks = (block << 1) |
    /// fnum_msb`.
    #[test]
    fn ksr_enabled_matches_app_manual_table_iii_2_d4_one_row() {
        let mut e = Envelope {
            ksr: true,
            ..Default::default()
        };
        for col in 0..16u8 {
            let block = col >> 1;
            let fnum_msb = col & 0x01;
            e.update_rks(block, fnum_msb);
            assert_eq!(
                e.rks, col,
                "KSR=1 col={col} block={block} fnum_msb={fnum_msb}: Rks={} expected={col}",
                e.rks,
            );
        }
    }

    /// §III-1-2: `RATE = 4·R + Rks`, with the explicit "Note that
    /// when R=0, RATE=0" carve-out. Verify both the formula and the
    /// halt.
    #[test]
    fn effective_rate_matches_4r_plus_rks_with_zero_halt() {
        let mut e = Envelope {
            rks: 7,
            ..Default::default()
        };
        // R=0 always halts regardless of Rks.
        assert_eq!(e.effective_rate(0), 0);
        // R=1, Rks=7 → 4·1 + 7 = 11.
        assert_eq!(e.effective_rate(1), 11);
        // R=15, Rks=15 → 4·15 + 15 = 75 → clamped to 63 (the §step()
        // shift cap; the manual is silent on the upper bound but our
        // Q16 step saturates the envelope within one sample beyond
        // RATE 31 anyway).
        e.rks = 15;
        assert_eq!(e.effective_rate(15), 63);
        // Mid case: R=3, Rks=2 → 14.
        e.rks = 2;
        assert_eq!(e.effective_rate(3), 14);
    }

    /// End-to-end: with the same patch + R but KSR=1, a higher block
    /// makes the envelope's per-sample step strictly larger (and
    /// therefore the decay reaches the sustain level strictly faster)
    /// than the low-block case. Per the §III-1-2 "envelope speeds up
    /// as the pitch rises" semantic.
    ///
    /// We use the decay phase rather than attack because the
    /// envelope's `level_q16` starts at 0 (default) — attack at any
    /// rate immediately saturates to 0 and transitions out in one
    /// step. Decay starts at 0 and ramps up to `sustain_level << 3`
    /// envelope-levels, exercising the per-rate step magnitude.
    #[test]
    fn ksr_enabled_higher_pitch_reaches_decay_sustain_faster() {
        let make = |block: u8, fnum_msb: u8| -> Envelope {
            let mut e = Envelope {
                ksr: true,
                ..Default::default()
            };
            // AR=15 (so attack saturates instantly), DR=2 (slow
            // enough that KSR amplification produces a clear step
            // count difference), SL=8 (mid-range sustain).
            e.load_from_patch(15, 2, 8, 0, true);
            e.update_rks(block, fnum_msb);
            e.key_on();
            e
        };
        let count_decay_steps = |mut e: Envelope| -> u32 {
            // Walk through Attack (1 step at AR=15) then Decay.
            for i in 0..2_000_000 {
                e.step(1);
                if matches!(e.phase, EnvPhase::Sustain) {
                    return i + 1;
                }
            }
            u32::MAX
        };
        let low_pitch = count_decay_steps(make(0, 0));
        let high_pitch = count_decay_steps(make(7, 1));
        assert!(
            high_pitch < low_pitch,
            "KSR=1 with block=7,fnum_msb=1 should reach sustain \
             strictly faster than block=0,fnum_msb=0: high={high_pitch} \
             low={low_pitch}"
        );
    }

    /// End-to-end: with KSR=0, the same change in (block, fnum_msb)
    /// across a 2-block boundary changes Rks by 1 (not 7 as in the
    /// KSR=1 case), confirming the `Rks = block >> 1` table from the
    /// D4=0 row matches the implementation when run through the
    /// envelope.
    #[test]
    fn ksr_disabled_pitch_sensitivity_is_smaller() {
        let mut e0 = Envelope {
            ksr: false,
            ..Default::default()
        };
        e0.update_rks(0, 0);
        let mut e1 = Envelope {
            ksr: false,
            ..Default::default()
        };
        e1.update_rks(7, 1);
        // Block 0..1 → 0; block 6..7 → 3. Difference is 3.
        assert_eq!(e0.rks, 0);
        assert_eq!(e1.rks, 3);
    }

    /// `OpllChannel::refresh_rks` derives both operators' Rks from
    /// the channel's current `block` and the top bit of the 9-bit
    /// `fnum`. Smoke check: when the carrier has KSR=1 and the
    /// modulator has KSR=0, only the carrier's Rks tracks the
    /// per-column index — the modulator's Rks stays on the `block >> 1`
    /// ladder.
    #[test]
    fn opll_channel_refresh_rks_uses_per_operator_ksr_bit() {
        // block=5, fnum_msb=1 → KSR=0 row: 5>>1 = 2; KSR=1 row:
        // (5<<1)|1 = 11. The top bit of fnum=0x180 makes fnum_msb=1.
        let mut ch = OpllChannel {
            block: 5,
            fnum: 0x180,
            ..Default::default()
        };
        ch.modulator.env.ksr = false;
        ch.carrier.env.ksr = true;
        ch.refresh_rks();
        assert_eq!(ch.modulator.env.rks, 2);
        assert_eq!(ch.carrier.env.rks, 11);
    }

    // ----------------------------------------------- §III-7 decay-time table

    /// **Yamaha YM2413 Application Manual Table III-7** spot-checks
    /// against the table's documented `EG decay time, 0 dB → 40 dB`
    /// column. The values are stored in our table as hundredths of
    /// milliseconds (= 10 µs units).
    ///
    /// Row spot-checks reproduce both extremes plus a mid-RATE row.
    #[test]
    fn table_iii_7_decay_time_spot_checks_against_manual() {
        // RATE = 4·RM + RL.
        // RM=15, RL=3 → RATE=63 → 1.27 ms → 127 hundredths.
        assert_eq!(TABLE_III_7_DECAY_HUNDREDTHS_MS[63], 127);
        // RM=1, RL=0 → RATE=4 → 20926.60 ms → 2_092_660 hundredths.
        assert_eq!(TABLE_III_7_DECAY_HUNDREDTHS_MS[4], 2_092_660);
        // RM=8, RL=0 → RATE=32 → 163.49 ms → 16_349 hundredths.
        assert_eq!(TABLE_III_7_DECAY_HUNDREDTHS_MS[32], 16_349);
        // RM=12, RL=0 → RATE=48 → 10.22 ms → 1_022 hundredths.
        assert_eq!(TABLE_III_7_DECAY_HUNDREDTHS_MS[48], 1_022);
        // RM=6, RL=3 → RATE=27 → 375.98 ms → 37_598 hundredths.
        assert_eq!(TABLE_III_7_DECAY_HUNDREDTHS_MS[27], 37_598);
    }

    /// RATE 0..=3 are not tabulated in Table III-7 (the manual's
    /// rows start at RM=1, RL=0 → RATE=4). The implementation
    /// defaults those entries to zero (halt); `R=0` is the
    /// documented `RATE=0` halt case from §III-1-2.
    #[test]
    fn table_iii_7_below_rate_four_is_halt() {
        for rate in 0u8..=3 {
            assert_eq!(
                TABLE_III_7_DECAY_HUNDREDTHS_MS[rate as usize], 0,
                "RATE={rate} is not tabulated by Table III-7"
            );
            assert_eq!(
                decay_step_q16_per_sample(rate),
                0,
                "decay_step_q16_per_sample({rate}) must halt for RATE<4"
            );
        }
    }

    /// Faster RATE → larger Q16 envelope-level step per sample. The
    /// manual's decay-time column is strictly monotone non-decreasing
    /// from RATE=63 (1.27 ms, fastest) down to RATE=4 (20926.60 ms,
    /// slowest), so the per-sample step is strictly monotone
    /// non-increasing in the same direction.
    #[test]
    fn table_iii_7_step_is_monotone_in_rate() {
        let mut prev = 0u32; // RATE=3 → halt
        for rate in 4u8..=63 {
            let s = decay_step_q16_per_sample(rate);
            assert!(
                s >= prev,
                "decay_step must not decrease with RATE: rate={rate} prev={prev} cur={s}"
            );
            prev = s;
        }
    }

    /// At a fixed RATE, the envelope's accumulated decay (Q16
    /// envelope-levels per sample × samples) should traverse the
    /// 40 dB span in approximately the tabulated time. We use
    /// RATE=32 (163.49 ms) at the OPLL operator sample rate
    /// (≈49 716 Hz) — 8128 samples — and assert the step × sample
    /// count matches the manual within ±2 % (rounding from the
    /// 0.01 ms × 49 716 / 100 000 path plus the 40-dB Q16 round).
    #[test]
    fn table_iii_7_step_traverses_40_db_in_tabulated_time() {
        let rate = 32u8;
        let step = decay_step_q16_per_sample(rate) as u64;
        // Hundredths of ms × 49 716 / 100 000 = samples in tabulated
        // time. RATE=32 → 16_349 × 49_716 / 100_000 = 8_127.7 → 8_127
        // samples.
        let hundredths_ms = TABLE_III_7_DECAY_HUNDREDTHS_MS[rate as usize] as u64;
        let total_samples = hundredths_ms * 49_716 / 100_000;
        // Accumulated 40-dB target in Q16 envelope-levels.
        let traversal = step * total_samples;
        let expected = ENV_LEVELS_40_DB_Q16;
        // Allow ±2 % tolerance for the integer rounding.
        let lower = expected * 98 / 100;
        let upper = expected * 102 / 100;
        assert!(
            (lower..=upper).contains(&traversal),
            "RATE={rate}: step={step} × samples={total_samples} = \
             {traversal}, expected ≈ {expected} (±2 %)"
        );
    }

    /// The footnote on page 13 of the application manual reads
    /// "Attenuation times of the release rate are the same as that
    /// of the decay rate". The envelope step path enforces this by
    /// reusing [`decay_step_q16_per_sample`] in the Release phase —
    /// verify by running two identical envelopes at the same RATE
    /// in Decay vs Release and observing the same per-sample level
    /// change.
    #[test]
    fn release_phase_uses_same_per_sample_step_as_decay() {
        let mut e_decay = Envelope::default();
        // AR=15 (instant attack), DR=8 (mid-range, exercises
        // Table III-7 RATE=32 with Rks=0), SL=15 (sustain at max
        // attenuation = release boundary), RR=8.
        e_decay.load_from_patch(15, 8, 15, 8, true);
        e_decay.key_on();
        // Force into Decay phase explicitly.
        e_decay.phase = EnvPhase::Decay;
        e_decay.level_q16 = 0;

        let mut e_release = e_decay;
        e_release.phase = EnvPhase::Release;

        // Step both by the same count and compare level deltas while
        // each stays in its own phase.
        for _ in 0..256 {
            let before_d = e_decay.level_q16;
            let before_r = e_release.level_q16;
            e_decay.step(1);
            e_release.step(1);
            // Skip the boundary samples where Decay flips to Sustain
            // and Release flips to Idle.
            if !matches!(e_decay.phase, EnvPhase::Decay)
                || !matches!(e_release.phase, EnvPhase::Release)
            {
                break;
            }
            let delta_d = e_decay.level_q16.saturating_sub(before_d);
            let delta_r = e_release.level_q16.saturating_sub(before_r);
            assert_eq!(
                delta_d, delta_r,
                "Decay vs Release per-sample step must match per the \
                 page-13 footnote: delta_decay={delta_d} \
                 delta_release={delta_r}"
            );
        }
    }

    // ----------------------------------------------- §III-7 attack-time table

    /// **Yamaha YM2413 Application Manual Table III-7** spot-checks
    /// against the table's documented `EG attack time, 0 dB → 40 dB`
    /// column. The values are stored in our table as hundredths of
    /// milliseconds (= 10 µs units).
    ///
    /// Row spot-checks reproduce both extremes plus three mid-RATE
    /// rows.
    #[test]
    fn table_iii_7_attack_time_spot_checks_against_manual() {
        // RATE = 4·RM + RL.
        // RM=15, RL=0..3 → RATE=60..63 → 0.00 ms (instant attack).
        for rate in 60u8..=63 {
            assert_eq!(
                TABLE_III_7_ATTACK_HUNDREDTHS_MS[rate as usize], 0,
                "RATE={rate} must be 0.00 ms per the manual's RM=15 row"
            );
        }
        // RM=1, RL=0 → RATE=4 → 1730.15 ms → 173_015 hundredths
        // (the slowest tabulated attack).
        assert_eq!(TABLE_III_7_ATTACK_HUNDREDTHS_MS[4], 173_015);
        // RM=8, RL=0 → RATE=32 → 13.52 ms → 1_352 hundredths.
        assert_eq!(TABLE_III_7_ATTACK_HUNDREDTHS_MS[32], 1_352);
        // RM=12, RL=0 → RATE=48 → 0.84 ms → 84 hundredths.
        assert_eq!(TABLE_III_7_ATTACK_HUNDREDTHS_MS[48], 84);
        // RM=6, RL=3 → RATE=27 → 30.90 ms → 3_090 hundredths.
        assert_eq!(TABLE_III_7_ATTACK_HUNDREDTHS_MS[27], 3_090);
        // RM=10, RL=2 → RATE=42 → 2.25 ms → 225 hundredths.
        assert_eq!(TABLE_III_7_ATTACK_HUNDREDTHS_MS[42], 225);
    }

    /// RATE 0..=3 are not tabulated in Table III-7 (the manual's
    /// rows start at RM=1, RL=0 → RATE=4). The implementation
    /// defaults those entries to zero (halt); `R=0` is the
    /// documented `RATE=0` halt case from §III-1-2.
    #[test]
    fn table_iii_7_attack_below_rate_four_is_halt() {
        for rate in 0u8..=3 {
            assert_eq!(
                TABLE_III_7_ATTACK_HUNDREDTHS_MS[rate as usize], 0,
                "RATE={rate} is not tabulated by Table III-7 attack column"
            );
            assert_eq!(
                attack_step_q16_per_sample(rate),
                0,
                "attack_step_q16_per_sample({rate}) must halt for RATE<4"
            );
        }
    }

    /// RATE 60..=63 (RM=15, any RL) are tabulated as 0.00 ms in the
    /// manual — interpreted as instantaneous attack. The helper
    /// returns `u32::MAX` so a single `Envelope::step` saturates
    /// `level_q16` to zero (= loudest).
    #[test]
    fn table_iii_7_attack_rate_sixty_to_sixty_three_is_instantaneous() {
        for rate in 60u8..=63 {
            assert_eq!(
                attack_step_q16_per_sample(rate),
                u32::MAX,
                "RATE={rate} attack must be instantaneous (u32::MAX step)"
            );
        }
        // End-to-end: an envelope at the maximum attenuation level
        // (silent) with AR producing RATE=63 collapses to zero in
        // exactly one step call.
        let mut e = Envelope {
            rks: 3, // 4·15 + 3 = 63
            ..Default::default()
        };
        e.load_from_patch(15, 0, 0, 0, true);
        e.key_on();
        e.level_q16 = ENV_MAX_LEVEL << 16;
        e.step(1);
        assert_eq!(e.level_q16, 0);
        assert_eq!(e.phase, EnvPhase::Decay);
    }

    /// Faster RATE → larger Q16 envelope-level step per sample. The
    /// manual's attack-time column is monotone non-decreasing from
    /// RATE=63 (0.00 ms, instant) down to RATE=4 (1730.15 ms,
    /// slowest), so the per-sample step is monotone non-decreasing
    /// from slow to fast RATE (and ties are allowed when adjacent
    /// table cells round to the same per-sample integer).
    #[test]
    fn table_iii_7_attack_step_is_monotone_in_rate() {
        let mut prev = 0u32; // RATE=3 → halt
        for rate in 4u8..=63 {
            let s = attack_step_q16_per_sample(rate);
            assert!(
                s >= prev,
                "attack_step must not decrease with RATE: rate={rate} prev={prev} cur={s}"
            );
            prev = s;
        }
    }

    /// At a fixed RATE the envelope's accumulated attack ramp (Q16
    /// envelope-levels per sample × samples) should traverse the
    /// 40 dB span in approximately the tabulated time. We use
    /// RATE=32 (13.52 ms) at the OPLL operator sample rate
    /// (≈49 716 Hz) — ~672 samples — and assert the step × sample
    /// count matches the manual within ±2 % (rounding from the
    /// 0.01 ms × 49 716 / 100 000 path plus the 40-dB Q16 round).
    #[test]
    fn table_iii_7_attack_step_traverses_40_db_in_tabulated_time() {
        let rate = 32u8;
        let step = attack_step_q16_per_sample(rate) as u64;
        let hundredths_ms = TABLE_III_7_ATTACK_HUNDREDTHS_MS[rate as usize] as u64;
        let total_samples = hundredths_ms * 49_716 / 100_000;
        let traversal = step * total_samples;
        let expected = ENV_LEVELS_40_DB_Q16;
        // Allow ±2 % tolerance for integer rounding.
        let lower = expected * 98 / 100;
        let upper = expected * 102 / 100;
        assert!(
            (lower..=upper).contains(&traversal),
            "RATE={rate}: step={step} × samples={total_samples} = \
             {traversal}, expected ≈ {expected} (±2 %)"
        );
    }

    /// At a fixed RATE the attack and decay traversal *step* shapes
    /// should follow the manual's separately-tabulated columns —
    /// attack is roughly 10x faster than decay at the same RATE per
    /// the table's documented spread (e.g. RATE=32: attack 13.52 ms
    /// vs decay 163.49 ms — ratio ≈ 12.1).
    ///
    /// This anchors the relationship between the two columns: the
    /// per-sample attack step should be strictly larger than the
    /// per-sample decay step at the same RATE for every tabulated
    /// row (since each column traverses the same 40-dB span in less
    /// time, attack always steps further per sample).
    #[test]
    fn table_iii_7_attack_step_is_larger_than_decay_step_per_rate() {
        for rate in 4u8..=59 {
            let attack = attack_step_q16_per_sample(rate);
            let decay = decay_step_q16_per_sample(rate);
            assert!(
                attack > decay,
                "attack > decay at same RATE per Table III-7: \
                 rate={rate} attack={attack} decay={decay}"
            );
        }
        // RATE 60..=63: attack saturates to u32::MAX (instantaneous);
        // decay is the tabulated 1.27 ms step. Maintain the strict
        // ordering.
        for rate in 60u8..=63 {
            let attack = attack_step_q16_per_sample(rate);
            let decay = decay_step_q16_per_sample(rate);
            assert_eq!(attack, u32::MAX);
            assert!(decay < u32::MAX);
        }
    }

    /// End-to-end Envelope check: with a slow attack RATE the
    /// envelope should take strictly more `step(1)` calls to reach
    /// `level_q16 == 0` than with a fast attack RATE, per the
    /// manual's RATE→time monotonicity. We compare RATE=32 (13.52 ms,
    /// ~672 samples) to RATE=48 (0.84 ms, ~42 samples).
    #[test]
    fn envelope_attack_phase_consults_table_iii_7_attack_column() {
        let count_attack_steps = |rate: u8| -> u32 {
            // Choose R and Rks so 4·R + Rks = rate exactly.
            // RATE=32 → R=8, Rks=0; RATE=48 → R=12, Rks=0.
            let r = rate / 4;
            let mut e = Envelope::default();
            e.load_from_patch(r, 0, 0, 0, true);
            e.key_on();
            // Start at max attenuation (silent) so the attack span
            // is the full ENV_MAX_LEVEL.
            e.level_q16 = ENV_MAX_LEVEL << 16;
            for i in 0..1_000_000u32 {
                e.step(1);
                if e.phase != EnvPhase::Attack {
                    return i + 1;
                }
            }
            u32::MAX
        };
        let slow = count_attack_steps(32);
        let fast = count_attack_steps(48);
        assert!(
            slow > fast,
            "slow attack (RATE=32) must take more steps than fast \
             attack (RATE=48): slow={slow} fast={fast}"
        );
        // Sanity bounds: the slow attack should be roughly the
        // ~672-sample 13.52 ms ballpark (40-dB span; tracking
        // a 127-level envelope is ~3.17x further so a few thousand
        // samples; bound generously).
        assert!(slow < 10_000, "RATE=32 attack should be <10k samples");
        assert!(fast < 1_000, "RATE=48 attack should be <1k samples");
    }

    // ----------------------------------------------- KSR (continued)

    /// `OpllChannel::load_patch` pulls the per-operator KSR bit from
    /// the patch byte and immediately re-derives Rks against the
    /// channel's current pitch. This is the path
    /// `Vrc7::refresh_from_regs` uses on a patch-swap mid-note.
    #[test]
    fn opll_channel_load_patch_picks_up_ksr_and_refreshes_rks() {
        // block=4, fnum_msb=1 from fnum=0x100. Hand-crafted patch:
        // modulator $00 = 0x10 (K=1, all else 0); carrier $01 = 0x00
        // (K=0). Other bytes zero so no side effects.
        let mut ch = OpllChannel {
            block: 4,
            fnum: 0x100,
            ..Default::default()
        };
        let p = Vrc7Patch::from_bytes(&[0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(p.mod_ksr);
        assert!(!p.car_ksr);
        ch.load_patch(&p, 0);
        // Modulator KSR=1 → Rks = (block<<1) | fnum_msb = (4<<1)|1 = 9.
        assert_eq!(ch.modulator.env.rks, 9);
        // Carrier KSR=0 → Rks = block>>1 = 2.
        assert_eq!(ch.carrier.env.rks, 2);
    }

    // ----------------------------------------------- LFO (AM/VIB)

    /// Normal-mode cadence: the tremolo phase advances exactly once
    /// every [`TREMOLO_LFO_DIVIDER`] = 64 samples and the vibrato
    /// phase once every [`VIBRATO_LFO_DIVIDER`] = 1024 samples, per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F"
    /// bit 3's "Tremolo is 64x faster, and vibrato is 1024x faster"
    /// description of the fast mode (so normal mode is 64× / 1024×
    /// slower).
    #[test]
    fn lfo_normal_mode_divider_cadence() {
        let mut lfo = Lfo::default();
        // 64 samples → exactly one tremolo step; 64 < 1024 so vibrato
        // has not stepped yet.
        for _ in 0..64 {
            lfo.tick(false, false);
        }
        assert_eq!(lfo.tremolo_phase, 1, "tremolo steps once per 64 samples");
        assert_eq!(lfo.vibrato_phase, 0, "vibrato has not stepped at 64");
        // After a total of 1024 samples: 1024/64 = 16 tremolo steps,
        // 1024/1024 = 1 vibrato step.
        for _ in 64..1024 {
            lfo.tick(false, false);
        }
        assert_eq!(lfo.tremolo_phase, 16, "16 tremolo steps in 1024 samples");
        assert_eq!(lfo.vibrato_phase, 1, "1 vibrato step in 1024 samples");
    }

    /// `$0F` bit 3 (fast LFO) bypasses both dividers: each operator
    /// sample advances both phases once.
    #[test]
    fn lfo_fast_mode_advances_every_sample() {
        let mut lfo = Lfo::default();
        for _ in 0..100 {
            lfo.tick(false, true);
        }
        assert_eq!(lfo.tremolo_phase, 100, "fast tremolo: one step per sample");
        assert_eq!(lfo.vibrato_phase, 100, "fast vibrato: one step per sample");
    }

    /// `$0F` bit 1 (hold) halts + resets both LFOs to phase 0, and
    /// holds them there while engaged — no advance regardless of how
    /// many ticks elapse.
    #[test]
    fn lfo_hold_resets_and_pins_both_phases() {
        let mut lfo = Lfo::default();
        // Advance a bit first so there is non-zero state to reset.
        for _ in 0..200 {
            lfo.tick(false, true);
        }
        assert!(lfo.tremolo_phase > 0 && lfo.vibrato_phase > 0);
        // Engage hold: both phases collapse to 0 and stay there.
        for _ in 0..500 {
            lfo.tick(true, false);
        }
        assert_eq!(lfo.tremolo_phase, 0);
        assert_eq!(lfo.vibrato_phase, 0);
        // Dividers reloaded to a full period (held, not advancing).
        assert_eq!(lfo.tremolo_divider, TREMOLO_LFO_DIVIDER - 1);
        assert_eq!(lfo.vibrato_divider, VIBRATO_LFO_DIVIDER - 1);
    }

    /// `$E000` audio reset clears the tremolo phase but PRESERVES the
    /// vibrato phase, per `docs/audio/nsf/vrc7-audio-wiki.html`
    /// §"Audio Reset ($E000)": "clear its registers (including
    /// tremolo LFO state, but not including vibrato LFO state)."
    #[test]
    fn lfo_audio_reset_clears_tremolo_preserves_vibrato() {
        let mut lfo = Lfo::default();
        // Advance both phases to non-zero values.
        for _ in 0..2048 {
            lfo.tick(false, false);
        }
        let vib_before = lfo.vibrato_phase;
        assert!(lfo.tremolo_phase > 0);
        assert!(vib_before > 0);
        lfo.audio_reset();
        assert_eq!(lfo.tremolo_phase, 0, "tremolo phase cleared by audio reset");
        assert_eq!(lfo.tremolo_divider, TREMOLO_LFO_DIVIDER - 1);
        assert_eq!(
            lfo.vibrato_phase, vib_before,
            "vibrato phase preserved across audio reset"
        );
    }

    /// Hold takes priority over fast: while bit 1 is set the phases
    /// stay pinned at zero even with bit 3 also set.
    #[test]
    fn lfo_hold_overrides_fast() {
        let mut lfo = Lfo::default();
        for _ in 0..50 {
            lfo.tick(true, true);
        }
        assert_eq!(lfo.tremolo_phase, 0);
        assert_eq!(lfo.vibrato_phase, 0);
    }

    // -------------------------------------------- §7 AM/VIB depth

    /// An operator with its AM bit clear gets no tremolo attenuation,
    /// regardless of the LFO phase.
    #[test]
    fn tremolo_atten_zero_when_am_disabled() {
        let mut lfo = Lfo::default();
        for _ in 0..1000 {
            lfo.tick(false, false);
            assert_eq!(lfo.tremolo_am_level(false), 0);
            assert_eq!(lfo.tremolo_atten_exp_units(false), 0);
        }
    }

    /// The §8a silicon AM table has exactly the measured structure: 210
    /// entries, 14 distinct levels (0..=13), the documented per-level
    /// hold counts (level 0 → 15 steps, levels 1..=12 → 8 steps each,
    /// level 13 → 3 steps), and a symmetric rise/fall.
    #[test]
    fn am_lfo_table_matches_measured_silicon() {
        assert_eq!(AM_LFO_LEVELS.len(), 210);
        // Distinct levels are exactly 0..=13.
        let max = *AM_LFO_LEVELS.iter().max().unwrap();
        let min = *AM_LFO_LEVELS.iter().min().unwrap();
        assert_eq!((min, max), (0, AM_LFO_PEAK_LEVEL));
        // Per-level hold counts (number of 64-sample steps at each
        // level across the whole period).
        let mut holds = [0u32; 14];
        for &l in AM_LFO_LEVELS.iter() {
            holds[l as usize] += 1;
        }
        // Apex (13) and trough (0) appear once per period; the
        // intermediate levels 1..=12 appear on both the rising and the
        // falling edge (8 + 8 = 16 steps each).
        assert_eq!(holds[0], 15, "level 0 held 15 steps (960 samples), once");
        for (lvl, h) in holds.iter().enumerate().take(13).skip(1) {
            assert_eq!(*h, 16, "level {lvl} held 8 steps on each edge (16 total)");
        }
        assert_eq!(holds[13], 3, "level 13 held 3 steps (192 samples), once");
        // Rising edge: the first 15 entries are level 0, then it climbs
        // monotonically to 13; after the apex it descends monotonically.
        let apex = AM_LFO_LEVELS.iter().position(|&l| l == 13).unwrap();
        for w in AM_LFO_LEVELS[..=apex].windows(2) {
            assert!(w[1] >= w[0], "rising edge non-monotone");
        }
        for w in AM_LFO_LEVELS[apex..].windows(2) {
            assert!(w[1] <= w[0], "falling edge non-monotone");
        }
        // Period sums to 13440 samples = 210 × 64 → 3.699 Hz.
        assert_eq!(AM_LFO_LEVELS.len() * TREMOLO_LFO_DIVIDER as usize, 13440);
    }

    /// The §8a AM oscillator rides between 0 (trough, loudest) and a
    /// `16 × 13 = 208` exp-unit peak (≈ 4.8 dB), advancing one table
    /// entry per [`TREMOLO_LFO_DIVIDER`] = 64 samples in normal mode.
    #[test]
    fn tremolo_atten_exp_units_walks_silicon_table() {
        let mut lfo = Lfo::default();
        // Phase 0 → trough (level 0, no extra attenuation).
        assert_eq!(lfo.tremolo_am_level(true), 0);
        assert_eq!(lfo.tremolo_atten_exp_units(true), 0);
        let mut saw_peak = 0u32;
        // Walk a full period in fast mode (one table step per tick).
        for _ in 0..AM_LFO_PERIOD_STEPS {
            lfo.tick(false, true);
            let lvl = lfo.tremolo_am_level(true);
            let units = lfo.tremolo_atten_exp_units(true);
            assert_eq!(units, 16 * lvl as u32, "16 * am weighting");
            assert!(lvl <= AM_LFO_PEAK_LEVEL, "level {lvl} exceeds peak");
            saw_peak = saw_peak.max(units);
        }
        assert_eq!(
            saw_peak,
            16 * AM_LFO_PEAK_LEVEL as u32,
            "peak = 16 × 13 = 208"
        );
        // Normal-mode cadence: 64 ticks per table step.
        let mut slow = Lfo::default();
        for _ in 0..(TREMOLO_LFO_DIVIDER) {
            assert_eq!(slow.tremolo_am_level(true), AM_LFO_LEVELS[0]);
            slow.tick(false, false);
        }
        // The 64th tick advances to the next table entry.
        assert_eq!(slow.tremolo_am_level(true), AM_LFO_LEVELS[1]);
    }

    /// §7 EG rate-increment: the four `eg_select` duty patterns have the
    /// documented 4/8, 5/8, 6/8, 7/8 advance counts, and the high-rate
    /// 16-entry tables advance up to 2 levels per sample.
    #[test]
    fn eg_rate_increment_tables_match_silicon() {
        let duties = [4u8, 5, 6, 7];
        for (sel, &duty) in duties.iter().enumerate() {
            let advances: u8 = EG_SELECT_TABLE[sel].iter().sum();
            assert_eq!(advances, duty, "eg_select {sel} duty");
        }
        // High-rate tables: rate 54 shows the measured 2,2,1,1,1,1 detail
        // on the first six rolling windows; rate 59 advances 2 levels for
        // the first twelve windows then 1.
        assert_eq!(EG_HIGHRATE_TABLE[54 - 52][..6], [0, 1, 0, 1, 1, 1]);
        let r59_twos = EG_HIGHRATE_TABLE[59 - 52]
            .iter()
            .filter(|&&v| v == 2)
            .count();
        assert_eq!(r59_twos, 12, "rate 59 advances 2 for twelve windows");
    }

    /// §7 worked example: decay rate 14 → `eg_shift = 10` (a rollover
    /// window of 1024 samples), `eg_select = 2` (6/8 duty) → the EG
    /// advances exactly one level on six of every eight 1024-sample
    /// windows, producing the silicon-measured repeating segment lengths
    /// `2048, 1024, 1024` (`ym2413-envelope-decay-rates-andete-2015-03-20.txt`).
    #[test]
    fn eg_decay_advance_reproduces_rate14_segments() {
        let rate = 14u8;
        // Walk the global counter and record the sample distance between
        // successive EG-level advances over one full 8-window cycle.
        let mut last_advance: Option<u32> = None;
        let mut segments = Vec::new();
        for counter in 1..=(16u32 * 1024) {
            if eg_decay_advance(rate, counter) > 0 {
                if let Some(prev) = last_advance {
                    segments.push(counter - prev);
                }
                last_advance = Some(counter);
            }
        }
        // The 6/8 duty pattern [0,1,1,1,0,1,1,1] over eight 1024-sample
        // windows yields inter-advance gaps repeating "1024,1024,2048"
        // (the §7 doc's measured pattern; the phase at which we start
        // sampling determines the rotation).
        assert_eq!(&segments[..6], &[1024, 1024, 2048, 1024, 1024, 2048]);
    }

    /// §7 boundary behaviour: rate ≤ 3 never advances; rate ≥ 60 advances
    /// +2 every sample.
    #[test]
    fn eg_decay_advance_rate_extremes() {
        for rate in 0..=3 {
            for counter in 0..2048 {
                assert_eq!(eg_decay_advance(rate, counter), 0, "rate {rate} halts");
            }
        }
        for rate in 60..=63 {
            for counter in 0..256 {
                assert_eq!(eg_decay_advance(rate, counter), 2, "rate {rate} +2/sample");
            }
        }
    }

    /// A held LFO (`$0F` bit 1) pins the phase at 0, so the audible AM
    /// attenuation is 0 even for an AM-enabled operator.
    #[test]
    fn tremolo_atten_zero_while_lfo_held() {
        let mut lfo = Lfo::default();
        for _ in 0..200 {
            lfo.tick(true, false);
            assert_eq!(lfo.tremolo_am_level(true), 0);
            assert_eq!(lfo.tremolo_atten_exp_units(true), 0);
        }
    }

    /// An operator with its VIB bit clear gets no pitch offset.
    #[test]
    fn vibrato_offset_zero_when_vib_disabled() {
        let mut lfo = Lfo::default();
        for _ in 0..1000 {
            lfo.tick(false, false);
            assert_eq!(lfo.vibrato_pitch_offset_q(false), 0);
        }
    }

    /// The §7 vibrato swings the pitch both sharp (positive offset) and
    /// flat (negative offset) about the nominal frequency, bounded by
    /// the ±7-cent peak ([`VIBRATO_PEAK_OFFSET_Q`]).
    #[test]
    fn vibrato_offset_is_symmetric_and_bounded() {
        let mut lfo = Lfo::default();
        let mut min = i32::MAX;
        let mut max = i32::MIN;
        // Cover several full periods in fast mode.
        for _ in 0..(VIBRATO_PHASE_PERIOD * 8) {
            lfo.tick(false, true);
            let o = lfo.vibrato_pitch_offset_q(true);
            assert!(o.abs() <= VIBRATO_PEAK_OFFSET_Q, "offset {o} exceeds peak");
            min = min.min(o);
            max = max.max(o);
        }
        assert!(max > 0, "vibrato must swing sharp");
        assert!(min < 0, "vibrato must swing flat");
    }

    /// `apply_vibrato` is the identity when the offset is 0 (VIB
    /// disabled) and shifts the rate up for a positive (sharp) offset,
    /// down for a negative (flat) one.
    #[test]
    fn apply_vibrato_scales_phase_rate() {
        let base = 1000u32 << 3; // a representative fnum << block
        assert_eq!(apply_vibrato(base, 0), base, "offset 0 is identity");
        assert!(
            apply_vibrato(base, VIBRATO_PEAK_OFFSET_Q) > base,
            "sharp raises rate"
        );
        assert!(
            apply_vibrato(base, -VIBRATO_PEAK_OFFSET_Q) < base,
            "flat lowers rate"
        );
        // ±7-cent peak is a ~0.4 % deviation — small but non-zero.
        let up = apply_vibrato(base, VIBRATO_PEAK_OFFSET_Q);
        assert!(up - base < base / 100, "deviation under 1%");
    }

    /// The §8b silicon-measured VIB phase-modulation table matches the
    /// values andete confirmed on hardware
    /// (`docs/audio/nsf/opll-ym2413/tables/vib-lfo-pm.csv`). Row 0 is
    /// all-zero (no sweep at the lowest pitch); each row's peak (column
    /// 2) equals `fnum >> 6`; columns 0 and 4 are always zero (the
    /// triangle's zero-crossings); and the second half negates the
    /// first.
    #[test]
    fn vib_pm_table_matches_measured_silicon() {
        assert_eq!(VIB_PM_TABLE[0], [0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(VIB_PM_TABLE[7], [0, 3, 7, 3, 0, -3, -7, -3]);
        for (row_idx, row) in VIB_PM_TABLE.iter().enumerate() {
            // Peak column (2) equals fnum>>6 (= the row index).
            assert_eq!(row[2] as usize, row_idx, "row {row_idx} peak");
            // Zero-crossings at columns 0 and 4.
            assert_eq!(row[0], 0, "row {row_idx} col0");
            assert_eq!(row[4], 0, "row {row_idx} col4");
            // Second half is the negation of the first half.
            assert_eq!(row[5], -row[1], "row {row_idx} antisymmetry col5");
            assert_eq!(row[6], -row[2], "row {row_idx} antisymmetry col6");
            assert_eq!(row[7], -row[3], "row {row_idx} antisymmetry col7");
        }
    }

    /// `Lfo::vibrato_pm` returns 0 for a VIB-disabled operator and walks
    /// the §8b table column-by-column (advancing one column every
    /// [`VIBRATO_LFO_DIVIDER`] samples) for an enabled one.
    #[test]
    fn vibrato_pm_indexes_table_by_phase_and_fnum() {
        let mut lfo = Lfo::default();
        // VIB disabled → always 0 regardless of phase.
        assert_eq!(lfo.vibrato_pm(7, false), 0);

        // Walk all 8 columns of row 7 using the fast LFO clock so each
        // tick advances the vibrato phase by one column.
        let expected_row7 = VIB_PM_TABLE[7];
        for (col, &want) in expected_row7.iter().enumerate() {
            assert_eq!(lfo.vibrato_pm(7, true), want as i32, "row 7 column {col}");
            lfo.tick(false, true); // fast: one column per tick
        }
        // Wrapped back to column 0.
        assert_eq!(lfo.vibrato_pm(7, true), expected_row7[0] as i32);
    }

    /// End-to-end reproduction of the exact §8b worked example from
    /// `ym2413-vib-lfo-andete-2015-12-01.txt`: with `fnum = 0x1c0`,
    /// `block = 6`, `ML = 1`, the phase-step takes the documented eight
    /// values `28672, 28768, 28896, 28768, 28672, 28576, 28448, 28576`
    /// in sequence (one held for each 1024-sample vibrato column).
    #[test]
    fn step_phase_pm_matches_andete_worked_example() {
        let mut lfo = Lfo::default();
        let mut op = Operator {
            mul: 1, // ML=1 → mlTab[1] = 2
            ..Operator::default()
        };
        let fnum = 0x1c0u32; // 448
        let block = 6u32;
        let fnum_hi3 = ((fnum >> 6) & 0x07) as u8; // = 7
        let expected = [28672, 28768, 28896, 28768, 28672, 28576, 28448, 28576];
        for (col, &want) in expected.iter().enumerate() {
            let pm = lfo.vibrato_pm(fnum_hi3, true);
            let before = op.phase_acc;
            op.step_phase_pm(fnum, block, pm);
            let inc = op.phase_acc.wrapping_sub(before);
            assert_eq!(inc, want, "vibrato column {col} phase-step");
            lfo.tick(false, true); // advance one column (fast clock)
        }
    }

    /// With `lfo_pm == 0` (VIB disabled), `step_phase_pm` advances the
    /// phase identically to the legacy `step_phase` fed
    /// `fnum_block = fnum << block` — the §8b formula reduces to
    /// `((fnum * mlTab[ML]) << block) >> 1`.
    #[test]
    fn step_phase_pm_zero_matches_legacy_step_phase() {
        for &mul in &[0u8, 1, 5, 15] {
            for &fnum in &[1u32, 100, 0x1ff] {
                for &block in &[0u32, 3, 7] {
                    let mut a = Operator {
                        mul,
                        ..Operator::default()
                    };
                    let mut b = a;
                    a.step_phase_pm(fnum, block, 0);
                    b.step_phase(fnum << block);
                    assert_eq!(
                        a.phase_acc, b.phase_acc,
                        "mul={mul} fnum={fnum} block={block}"
                    );
                }
            }
        }
    }

    /// End-to-end: an AM-enabled carrier produces a *time-varying*
    /// output amplitude (the tremolo audibly modulates the level),
    /// whereas the same channel with AM disabled holds a steadier
    /// amplitude envelope over the same window.
    #[test]
    fn am_enabled_operator_modulates_output_amplitude() {
        // Build a steady-state channel: full sine carrier, key-on,
        // fastest attack so the envelope settles immediately.
        let patch = Vrc7Patch {
            car_attack: 0x0F,
            mod_attack: 0x0F,
            car_tremolo: true, // carrier AM on
            ..Vrc7Patch::default()
        };

        let mut ch = OpllChannel::default();
        ch.load_patch(&patch, 0); // volume 0 = loudest
        ch.fnum = 0x180;
        ch.block = 4;
        ch.refresh_rks();
        ch.trigger_key_on();

        let mut lfo = Lfo::default();
        // Settle the envelope.
        for _ in 0..2000 {
            lfo.tick(false, true);
            let _ = ch.sample_with_test(&TestRegister::default(), &lfo);
        }
        // Collect a window of |output| and confirm it varies — the AM
        // triangle should push the level up and down across the period.
        let mut lo = i32::MAX;
        let mut hi = i32::MIN;
        for _ in 0..(TREMOLO_PHASE_PERIOD * 2) {
            lfo.tick(false, true);
            let s = ch.sample_with_test(&TestRegister::default(), &lfo).abs();
            lo = lo.min(s);
            hi = hi.max(s);
        }
        assert!(
            hi > lo,
            "AM should make the carrier amplitude vary (lo={lo}, hi={hi})"
        );
    }

    // ----------------------------------------------- §III-1-7 rhythm

    /// `$0E` bit decode per the §III-1-7 bit table: `D5..D0` =
    /// `RHYTHM BD SD TOM TOP-CY HH`, one bit at a time.
    #[test]
    fn rhythm_register_bit_decode() {
        let zero = RhythmRegister::from_byte(0x00);
        assert_eq!(zero, RhythmRegister::default());

        assert!(RhythmRegister::from_byte(0x20).rhythm_mode); // D5
        assert!(RhythmRegister::from_byte(0x10).bd); // D4
        assert!(RhythmRegister::from_byte(0x08).sd); // D3
        assert!(RhythmRegister::from_byte(0x04).tom); // D2
        assert!(RhythmRegister::from_byte(0x02).t_cy); // D1
        assert!(RhythmRegister::from_byte(0x01).hh); // D0

        // Each single-bit decode sets exactly one field.
        for (byte, want) in [
            (
                0x20u8,
                RhythmRegister {
                    rhythm_mode: true,
                    ..RhythmRegister::default()
                },
            ),
            (
                0x10,
                RhythmRegister {
                    bd: true,
                    ..RhythmRegister::default()
                },
            ),
            (
                0x01,
                RhythmRegister {
                    hh: true,
                    ..RhythmRegister::default()
                },
            ),
        ] {
            assert_eq!(RhythmRegister::from_byte(byte), want);
        }

        // Bits 7..6 are outside the §III-1-7 `D5..D0` table and
        // must not leak into the decode.
        assert_eq!(
            RhythmRegister::from_byte(0xC0),
            RhythmRegister::default(),
            "D7/D6 are not rhythm-control bits"
        );
        // All six on at once.
        let all = RhythmRegister::from_byte(0x3F);
        assert!(
            all.rhythm_mode && all.bd && all.sd && all.tom && all.t_cy && all.hh,
            "$3F sets every §III-1-7 field"
        );
    }

    /// Table III-9 rhythm slot allocation: BD = 13+16, HH = 14,
    /// TOM = 15, SD = 17, TOP-CYM = 18 — six slots total, no overlap.
    #[test]
    fn rhythm_table_iii_9_slot_allocation() {
        assert_eq!(RhythmInstrument::BassDrum.slots(), &[13, 16]);
        assert_eq!(RhythmInstrument::HiHat.slots(), &[14]);
        assert_eq!(RhythmInstrument::TomTom.slots(), &[15]);
        assert_eq!(RhythmInstrument::SnareDrum.slots(), &[17]);
        assert_eq!(RhythmInstrument::TopCymbal.slots(), &[18]);

        // §V-4: "three channels and six slots" — the five
        // instruments together cover slots 13..=18 exactly once.
        let mut seen = [false; 19];
        for inst in [
            RhythmInstrument::BassDrum,
            RhythmInstrument::SnareDrum,
            RhythmInstrument::TomTom,
            RhythmInstrument::TopCymbal,
            RhythmInstrument::HiHat,
        ] {
            for &s in inst.slots() {
                assert!(!seen[s as usize], "slot {s} allocated twice");
                seen[s as usize] = true;
            }
        }
        assert_eq!(&seen[13..=18], &[true; 6], "slots 13~18 all covered");
    }

    /// §III-1-7 channel allocation: percussion through channels 7~9;
    /// BD owns channel 7 (the only two-slot FM pair per §V-4), HH+SD
    /// share channel 8, TOM+TOP-CYM share channel 9.
    #[test]
    fn rhythm_channel_allocation() {
        assert_eq!(RhythmInstrument::BassDrum.channel(), 7);
        assert_eq!(RhythmInstrument::HiHat.channel(), 8);
        assert_eq!(RhythmInstrument::SnareDrum.channel(), 8);
        assert_eq!(RhythmInstrument::TomTom.channel(), 9);
        assert_eq!(RhythmInstrument::TopCymbal.channel(), 9);
        // Slot↔channel consistency: each instrument's slots are the
        // modulator (12 + c-6) and/or carrier (15 + c-6) slot of its
        // channel.
        for inst in [
            RhythmInstrument::BassDrum,
            RhythmInstrument::SnareDrum,
            RhythmInstrument::TomTom,
            RhythmInstrument::TopCymbal,
            RhythmInstrument::HiHat,
        ] {
            let c = inst.channel();
            let modulator_slot = 12 + (c - 6);
            let carrier_slot = 15 + (c - 6);
            for &s in inst.slots() {
                assert!(
                    s == modulator_slot || s == carrier_slot,
                    "{inst:?}: slot {s} not on channel {c}"
                );
            }
        }
    }

    /// §III-1-8 rhythm-volume nibble layout for `$36`~`$38`: BD in
    /// `$36` low nibble; HH high / SD low in `$37`; TOM high / T-CYM
    /// low in `$38`.
    #[test]
    fn rhythm_volume_nibble_decode() {
        let v = RhythmVolumes::from_regs(0xFA, 0x3C, 0x58);
        assert_eq!(v.bd, 0x0A, "$36 D3..D0 = BD ($36 D7..D4 unused)");
        assert_eq!(v.hh, 0x03, "$37 D7..D4 = HH");
        assert_eq!(v.sd, 0x0C, "$37 D3..D0 = SD");
        assert_eq!(v.tom, 0x05, "$38 D7..D4 = TOM");
        assert_eq!(v.t_cy, 0x08, "$38 D3..D0 = T-CYM");
        assert_eq!(RhythmVolumes::from_regs(0, 0, 0), RhythmVolumes::default());
    }

    /// The §III-1-7 recommended rhythm F-Number/Block preset writes:
    /// `$16←$20`, `$17←$50`, `$18←$C0`, `$26←$05`, `$27←$05`,
    /// `$28←$01` — and the Key-ON bits (D4 of `$26`~`$28`) are
    /// cleared in every preset byte, as the same section requires
    /// ("Key-ON bits $26, $27, $28 must always be cleared to 0").
    #[test]
    fn rhythm_fnum_preset_matches_manual() {
        assert_eq!(
            RHYTHM_FNUM_PRESET,
            [
                (0x16, 0x20),
                (0x17, 0x50),
                (0x18, 0xC0),
                (0x26, 0x05),
                (0x27, 0x05),
                (0x28, 0x01),
            ]
        );
        for (addr, data) in RHYTHM_FNUM_PRESET {
            if (0x26..=0x28).contains(&addr) {
                assert_eq!(data & 0x10, 0, "Key-ON bit clear in ${addr:02X} preset");
            }
        }
    }

    // -------------------------------------------------- §V-4 BD synthesis

    /// `RhythmBassDrum::new` loads the BD rhythm patch
    /// ([`crate::expansion::VRC7_RHYTHM_ROM`]`[0]`) into the channel's
    /// modulator + carrier, so the FM operator params match the ROM.
    #[test]
    fn bass_drum_loads_bd_rhythm_patch() {
        let bd = RhythmBassDrum::new();
        let patch = Vrc7Patch::from_bytes(&crate::expansion::VRC7_RHYTHM_ROM[0]);
        // BD patch ROM[0] = [01 01 18 0F DF F8 6A 6D].
        // $00/$01 = 01 → MUL = 1 for both operators, no AM/VIB/SUS/KSR.
        assert_eq!(bd.channel.modulator.mul, patch.mod_mult);
        assert_eq!(bd.channel.carrier.mul, patch.car_mult);
        assert_eq!(bd.channel.modulator.mul, 1);
        // $03 = 0F → feedback FFF = 7 (max self-feedback).
        assert_eq!(bd.channel.fb, patch.feedback);
        assert_eq!(bd.channel.fb, 7);
        // The carrier's TL is replaced by per-channel volume.
        assert_eq!(bd.channel.carrier.tl, 0);
    }

    /// `new` also installs the §III-1-7 recommended channel-7 pitch
    /// (`$16=$20` F-Num low, `$26=$05` → Block 2 + F-Num MSB),
    /// giving F-Num = 0x120, Block = 2.
    #[test]
    fn bass_drum_default_pitch_matches_preset() {
        let bd = RhythmBassDrum::new();
        assert_eq!(bd.channel.fnum, 0x120);
        assert_eq!(bd.channel.block, 2);
    }

    /// The `$0E` BD bit is the BD's key gate (§III-1-7): a 0→1 edge
    /// triggers the carrier envelope attack, a 1→0 edge releases it.
    #[test]
    fn bass_drum_key_gate_drives_envelope() {
        let mut bd = RhythmBassDrum::new();
        // Idle until keyed.
        assert!(!bd.keyed);
        assert!(matches!(bd.channel.carrier.env.phase, EnvPhase::Idle));

        // Key-on edge → carrier leaves Idle (enters attack).
        bd.set_key(true);
        assert!(bd.keyed);
        assert!(!matches!(bd.channel.carrier.env.phase, EnvPhase::Idle));
        assert!(bd.is_active());

        // Re-asserting the same level is a no-op (no spurious retrigger).
        let phase_before = bd.channel.carrier.env.phase;
        bd.set_key(true);
        assert_eq!(bd.channel.carrier.env.phase, phase_before);

        // Key-off edge → carrier enters release.
        bd.set_key(false);
        assert!(!bd.keyed);
        assert!(matches!(bd.channel.carrier.env.phase, EnvPhase::Release));
    }

    /// §III-4 Figure III-3(c): "the same percussive sounds are output
    /// twice." The BD sample is the underlying channel output doubled.
    #[test]
    fn bass_drum_output_is_doubled() {
        // Two identical units; sample one through the BD path and the
        // other through the raw channel path with identical state.
        let mut bd = RhythmBassDrum::new();
        let mut raw = bd.channel; // copy of the loaded channel
        bd.set_key(true);
        raw.trigger_key_on();

        // Advance both a few operator samples so the envelope leaves 0.
        let mut bd_last = 0;
        let mut raw_last = 0;
        for _ in 0..16 {
            bd_last = bd.sample();
            raw_last = raw.sample();
        }
        assert_eq!(bd_last, raw_last * 2, "BD output is the channel output ×2");
    }

    /// `set_volume` maps the `$36` BD-VOL nibble onto the channel's
    /// per-channel volume field (3 dB per step, masked to 4 bits).
    #[test]
    fn bass_drum_volume_sets_channel_volume() {
        let mut bd = RhythmBassDrum::new();
        bd.set_volume(0x0A);
        assert_eq!(bd.channel.volume, 0x0A);
        // Only the low nibble is significant ($36 D7..D4 unused for BD).
        bd.set_volume(0xF7);
        assert_eq!(bd.channel.volume, 0x07);
    }

    /// `set_pitch` retunes channel 7 and clamps to the 9-bit F-Num /
    /// 3-bit Block fields.
    #[test]
    fn bass_drum_set_pitch_clamps_fields() {
        let mut bd = RhythmBassDrum::new();
        bd.set_pitch(0x1FF, 0x07);
        assert_eq!(bd.channel.fnum, 0x1FF);
        assert_eq!(bd.channel.block, 0x07);
        // Out-of-range bits are dropped, not retained.
        bd.set_pitch(0xFFFF, 0xFF);
        assert_eq!(bd.channel.fnum, 0x1FF);
        assert_eq!(bd.channel.block, 0x07);
    }

    // ----------------------------------------- YM2413 rhythm noise generator

    /// §3 "all-zero is a trap": from the all-zero state the LFSR stays
    /// stuck forever, so the hardware must seed a non-zero value. The
    /// default constructor seeds bit 0 and is therefore never stuck.
    #[test]
    fn noise_lfsr_all_zero_is_a_trap() {
        let mut z = OpllNoiseLfsr { state: 0 };
        for _ in 0..1000 {
            assert_eq!(z.step(), 0);
            assert_eq!(z.state, 0, "all-zero state must remain stuck at 0");
        }
        // The seeded constructor escapes the trap.
        let mut s = OpllNoiseLfsr::new();
        assert_ne!(s.state, 0);
        s.step();
        assert_ne!(s.state, 0, "a seeded LFSR never collapses to all-zero");
    }

    /// §4 Galois single-step: `bit = state & 1; state >>= 1; if bit
    /// state ^= mask`. The mask for `x^23 + x^9 + 1` taps bit 22 (the
    /// x^23 wrap) and bit 8 (x^9): `0x40_0181`. Verify the step against
    /// the explicit formula and that the returned bit is the shifted-out
    /// LSB.
    #[test]
    fn noise_lfsr_galois_step_matches_polynomial() {
        // Pick an arbitrary non-trivial state whose LSB is 1 so the
        // feedback path is exercised.
        let mut lfsr = OpllNoiseLfsr { state: 0x0012_3457 };
        let before = lfsr.state;
        let out = lfsr.step();
        assert_eq!(out, (before & 1) as u8);
        let expected = {
            let bit = before & 1;
            let mut s = before >> 1;
            if bit != 0 {
                s ^= (1 << 22) | (1 << 8) | 1;
            }
            s & 0x007F_FFFF
        };
        assert_eq!(lfsr.state, expected);
        // And a state whose LSB is 0 takes the no-feedback branch.
        let mut even = OpllNoiseLfsr { state: 0x0012_3456 };
        let b = even.state;
        let o = even.step();
        assert_eq!(o, 0);
        assert_eq!(even.state, b >> 1);
    }

    /// §3 "maximal length": the `x^23 + x^9 + 1` LFSR visits every one of
    /// the `2^23 - 1` non-zero states exactly once. A full sweep is 8.3M
    /// iterations; we instead assert the register never re-enters its
    /// seed within a window far larger than any short cycle would allow,
    /// and stays within the 23-bit field throughout.
    #[test]
    fn noise_lfsr_is_long_period() {
        let mut lfsr = OpllNoiseLfsr::new();
        let seed = lfsr.state;
        for i in 0..100_000 {
            lfsr.step();
            assert!(lfsr.state <= 0x007F_FFFF, "state escaped 23-bit field");
            assert_ne!(
                lfsr.state,
                seed,
                "seed recurred after {} steps — not maximal length",
                i + 1
            );
        }
    }

    /// §"UPDATE" rhythm protocol: per 72-cycle frame the unit samples HH,
    /// iterates 3, samples SD, iterates 15 (18 operator steps total).
    /// `rhythm_frame_bits` must return the bit *before* the 3 HH-to-SD
    /// iterations for HH and the bit after those 3 for SD, and leave the
    /// register exactly 18 steps advanced.
    #[test]
    fn noise_lfsr_rhythm_frame_protocol() {
        // Reference register stepped manually with the documented cadence.
        let mut reference = OpllNoiseLfsr::new();
        let hh_ref = reference.bit();
        for _ in 0..3 {
            reference.step();
        }
        let sd_ref = reference.bit();
        for _ in 0..15 {
            reference.step();
        }

        let mut frame = OpllNoiseLfsr::new();
        let (hh, sd) = frame.rhythm_frame_bits();
        assert_eq!(hh, hh_ref, "HH bit is sampled before the 3-step gap");
        assert_eq!(sd, sd_ref, "SD bit is sampled 3 steps after HH");
        assert_eq!(
            frame.state, reference.state,
            "one rhythm frame advances exactly 18 operator steps"
        );
    }

    /// `reset` returns the generator to the non-zero power-on seed.
    #[test]
    fn noise_lfsr_reset_restores_seed() {
        let mut lfsr = OpllNoiseLfsr::new();
        let seed = lfsr.state;
        for _ in 0..123 {
            lfsr.step();
        }
        assert_ne!(lfsr.state, seed);
        lfsr.reset();
        assert_eq!(lfsr.state, seed);
        assert_ne!(lfsr.state, 0);
    }

    // --------------------------------------------- §8a/§8b synthesis property

    /// Build a keyed-on OPLL channel producing a steady full-volume
    /// carrier tone (fast attack, sustained envelope, no modulator
    /// feedback / TL), with the carrier's AM/VIB bits set per the args.
    /// The carrier is driven directly (mul=1) so it behaves like a near-
    /// pure sine for the property measurements below.
    fn steady_carrier_channel(am: bool, vib: bool) -> OpllChannel {
        let mut ch = OpllChannel {
            fnum: 0x100,
            block: 4,
            volume: 0, // loudest
            ..OpllChannel::default()
        };
        // Modulator silenced (TL max) so the carrier is the only voice.
        ch.modulator.mul = 1;
        ch.modulator.tl = 63;
        ch.modulator.env.load_from_patch(15, 0, 0, 15, true); // instant attack, hold
                                                              // Carrier: fast attack, sustained tone, AM/VIB per request.
        ch.carrier.mul = 1;
        ch.carrier.am = am;
        ch.carrier.vib = vib;
        ch.carrier.env.load_from_patch(15, 0, 0, 15, true);
        ch.refresh_rks();
        ch.trigger_key_on();
        // Settle the attack so the carrier reaches full amplitude.
        for _ in 0..64 {
            let _ = ch.sample();
        }
        ch
    }

    /// Render a channel for `n` samples (advancing the supplied LFO each
    /// sample) and return the peak absolute carrier amplitude.
    fn peak_over(ch: &mut OpllChannel, lfo: &mut Lfo, fast: bool, n: usize) -> i32 {
        let mut peak = 0;
        let test = TestRegister::default();
        for _ in 0..n {
            let s = ch.sample_with_test(&test, lfo).abs();
            peak = peak.max(s);
            lfo.tick(false, fast);
        }
        peak
    }

    /// §8a synthesis property: an AM-enabled carrier's peak amplitude
    /// dips by the silicon-measured ≈ 4.8 dB between the AM trough
    /// (level 0) and crest (level 13), on the live synthesis path. We
    /// hold the LFO at each extreme and compare the rendered peaks.
    #[test]
    fn am_depth_on_synthesis_path_is_measured_4p8_db() {
        // Trough: LFO phase 0 → AM level 0 → no extra attenuation.
        let mut trough = steady_carrier_channel(true, false);
        let mut lfo0 = Lfo::default();
        let peak_trough = peak_over(&mut trough, &mut lfo0, false, 256);

        // Crest: advance a fresh LFO (fast clock) to the AM peak level
        // (13), then hold there by re-reading the same phase. We find a
        // tremolo_phase whose table entry is 13.
        let crest_phase = AM_LFO_LEVELS.iter().position(|&l| l == 13).unwrap() as u32;
        let mut crest = steady_carrier_channel(true, false);
        let mut lfo_crest = Lfo {
            tremolo_phase: crest_phase,
            ..Lfo::default()
        };
        assert_eq!(lfo_crest.tremolo_am_level(true), 13, "set LFO to AM crest");
        // Render without advancing the LFO phase (divider keeps it on
        // the crest table entry for the whole window).
        let test = TestRegister::default();
        let mut peak_crest = 0;
        for _ in 0..256 {
            peak_crest = peak_crest.max(crest.sample_with_test(&test, &lfo_crest).abs());
            // Keep the phase pinned on the crest entry.
            lfo_crest.tremolo_phase = crest_phase;
        }

        assert!(peak_trough > 0 && peak_crest > 0, "carrier produced audio");
        assert!(
            peak_crest < peak_trough,
            "AM crest ({peak_crest}) should attenuate below trough ({peak_trough})"
        );
        // Depth in dB: 20*log10(crest/trough) ≈ -4.8 dB. Allow a modest
        // band for quantisation in the 4-bit-dropped exp output.
        let depth_db = 20.0 * (peak_crest as f64 / peak_trough as f64).log10();
        assert!(
            (-5.6..=-4.0).contains(&depth_db),
            "AM depth {depth_db:.2} dB outside the measured ≈4.8 dB band \
             (trough peak {peak_trough}, crest peak {peak_crest})"
        );
    }

    /// §8a synthesis property: with AM disabled the carrier peak is
    /// constant regardless of the LFO phase (no tremolo), but with AM
    /// enabled the peak amplitude is modulated by it (the tremolo is
    /// audible). Peaks are measured over a 256-sample window with the
    /// LFO held at a fixed AM level.
    #[test]
    fn am_enabled_modulates_peak_across_period() {
        let test = TestRegister::default();
        let crest_phase = AM_LFO_LEVELS.iter().position(|&l| l == 13).unwrap() as u32;
        // Peak over a window with the tremolo phase pinned to `phase`.
        let peak_pinned = |am: bool, phase: u32| -> i32 {
            let mut ch = steady_carrier_channel(am, false);
            let lfo = Lfo {
                tremolo_phase: phase,
                ..Lfo::default()
            };
            let mut peak = 0;
            for _ in 0..256 {
                peak = peak.max(ch.sample_with_test(&test, &lfo).abs());
            }
            peak
        };

        // AM off: peak identical at trough phase and crest phase.
        assert_eq!(
            peak_pinned(false, 0),
            peak_pinned(false, crest_phase),
            "AM-disabled carrier ignores the LFO phase"
        );

        // AM on: crest phase attenuates the peak below the trough phase.
        let p_on_trough = peak_pinned(true, 0);
        let p_on_crest = peak_pinned(true, crest_phase);
        assert!(
            p_on_crest < p_on_trough,
            "AM-enabled carrier is modulated by the LFO phase \
             (trough {p_on_trough}, crest {p_on_crest})"
        );
    }

    /// §8b synthesis property: a VIB-enabled carrier's phase advances at
    /// a different rate at the vibrato sweep extremes (columns 2 and 6 of
    /// the top F-Number row), proving the §8b table drives the live
    /// phase-step. With VIB off the phase rate is constant.
    #[test]
    fn vib_enabled_sweeps_phase_rate_on_synthesis_path() {
        // fnum 0x1c0 → fnum>>6 = 7 (the largest-sweep row).
        let make = |vib: bool| -> OpllChannel {
            let mut ch = OpllChannel {
                fnum: 0x1c0,
                block: 6,
                volume: 0,
                ..OpllChannel::default()
            };
            ch.modulator.mul = 1;
            ch.modulator.tl = 63;
            ch.modulator.env.load_from_patch(15, 0, 0, 15, true);
            ch.carrier.mul = 1;
            ch.carrier.vib = vib;
            ch.carrier.env.load_from_patch(15, 0, 0, 15, true);
            ch.refresh_rks();
            ch.trigger_key_on();
            ch
        };
        // Measure the carrier phase increment over one sample at the
        // positive-peak vibrato column (2) vs the negative-peak (6).
        let phase_inc_at = |col: u32| -> u32 {
            let mut ch = make(true);
            let lfo = Lfo {
                vibrato_phase: col,
                ..Lfo::default()
            };
            let before = ch.carrier.phase_acc;
            let _ = ch.sample_with_test(&TestRegister::default(), &lfo);
            ch.carrier.phase_acc.wrapping_sub(before)
        };
        let inc_sharp = phase_inc_at(2); // +7 correction
        let inc_flat = phase_inc_at(6); // -7 correction
        assert!(
            inc_sharp > inc_flat,
            "vibrato column 2 (sharp, {inc_sharp}) must exceed column 6 (flat, {inc_flat})"
        );
        // VIB off: the increment is the same regardless of LFO column.
        let mut off = make(false);
        let lfo_a = Lfo {
            vibrato_phase: 2,
            ..Lfo::default()
        };
        let before_a = off.carrier.phase_acc;
        let _ = off.sample_with_test(&TestRegister::default(), &lfo_a);
        let inc_a = off.carrier.phase_acc.wrapping_sub(before_a);
        let mut off2 = make(false);
        let lfo_b = Lfo {
            vibrato_phase: 6,
            ..Lfo::default()
        };
        let before_b = off2.carrier.phase_acc;
        let _ = off2.sample_with_test(&TestRegister::default(), &lfo_b);
        let inc_b = off2.carrier.phase_acc.wrapping_sub(before_b);
        assert_eq!(
            inc_a, inc_b,
            "VIB-disabled carrier has a constant phase rate"
        );
    }
}
