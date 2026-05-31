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
//! * `ym2413-logsin-exp-tables-andete-2015-04-09.txt` — the exact
//!   `initTables` / `lookupSin` / `lookupExp` algorithm transcribed
//!   from andete's independent silicon-RE post.
//! * `vrcvii-kevtris.txt` — VRC7-specific register map + frequency
//!   formula `F = 49722 * fnum / 2^(19 - octave)`.
//! * `ym2413-application-manual-smspower.html` /
//!   `ym2413-application-manual.pdf` — vendor datasheet for the
//!   register-contents semantics.
//!
//! Per the §"Provenance & non-emulator sourcing" appendix in
//! `opll-ym2413-tables.md`, the staged tables are derived from
//! vendor-datasheet + independent silicon-RE (Gambrell/Niemitalo +
//! andete) sources; no emulator source tree (emu2413, Nuked-OPLL,
//! ymfm, MAME, FFmpeg, GME, OpenMSX, libGME) was consulted.
//!
//! Numeric tables that the §"Provenance" appendix flags as
//! **deliberately not transcribed from any emulator constant** —
//! the KSL byte array, the AM/VIB LFO step arrays, and the per-rate
//! envelope-increment array — are NOT lifted from external source
//! here either. The envelope generator currently runs a coarse
//! linear-rate approximation against the manual's documented
//! semantics (rate 0 = halt, rate 1 = slowest, rate 15 = fastest);
//! exact OPLx-decapsulated per-rate increments are a DOCS-GAP
//! followup tracked in the crate README. The KSL attenuation table
//! is computed from the OPL-family `(block_fnum_KSL_base) >> (3 -
//! KSL)` formula documented in §4 against a base table that has not
//! been transcribed — KSL therefore currently contributes 0 dB and
//! is also a documented followup.

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
/// numeric step arrays are deliberately not transcribed from any
/// external source. The envelope here therefore implements the
/// **documented behaviour** (key-on triggers attack to 0; attack
/// transitions to decay; decay ramps to the sustain-level; key-off /
/// non-sustain triggers release) but its per-rate slope is a coarse
/// linear approximation calibrated so rate=0 halts and rate=15 is the
/// fastest. Precise OPLx-decapsulated rate increments are a documented
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
}

/// Maximum envelope level (silence). Per andete §"envelope levels":
/// envelope levels run 0..=127 and the exp-table offset is
/// `+16 * eg_level`, so `16 * 127 = 2032` — comfortably below the
/// log-sin maximum of 2137, ensuring silence.
pub const ENV_MAX_LEVEL: u32 = 127;

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
        // Note: `release_disabled` is set independently by the channel's
        // patch loader — only the modulator operator sets it on $00.S.
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
    /// The per-rate step magnitude is the coarse approximation flagged
    /// in the module docstring: rate 0 halts, rate `n>0` advances by
    /// `2^(n-1)` Q16 units per sample. This produces a monotonic
    /// rate ladder that is faithful to the manual's rate=0..=15
    /// semantics but is NOT bit-exact against the OPLx-decapsulated
    /// per-rate increment arrays.
    pub fn step(&mut self, samples: u32) {
        let advance = |rate: u8, s: u32| -> u32 {
            if rate == 0 {
                0
            } else {
                // (1 << (rate-1)) Q16-units per sample
                (1u32 << (rate as u32 - 1)).saturating_mul(s)
            }
        };

        match self.phase {
            EnvPhase::Idle => {
                self.level_q16 = ENV_MAX_LEVEL << 16;
            }
            EnvPhase::Attack => {
                let step = advance(self.attack_rate, samples);
                // Attack ramps DOWN to 0 (= loudest).
                self.level_q16 = self.level_q16.saturating_sub(step);
                if self.level_q16 == 0 {
                    self.phase = EnvPhase::Decay;
                }
            }
            EnvPhase::Decay => {
                let step = advance(self.decay_rate, samples);
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
                    // release rate.
                    let step = advance(self.release_rate, samples);
                    self.level_q16 = self.level_q16.saturating_add(step).min(ENV_MAX_LEVEL << 16);
                    if self.level_q16 >= ENV_MAX_LEVEL << 16 {
                        self.phase = EnvPhase::Idle;
                    }
                }
                // Sustained tone: hold here until key-off.
            }
            EnvPhase::Release => {
                let step = advance(self.release_rate, samples);
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
    /// and resets both the tremolo and vibrato LFO." We do not yet
    /// run an LFO (the §7 numeric step arrays are a documented
    /// DOCS-GAP), so this bit is recorded but has no operator
    /// effect today.
    pub hold_lfo: bool,
    /// `$0F` bit 2 — "Holds and resets waveform phase to zero. The
    /// envelopes are not halted, though the output will be silent."
    pub hold_phase: bool,
    /// `$0F` bit 3 — "Update tremolo and vibrato LFOs every sample
    /// instead of once every several samples." Same no-op-today
    /// story as `hold_lfo`.
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
        self.modulator.env.load_from_patch(
            p.mod_attack,
            p.mod_decay,
            p.mod_sustain_level,
            p.mod_release,
            p.mod_sustain,
        );
        // §"Custom Patch": modulator $00.S also disables its release.
        self.modulator.env.release_disabled = p.mod_sustain;

        // Carrier (operator #1).
        self.carrier.mul = p.car_mult;
        self.carrier.tl = 0; // carrier has no TL; volume takes its place
        self.carrier.half_rect = p.car_wave != 0;
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

        self.fb = p.feedback;
        self.volume = volume & 0x0F;
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
        self.sample_with_test(&TestRegister::default())
    }

    /// `sample` with the test-register hooks honoured. The 4-bit
    /// `$0F` field is documented in
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F":
    ///
    /// * bit 0 — envelope output forced to 0 (full volume) for both
    ///   modulator and carrier. The envelopes are still ticked
    ///   internally, only the per-sample contribution is bypassed.
    /// * bit 1 — hold LFO phase at 0 (halt + reset both tremolo and
    ///   vibrato). We don't yet implement the LFO numeric step
    ///   arrays (a documented §7 DOCS-GAP), so this bit is a no-op
    ///   from the operator's point of view, but it is recorded on
    ///   the chip so a future LFO landing inherits the gate.
    /// * bit 2 — hold + reset waveform phase to 0. Both operator
    ///   phase accumulators are pinned at 0 (and reset on entry);
    ///   envelopes keep running but output is silent (sin(0)≈0).
    /// * bit 3 — LFO speed override (tremolo 64×, vibrato 1024×
    ///   faster). Same no-op-but-recorded story as bit 1.
    pub fn sample_with_test(&mut self, test: &TestRegister) -> i32 {
        // Phase generator base rate. The VRC7 vrcvii doc gives:
        //   F = 49722 * fnum / 2^(19 - block)  Hz
        // Equivalent per-49716 Hz sample phase delta:
        //   delta_per_sample = (fnum << block) * MUL_x2 / 2
        let fnum_block = (self.fnum as u32) << (self.block as u32);

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
        // §"Test Register $0F" bit 0: modulator envelope contribution
        // forced to 0. We do this by sampling with the env-offset
        // pre-cancelled (the env is still ticked below).
        let mod_out = if test.envs_zero {
            self.modulator
                .sample_with_env_override(fb_phase, mod_tl_atten, 0)
        } else {
            self.modulator.sample(fb_phase, mod_tl_atten)
        };

        // Update modulator feedback history.
        self.fb_prev[1] = self.fb_prev[0];
        self.fb_prev[0] = mod_out;

        // Step both phase generators by one operator sample — but
        // §"Test Register $0F" bit 2 also says the phase is *held*,
        // so we skip the step in that case too.
        if !test.hold_phase {
            self.modulator.step_phase(fnum_block);
            self.carrier.step_phase(fnum_block);
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
        // = 8).
        let car_volume_atten = (self.volume as u32) * 8;
        if test.envs_zero {
            self.carrier
                .sample_with_env_override(car_mod, car_volume_atten, 0)
        } else {
            self.carrier.sample(car_mod, car_volume_atten)
        }
    }

    /// Whether the carrier is currently producing audio (envelope not
    /// fully released).
    pub fn is_active(&self) -> bool {
        !matches!(self.carrier.env.phase, EnvPhase::Idle)
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
            let s = ch.sample_with_test(&test);
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
            let s = ch.sample_with_test(&test);
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
            let _ = ch.sample_with_test(&test);
        }
        ch.trigger_key_off();
        let mut reached_idle = false;
        for _ in 0..200_000 {
            let _ = ch.sample_with_test(&test);
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
}
