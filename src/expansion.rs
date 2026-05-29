//! Expansion-chip emulators.
//!
//! NSF supports six audio expansion chips, all of which sit on top of
//! the 2A03 APU. Their outputs are summed (with chip-specific scaling)
//! into the host APU mixer. None of the chips share registers with the
//! 2A03 — they all live in `$5000..=$5FFF` or, for the cartridge-based
//! chips that re-use `$8000..=$FFFF`, in `$9000..=$FFFF`.
//!
//! Round 2 implements the channel sequencers + mixer outputs. None of
//! the chips need cycle-accurate read-side timing for music playback;
//! we tick them off the same per-CPU-cycle clock as the 2A03 channels.
//!
//! References (used as documentation only — no source consulted):
//!
//! * Sunsoft 5B — nesdev.org/wiki/Sunsoft_5B_audio
//! * MMC5 — nesdev.org/wiki/MMC5_audio
//! * VRC6 — nesdev.org/wiki/VRC6_audio
//! * VRC7 — nesdev.org/wiki/VRC7_audio (FM synth — round 2 picks an
//!   approximation that emits the right channel-mix balance without a
//!   bit-exact OPLL operator implementation)
//! * Namco 163 — nesdev.org/wiki/Namco_163_audio
//! * FDS — nesdev.org/wiki/Famicom_Disk_System_audio

use crate::header::ExpansionChips;

// ---------------------------------------------------------------- VRC6

/// VRC6 has two pulse channels + one sawtooth. Registers live at
/// `$9000..=$B002` in three groups of three.
#[derive(Default)]
pub struct Vrc6 {
    pub enabled: bool,
    pub pulse: [Vrc6Pulse; 2],
    pub saw: Vrc6Saw,
    /// Output frequency divider (`$9003`): 1 / 16 / 256.
    pub freq_shift: u8,
    pub halt: bool,
}

#[derive(Default, Clone, Copy)]
pub struct Vrc6Pulse {
    pub enabled: bool,
    pub mode_digital: bool,
    pub duty: u8,
    pub volume: u8,
    pub timer_period: u16,
    pub timer: u16,
    pub step: u8,
}

#[derive(Default)]
pub struct Vrc6Saw {
    pub enabled: bool,
    pub rate: u8,
    pub timer_period: u16,
    pub timer: u16,
    pub accum: u8,
    pub step: u8,
}

impl Vrc6 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x9000 => {
                self.pulse[0].volume = value & 0x0F;
                self.pulse[0].duty = (value >> 4) & 0x07;
                self.pulse[0].mode_digital = value & 0x80 != 0;
            }
            0x9001 => {
                self.pulse[0].timer_period = (self.pulse[0].timer_period & 0x0F00) | value as u16;
            }
            0x9002 => {
                self.pulse[0].timer_period =
                    (self.pulse[0].timer_period & 0x00FF) | (((value & 0x0F) as u16) << 8);
                self.pulse[0].enabled = value & 0x80 != 0;
            }
            0x9003 => {
                self.halt = value & 0x01 != 0;
                self.freq_shift = (value >> 1) & 0x03;
            }
            0xA000 => {
                self.pulse[1].volume = value & 0x0F;
                self.pulse[1].duty = (value >> 4) & 0x07;
                self.pulse[1].mode_digital = value & 0x80 != 0;
            }
            0xA001 => {
                self.pulse[1].timer_period = (self.pulse[1].timer_period & 0x0F00) | value as u16;
            }
            0xA002 => {
                self.pulse[1].timer_period =
                    (self.pulse[1].timer_period & 0x00FF) | (((value & 0x0F) as u16) << 8);
                self.pulse[1].enabled = value & 0x80 != 0;
            }
            0xB000 => {
                self.saw.rate = value & 0x3F;
            }
            0xB001 => {
                self.saw.timer_period = (self.saw.timer_period & 0x0F00) | value as u16;
            }
            0xB002 => {
                self.saw.timer_period =
                    (self.saw.timer_period & 0x00FF) | (((value & 0x0F) as u16) << 8);
                self.saw.enabled = value & 0x80 != 0;
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        if self.halt {
            return;
        }
        let scale: u32 = match self.freq_shift {
            0 => 1,
            1 => 16,
            _ => 256,
        };
        for p in &mut self.pulse {
            if !p.enabled {
                continue;
            }
            let mut left = cycles;
            while left > 0 {
                let take = left.min(scale);
                if p.timer == 0 {
                    p.timer = p.timer_period;
                    p.step = (p.step + 1) & 0x0F;
                } else {
                    p.timer = p.timer.saturating_sub(take.min(p.timer as u32) as u16);
                }
                left = left.saturating_sub(take);
            }
        }
        if self.saw.enabled {
            let mut left = cycles;
            while left > 0 {
                let take = left.min(scale);
                if self.saw.timer == 0 {
                    self.saw.timer = self.saw.timer_period;
                    self.saw.step = (self.saw.step + 1) & 0x0D;
                    if self.saw.step == 0 {
                        self.saw.accum = 0;
                    } else if self.saw.step & 0x01 == 0 {
                        self.saw.accum = self.saw.accum.wrapping_add(self.saw.rate);
                    }
                } else {
                    self.saw.timer = self
                        .saw
                        .timer
                        .saturating_sub(take.min(self.saw.timer as u32) as u16);
                }
                left = left.saturating_sub(take);
            }
        }
    }

    /// Pulse 0 + pulse 1 + saw, range ≈ 0..61 (15+15+31).
    pub fn output(&self) -> f32 {
        let mut o = 0u32;
        for p in &self.pulse {
            if !p.enabled || p.timer_period < 1 {
                continue;
            }
            let high = if p.mode_digital {
                true
            } else {
                p.step <= p.duty
            };
            if high {
                o += p.volume as u32;
            }
        }
        if self.saw.enabled {
            o += (self.saw.accum >> 3) as u32; // top 5 bits
        }
        // Mixer scaling per nesdev wiki: VRC6 outputs ~0..61 → ~0.4
        // peak when summed against the 2A03. Tuned empirically.
        o as f32 / 100.0
    }
}

// ---------------------------------------------------------------- MMC5

/// MMC5 audio has 2 pulses (almost identical to 2A03 pulses but no
/// sweep) at `$5000..=$5007` plus a raw 8-bit PCM channel at `$5011`
/// and a status register at `$5015`.
#[derive(Default)]
pub struct Mmc5 {
    pub enabled: bool,
    pub pulse: [Mmc5Pulse; 2],
    pub pcm: u8,
    pub pcm_read_mode: bool,
    pub status: u8,
}

#[derive(Default, Clone, Copy)]
pub struct Mmc5Pulse {
    pub enabled: bool,
    pub duty: u8,
    pub volume: u8,
    pub constant: bool,
    pub timer_period: u16,
    pub timer: u16,
    pub step: u8,
    pub length: u8,
    pub halt: bool,
}

const MMC5_DUTY: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 1],
];

impl Mmc5 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x5000 => {
                self.pulse[0].duty = (value >> 6) & 0x03;
                self.pulse[0].halt = value & 0x20 != 0;
                self.pulse[0].constant = value & 0x10 != 0;
                self.pulse[0].volume = value & 0x0F;
            }
            0x5002 => {
                self.pulse[0].timer_period = (self.pulse[0].timer_period & 0xFF00) | value as u16;
            }
            0x5003 => {
                self.pulse[0].timer_period =
                    (self.pulse[0].timer_period & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.pulse[0].length = (value >> 3) & 0x1F;
                self.pulse[0].step = 0;
            }
            0x5004 => {
                self.pulse[1].duty = (value >> 6) & 0x03;
                self.pulse[1].halt = value & 0x20 != 0;
                self.pulse[1].constant = value & 0x10 != 0;
                self.pulse[1].volume = value & 0x0F;
            }
            0x5006 => {
                self.pulse[1].timer_period = (self.pulse[1].timer_period & 0xFF00) | value as u16;
            }
            0x5007 => {
                self.pulse[1].timer_period =
                    (self.pulse[1].timer_period & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.pulse[1].length = (value >> 3) & 0x1F;
                self.pulse[1].step = 0;
            }
            0x5010 => {
                self.pcm_read_mode = value & 0x01 != 0;
            }
            0x5011 if !self.pcm_read_mode => {
                self.pcm = value;
            }
            0x5015 => {
                self.status = value;
                self.pulse[0].enabled = value & 0x01 != 0;
                self.pulse[1].enabled = value & 0x02 != 0;
            }
            _ => {}
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x5015 => {
                let mut s = 0u8;
                if self.pulse[0].length > 0 {
                    s |= 0x01;
                }
                if self.pulse[1].length > 0 {
                    s |= 0x02;
                }
                s
            }
            _ => 0xFF,
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        for p in &mut self.pulse {
            if !p.enabled {
                continue;
            }
            let mut left = cycles;
            while left > 0 {
                let take = left.min(2); // /2 prescaler (matches 2A03 pulse).
                if p.timer == 0 {
                    p.timer = p.timer_period;
                    p.step = (p.step + 1) & 0x07;
                } else {
                    p.timer = p.timer.saturating_sub(take.min(p.timer as u32) as u16);
                }
                left = left.saturating_sub(take);
            }
        }
    }

    pub fn output(&self) -> f32 {
        let p0 = if self.pulse[0].enabled
            && self.pulse[0].timer_period >= 8
            && MMC5_DUTY[self.pulse[0].duty as usize][self.pulse[0].step as usize] != 0
        {
            self.pulse[0].volume as u32
        } else {
            0
        };
        let p1 = if self.pulse[1].enabled
            && self.pulse[1].timer_period >= 8
            && MMC5_DUTY[self.pulse[1].duty as usize][self.pulse[1].step as usize] != 0
        {
            self.pulse[1].volume as u32
        } else {
            0
        };
        // Pulses share the 2A03 mixer curve approximation.
        let pulse_sum = (p0 + p1) as f32;
        let pulse_out = if pulse_sum <= 0.0 {
            0.0
        } else {
            95.88 / (8128.0 / pulse_sum + 100.0)
        };
        // Raw PCM channel: 8-bit unsigned, treated as a pure DC offset
        // around the midline. Mix in linearly — empirical scale tracks
        // the 2A03 DMC contribution.
        let pcm_out = (self.pcm as f32 - 128.0) / 256.0 * 0.6;
        pulse_out + pcm_out
    }
}

// ---------------------------------------------------------------- Sunsoft 5B

/// Sunsoft 5B (a Yamaha YM2149F derivative) has three square channels,
/// a noise generator, and an envelope generator that may be shared by
/// any of the three channels.
///
/// Round 12 (per `docs/audio/nsf/sunsoft-5b-audio-wiki.html`):
///   * **Tone**: 12-bit period at `$00`..=`$05`; the high/low square
///     state flips every 16 clocks when an internal counter reaches
///     (>=) the period, then the counter resets to 0 per §Sound.
///     Writing a period smaller than the current counter triggers an
///     immediate flip on the next 16-clock boundary per §Sound.
///   * **Noise**: 5-bit period at `$06`; a 17-bit linear-feedback
///     shift register with taps at bits 16 and 13 advances every 32
///     clocks (one new random bit per 32 clocks per §Noise).
///   * **Mixer** at `$07`: low three bits = per-channel tone-disable
///     (active-high), high three bits = per-channel noise-disable
///     (active-high). When BOTH tone and noise are disabled on a
///     channel, the channel emits a constant signal at the configured
///     volume per §Sound. When both are enabled, the channel emits
///     the logical AND of tone and noise.
///   * **Volume / envelope-route** at `$08`..=`$0A`: bit 4 routes
///     the envelope generator instead of the 4-bit volume.
///   * **Envelope**: 16-bit period at `$0B`/`$0C` and 4-bit shape at
///     `$0D`. A 32-step ramp ticks every `16 * period` clocks. Shape
///     bits select continue / attack / alternate / hold per §Shape:
///     the eight bilevel patterns `$08`..=`$0F` and the four
///     decay/attack-once patterns `$00`..=`$07`. Writing `$0D`
///     resets the envelope phase to step 0 of the selected shape.
///   * **Output**: each tone produces a 5-bit signal converted by a
///     logarithmic DAC with 1.5 dB per step. Envelope step 1 equals
///     volume 0 (silent); even envelope steps map to the
///     corresponding 4-bit volume per the §Output step table.
///
/// Period 0 produces the same period as 1 per the §Sound note (and
/// the cited Period 0 verification) for tone, noise, and envelope.
#[derive(Default)]
pub struct Sunsoft5b {
    pub enabled: bool,
    pub addr: u8,
    pub regs: [u8; 16],
    pub channels: [S5bChan; 3],
    /// Noise generator — 5-bit period at `$06`, 17-bit LFSR shared
    /// by every channel whose `$07` noise-disable bit is clear.
    pub noise: S5bNoise,
    /// Envelope generator — 16-bit period at `$0B`/`$0C`, 32-step
    /// ramp, shape parameters at `$0D` low nibble.
    pub envelope: S5bEnvelope,
}

#[derive(Default, Clone, Copy)]
pub struct S5bChan {
    pub timer_period: u16,
    /// Counter that increments every 16 clocks; on reaching the
    /// period it flips `level` and resets to 0 per §Sound.
    pub timer: u16,
    pub level: u8,
}

#[derive(Clone, Copy)]
pub struct S5bNoise {
    /// 5-bit period from `$06` (low five bits).
    pub period: u8,
    /// 16-clock-tick counter. Bit 0 toggles every 16 clocks so the
    /// noise advances only on its 0-phase (every 32 clocks per
    /// §Noise); the upper bits are the noise-period counter.
    pub timer: u16,
    /// 17-bit linear-feedback shift register. Bit 0 is the live
    /// output sample shared with the three channels.
    pub lfsr: u32,
}

impl Default for S5bNoise {
    fn default() -> Self {
        // The noise LFSR must seed to a non-zero value, otherwise
        // an all-zero shift-register stays at all-zero forever and
        // the chip emits silence on the noise output.
        Self {
            period: 0,
            timer: 0,
            lfsr: 1,
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct S5bEnvelope {
    /// 16-bit period at `$0B`/`$0C`.
    pub period: u16,
    /// 16-clock-tick counter — the envelope advances one step every
    /// `period` of these 16-clock intervals per §Period.
    pub timer: u16,
    /// Current ramp step, 0..=31.
    pub step: u8,
    /// Shape low nibble: `CAaH` (continue / attack / alternate /
    /// hold) per §Shape table. The semantics live in
    /// `envelope_advance()`.
    pub shape: u8,
    /// Set true after the first attack pass completes; controls the
    /// `continue=0` "one-shot" patterns that drop to silence and
    /// stay silent forever.
    pub attacked: bool,
    /// Holding flag once the shape requested a hold and the attack
    /// pass finished — when true, the step counter stops advancing
    /// per §Shape "hold".
    pub holding: bool,
}

impl Sunsoft5b {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xC000 => self.addr = value & 0x0F,
            0xE000 => {
                let r = (self.addr & 0x0F) as usize;
                self.regs[r] = value;
                match r {
                    // Channel A/B/C 12-bit tone periods at `$00`..=`$05`.
                    0..=5 => {
                        let ch = r / 2;
                        let lo = self.regs[ch * 2] as u16;
                        let hi = (self.regs[ch * 2 + 1] & 0x0F) as u16;
                        self.channels[ch].timer_period = (hi << 8) | lo;
                    }
                    // Noise 5-bit period at `$06`.
                    6 => {
                        self.noise.period = value & 0x1F;
                    }
                    // Envelope low / high period at `$0B` / `$0C`.
                    0x0B => {
                        let hi = self.regs[0x0C] as u16;
                        self.envelope.period = (hi << 8) | value as u16;
                    }
                    0x0C => {
                        let lo = self.regs[0x0B] as u16;
                        self.envelope.period = ((value as u16) << 8) | lo;
                    }
                    // Envelope shape at `$0D` — writing it resets the
                    // envelope phase to the start of the selected
                    // shape per §Shape.
                    0x0D => {
                        let shape = value & 0x0F;
                        self.envelope.shape = shape;
                        self.envelope.timer = 0;
                        self.envelope.attacked = false;
                        self.envelope.holding = false;
                        // Attack bit 2 selects rising vs falling, so
                        // the ramp starts at 0 if attacking and 31
                        // otherwise — matches the leading edge in
                        // every row of the §Shape table.
                        let attack = (shape & 0b0100) != 0;
                        self.envelope.step = if attack { 0 } else { 31 };
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        // The 5B's audio is driven by the CPU clock; tone, noise and
        // envelope all observe a 16-clock minor tick per §Sound. We
        // count whole 16-clock intervals here.
        let intervals = cycles / 16;
        for _ in 0..intervals {
            // Tone channels — flip when the counter reaches the
            // period (`>= period`), then reset to 0 per §Sound. A
            // period of 0 behaves as 1 (flip every 16 clocks). The
            // §Sound note about shortened periods causing an
            // immediate flip falls out of `>=` automatically.
            for ch in &mut self.channels {
                let p = if ch.timer_period == 0 {
                    1
                } else {
                    ch.timer_period
                };
                ch.timer = ch.timer.saturating_add(1);
                if ch.timer >= p {
                    ch.timer = 0;
                    ch.level ^= 1;
                }
            }
            // Noise — advances only every other 16-clock interval
            // (every 32 clocks per §Noise). Period 0 again behaves
            // as 1.
            self.noise.timer = self.noise.timer.wrapping_add(1);
            if self.noise.timer & 1 == 0 {
                let p = if self.noise.period == 0 {
                    1
                } else {
                    self.noise.period
                };
                let nticks = self.noise.timer >> 1;
                if nticks as u32 >= p as u32 {
                    self.noise.timer = 0;
                    // 17-bit LFSR with taps at bits 16 and 13 per
                    // §Noise. Shift right by one; the feedback bit
                    // is (bit 0 XOR bit 3) re-inserted at bit 16.
                    let lfsr = self.noise.lfsr;
                    let new_bit = ((lfsr ^ (lfsr >> 3)) & 1) << 16;
                    self.noise.lfsr = (lfsr >> 1) | new_bit;
                    self.noise.lfsr &= 0x0001_FFFF;
                }
            }
            // Envelope — advances one step every `period` intervals
            // per §Period. Period 0 again behaves as 1. The holding
            // flag stops the ramp at the held step per §Shape.
            if !self.envelope.holding {
                let p = if self.envelope.period == 0 {
                    1
                } else {
                    self.envelope.period
                };
                self.envelope.timer = self.envelope.timer.wrapping_add(1);
                if self.envelope.timer >= p {
                    self.envelope.timer = 0;
                    self.envelope_advance();
                }
            }
        }
    }

    /// Advance the envelope one step inside the active shape per
    /// §Shape, handling continue / attack / alternate / hold.
    fn envelope_advance(&mut self) {
        let shape = self.envelope.shape;
        let attack = (shape & 0b0100) != 0;
        let cont = (shape & 0b1000) != 0;
        let alt = (shape & 0b0010) != 0;
        let hold = (shape & 0b0001) != 0;

        let mut step = self.envelope.step;
        let mut attacked = self.envelope.attacked;
        let mut holding = self.envelope.holding;

        // Direction: follow the `attack` bit until the first edge
        // is reached; thereafter, alternate flips the direction.
        let rising = if !attacked { attack } else { attack ^ alt };

        // Compute the natural next step in the active direction,
        // then apply edge-of-ramp behaviour when we land outside
        // 0..=31.  The order is important: edge transitions take
        // effect on the same tick that crossed the boundary, so the
        // ramp doesn't sit one extra tick at the held endpoint
        // before applying the §Shape behaviour.
        let next = if rising {
            step as i16 + 1
        } else {
            step as i16 - 1
        };
        if (0..=31).contains(&next) {
            step = next as u8;
        } else {
            attacked = true;
            if rising {
                // Just walked past 31 — apply §Shape behaviour for
                // the high edge.
                if !cont {
                    // §Shape `$04..$07`: one-shot attack `/_______`
                    // — drop to 0 and stay there forever.
                    step = 0;
                    holding = true;
                } else if hold {
                    // §Shape `$0D` / `$0F`: hold after attack. With
                    // alternate, value flips at the end of the
                    // attack per §Shape (`$0F` → 0).
                    step = if alt { 0 } else { 31 };
                    holding = true;
                } else if alt {
                    // §Shape `$0E`: continue + attack + alternate
                    // (no hold) — flip and start falling. 30 is the
                    // first falling step after the peak.
                    step = 30;
                } else {
                    // §Shape `$0C`: continue + attack sawtooth —
                    // wrap to 0 and rise again.
                    step = 0;
                }
            } else {
                // Just walked past 0 — apply §Shape behaviour for
                // the low edge.
                if !cont {
                    // §Shape `$00..$03`: one-shot decay `\_______`
                    // — hold at 0 forever.
                    step = 0;
                    holding = true;
                } else if hold {
                    // §Shape `$09` / `$0B`: hold after attack. With
                    // alternate, value flips at the end per §Shape.
                    step = if alt { 31 } else { 0 };
                    holding = true;
                } else if alt {
                    // §Shape `$0A`: continue + falling + alternate
                    // (no hold) — flip and start rising. Step 1 is
                    // the first rising step after the floor.
                    step = 1;
                } else {
                    // §Shape `$08`: continue + falling sawtooth —
                    // wrap to 31 and fall again.
                    step = 31;
                }
            }
        }
        self.envelope.step = step;
        self.envelope.attacked = attacked;
        self.envelope.holding = holding;
    }

    pub fn output(&self) -> f32 {
        let mut sum = 0.0f32;
        // §Sound: mixer enable byte at register 7. Low three bits =
        // tone-disable, high three bits = noise-disable. Both active
        // high (bit set = disabled). When both bits are set for the
        // channel, the channel still outputs a constant DC at the
        // configured volume.
        let r7 = self.regs[7];
        let noise_bit = (self.noise.lfsr & 1) as u8;
        for (i, ch) in self.channels.iter().enumerate() {
            let tone_dis = (r7 >> i) & 1 == 1;
            let noise_dis = (r7 >> (i + 3)) & 1 == 1;
            // §Sound: per-channel signal is one of tone, noise,
            // tone-AND-noise, or constant (when both are disabled).
            let signal: u8 = match (tone_dis, noise_dis) {
                (false, true) => ch.level,
                (true, false) => noise_bit,
                (false, false) => ch.level & noise_bit,
                (true, true) => 1,
            };
            // §Sound: bit 4 of `$08`..=`$0A` routes the envelope
            // generator; otherwise the 4-bit volume in bits 3..0.
            let vol_reg = self.regs[8 + i];
            let env_route = (vol_reg & 0x10) != 0;
            let amp_lin = if env_route {
                S5B_ENV_LIN[self.envelope.step as usize]
            } else {
                LIN_AY_VOL[(vol_reg & 0x0F) as usize]
            };
            sum += if signal != 0 { amp_lin } else { 0.0 };
        }
        sum / 3.0
    }
}

/// 16-step logarithmic DAC table for the 4-bit volume register, in
/// linear amplitude. Each step is 1.5 dB louder than the previous
/// per §Output; the table is normalised so step 15 lands near the
/// chip's peak and step 0 is silent.
const LIN_AY_VOL: [f32; 16] = [
    0.0, 0.011, 0.022, 0.033, 0.046, 0.066, 0.094, 0.133, 0.188, 0.265, 0.375, 0.529, 0.747, 1.057,
    1.494, 2.114,
];

/// 32-step envelope DAC table per §Output: envelope steps 0 and 1
/// both map to silence (volume 0); envelope step `2k+1` and `2k+2`
/// map to volume `k`. Intermediate odd entries are independently
/// interpolated at 0.75 dB to match the §Output 1.5 dB-per-volume
/// step / 0.75 dB-per-envelope step rule.
const S5B_ENV_LIN: [f32; 32] = [
    0.0, 0.0, 0.0095, 0.011, 0.018, 0.022, 0.027, 0.033, 0.039, 0.046, 0.055, 0.066, 0.079, 0.094,
    0.111, 0.133, 0.158, 0.188, 0.224, 0.265, 0.316, 0.375, 0.447, 0.529, 0.629, 0.747, 0.889,
    1.057, 1.257, 1.494, 1.776, 2.114,
];

// ---------------------------------------------------------------- N163

/// Namco 163 wavetable synthesis. Up to 8 channels share a 128-byte
/// wave RAM. Round 11 (per `docs/audio/nsf/namco-163-audio-wiki.html`
/// §Channel Update + §Frequency + §Waveform): every 15 CPU cycles the
/// chip updates *one* enabled channel — it adds the channel's 18-bit
/// frequency to its 24-bit phase, computes one 4-bit signed sample
/// (`sample - 8`) scaled by the 4-bit linear volume, and *holds* that
/// `last_output` until the next channel-update tick. The phase
/// accumulator is stored back into the live RAM at `$79`/`$7B`/`$7D`
/// (for ch8; the other channels mirror the layout 8 bytes earlier per
/// channel index — see `chan_base`). Channels are enabled top-down:
/// the `1+C` field in `$7F` selects channels `9-N..=8`.
pub struct N163 {
    pub enabled: bool,
    pub addr: u8,
    pub addr_inc: bool,
    pub ram: [u8; 0x80],
    /// Number of currently-enabled channels (1..=8). When 1, only
    /// channel 8 plays; when 8, channels 1..=8 all play.
    pub channels_active: u8,
    /// Per-channel-update cycle accumulator. The chip clocks every
    /// 15 CPU cycles regardless of how many channels are enabled.
    pub cycle_accum: u32,
    /// Index (0-based, 0..=channels_active-1) of the channel that
    /// will tick next inside the active set. Round-robin.
    pub next_chan_slot: u8,
    /// Sample-and-hold register: the chip drives a single shared DAC
    /// at the channel-update rate, so the audible output is the last
    /// channel's update. Sum-of-active-channels matches the audible
    /// average since outputs alternate at the update rate.
    pub last_output: f32,
}

impl Default for N163 {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: 0,
            addr_inc: false,
            ram: [0u8; 0x80],
            channels_active: 1,
            cycle_accum: 0,
            next_chan_slot: 0,
            last_output: 0.0,
        }
    }
}

impl N163 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Base RAM offset of channel `idx_one_based` (1..=8): ch1 at
    /// `$40`, ch2 at `$48`, ..., ch8 at `$78`. Per the nesdev wiki
    /// §"Other Channels" — each channel's register block is 8 bytes
    /// before the next-higher channel.
    #[inline]
    fn chan_base(idx_one_based: u8) -> usize {
        debug_assert!((1..=8).contains(&idx_one_based));
        0x40 + (idx_one_based as usize - 1) * 8
    }

    /// One-based channel index for the `slot`th active channel
    /// (0..=channels_active-1). Active channels are the highest
    /// `channels_active` channels per the `1+C` field on `$7F`, so
    /// `slot=0 → channel (9 - channels_active)`, `slot=channels_active-1
    /// → channel 8`.
    #[inline]
    fn active_channel(&self, slot: u8) -> u8 {
        debug_assert!(slot < self.channels_active);
        9 - self.channels_active + slot
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xF800 => {
                self.addr = value & 0x7F;
                self.addr_inc = value & 0x80 != 0;
            }
            0x4800 => {
                let target = self.addr as usize;
                self.ram[target] = value;
                if self.addr_inc {
                    // Per nesdev §Address Port: the address does NOT
                    // wrap; it stops at $7F.
                    if self.addr < 0x7F {
                        self.addr += 1;
                    }
                }
                // The control byte at $7F encodes `1+C` in the high
                // nibble (bits 6-4 hold the C value). Decode whenever
                // the program touches $7F.
                if target == 0x7F {
                    self.channels_active = ((value >> 4) & 0x07) + 1;
                    // If the next-tick pointer is now past the end
                    // of the active set, wrap it back to slot 0.
                    if self.next_chan_slot >= self.channels_active {
                        self.next_chan_slot = 0;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            // The hardware read-port also auto-increments the pointer
            // when `addr_inc` is set, but `Expansion::read` takes
            // `&self`, so the increment lives on the writable
            // mirror path; reads here are non-mutating.
            0x4800 => self.ram[self.addr as usize],
            _ => 0xFF,
        }
    }

    /// Decode the 18-bit frequency, 24-bit phase, wave length, wave
    /// address, and linear volume for one channel from sound RAM.
    /// Layout (for ch8; other channels mirror the layout, see
    /// `chan_base`):
    ///
    /// * `$78` low frequency (bits 0-7)
    /// * `$79` low phase
    /// * `$7A` mid frequency (bits 8-15)
    /// * `$7B` mid phase
    /// * `$7C` `LLLLLLFF` — high 6 bits = wave length, low 2 = high freq bits
    /// * `$7D` high phase
    /// * `$7E` wave address (in 4-bit samples)
    /// * `$7F` `.CCCVVVV` — C = enabled-1, V = linear volume
    fn decode_channel(&self, ch: u8) -> N163Channel {
        let base = Self::chan_base(ch);
        let freq = (self.ram[base] as u32)
            | ((self.ram[base + 2] as u32) << 8)
            | (((self.ram[base + 4] & 0x03) as u32) << 16);
        let phase = (self.ram[base + 1] as u32)
            | ((self.ram[base + 3] as u32) << 8)
            | ((self.ram[base + 5] as u32) << 16);
        // wave_len in 4-bit samples: 256 - (L bits << 2). The L field
        // is bits 7-2 of $7C, so `value & 0xFC` already has the trailing
        // two zeros and the math is "256 - (raw & 0xFC)" → 4..=256.
        let wave_len = 256u32 - (self.ram[base + 4] & 0xFC) as u32;
        let wave_addr = self.ram[base + 6];
        let volume = self.ram[base + 7] & 0x0F;
        N163Channel {
            base,
            freq,
            phase,
            wave_len,
            wave_addr,
            volume,
        }
    }

    /// Read a 4-bit sample at nibble index `pos` (mod 256) out of the
    /// 128-byte sound RAM. Two samples per byte; the low nibble is the
    /// even-indexed sample, the high nibble is the odd-indexed.
    #[inline]
    fn read_nibble(&self, pos: u32) -> u8 {
        let nibble_index = (pos & 0xFF) as usize;
        let byte = self.ram[(nibble_index >> 1) & 0x7F];
        if nibble_index & 1 == 0 {
            byte & 0x0F
        } else {
            byte >> 4
        }
    }

    /// Advance one channel by one update — adds `freq` to `phase`,
    /// wraps mod `wave_len << 16`, recomputes the held output sample,
    /// and writes the updated phase back into sound RAM. Per
    /// §"Channel Update" — speculative single-channel update.
    fn tick_one_channel(&mut self) {
        if self.channels_active == 0 {
            return;
        }
        let slot = self.next_chan_slot;
        let ch = self.active_channel(slot);
        let dec = self.decode_channel(ch);

        let modulus = dec.wave_len << 16;
        let new_phase = if modulus == 0 {
            0
        } else {
            (dec.phase + dec.freq) % modulus
        };

        // Write phase back into RAM at the three phase bytes for this
        // channel. The bit layout matches: low 8 bits → +1, middle 8
        // bits → +3, high 8 bits → +5.
        self.ram[dec.base + 1] = new_phase as u8;
        self.ram[dec.base + 3] = (new_phase >> 8) as u8;
        self.ram[dec.base + 5] = (new_phase >> 16) as u8;

        // sample(((phase >> 16) + wave_addr) & 0xFF)
        let pos = (new_phase >> 16) + dec.wave_addr as u32;
        let nib = self.read_nibble(pos);
        let signed = nib as i32 - 8;
        // Output range: signed in -8..=7, volume in 0..=15 → -120..=105.
        // Scale into a roughly unity envelope — the wiki specifies
        // `(sample - 8) * volume` as the unscaled DAC, which a real
        // 163 outputs through an external resistor ladder. Normalise
        // here so the linear mixer in `Expansion::output` stays in
        // a reasonable range.
        self.last_output = signed as f32 * dec.volume as f32 / 128.0;

        // Round-robin pointer through the active set.
        self.next_chan_slot = (slot + 1) % self.channels_active;
    }

    /// CPU-side tick: the chip updates one channel every 15 CPU
    /// cycles. Batch the work — we only need to call `tick_one_channel`
    /// once per accumulated 15-cycle window.
    pub fn tick(&mut self, cycles: u32) {
        if !self.enabled {
            return;
        }
        self.cycle_accum += cycles;
        while self.cycle_accum >= 15 {
            self.cycle_accum -= 15;
            self.tick_one_channel();
        }
    }

    pub fn output(&self) -> f32 {
        self.last_output
    }
}

/// Decoded view of one N163 channel — pulled out of `&self.ram` once
/// per channel update so the update + phase-writeback paths share the
/// same layout.
struct N163Channel {
    base: usize,
    freq: u32,
    phase: u32,
    wave_len: u32,
    wave_addr: u8,
    volume: u8,
}

// ---------------------------------------------------------------- VRC7

/// The 16 hardwired instrument patches dumped from the VRC7's internal
/// ROM, per the §"Internal patch set" table in
/// `docs/audio/nsf/vrc7-audio-wiki.html`. Slot `0` is the user "custom
/// patch" placeholder — its eight bytes are all `--` (don't-care) and
/// the actual user patch is read from `regs[0x00..=0x07]` at runtime.
/// Slots `1..=15` correspond to the 15 read-only instrument presets
/// ("Buzzy Bell" through "Sweep").
///
/// Byte layout matches the §"Custom Patch" table: `[$00, $01, $02,
/// $03, $04, $05, $06, $07]`. See [`Vrc7Patch::from_bytes`] for the
/// bitfield decode.
pub const VRC7_INSTRUMENT_ROM: [[u8; 8]; 16] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0 (Custom Patch)
    [0x03, 0x21, 0x05, 0x06, 0xE8, 0x81, 0x42, 0x27], // 1 Buzzy Bell
    [0x13, 0x41, 0x14, 0x0D, 0xD8, 0xF6, 0x23, 0x12], // 2 Guitar
    [0x11, 0x11, 0x08, 0x08, 0xFA, 0xB2, 0x20, 0x12], // 3 Wurly
    [0x31, 0x61, 0x0C, 0x07, 0xA8, 0x64, 0x61, 0x27], // 4 Flute
    [0x32, 0x21, 0x1E, 0x06, 0xE1, 0x76, 0x01, 0x28], // 5 Clarinet
    [0x02, 0x01, 0x06, 0x00, 0xA3, 0xE2, 0xF4, 0xF4], // 6 Synth
    [0x21, 0x61, 0x1D, 0x07, 0x82, 0x81, 0x11, 0x07], // 7 Trumpet
    [0x23, 0x21, 0x22, 0x17, 0xA2, 0x72, 0x01, 0x17], // 8 Organ
    [0x35, 0x11, 0x25, 0x00, 0x40, 0x73, 0x72, 0x01], // 9 Bells
    [0xB5, 0x01, 0x0F, 0x0F, 0xA8, 0xA5, 0x51, 0x02], // A Vibes
    [0x17, 0xC1, 0x24, 0x07, 0xF8, 0xF8, 0x22, 0x12], // B Vibraphone
    [0x71, 0x23, 0x11, 0x06, 0x65, 0x74, 0x18, 0x16], // C Tutti
    [0x01, 0x02, 0xD3, 0x05, 0xC9, 0x95, 0x03, 0x02], // D Fretless
    [0x61, 0x63, 0x0C, 0x00, 0x94, 0xC0, 0x33, 0xF6], // E Synth Bass
    [0x21, 0x72, 0x0D, 0x00, 0xC1, 0xD5, 0x56, 0x06], // F Sweep
];

/// Decoded 2-operator patch parameters per the §"Custom Patch"
/// table in `docs/audio/nsf/vrc7-audio-wiki.html`. The patch defines
/// one modulator + one carrier; bytes `$00`/`$04`/`$06` describe the
/// modulator, bytes `$01`/`$05`/`$07` describe the carrier, and bytes
/// `$02`/`$03` mix global parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vrc7Patch {
    // ---- $00 / $01: modulator + carrier "TVSKMMMM"
    /// `$00 T` modulator tremolo enable.
    pub mod_tremolo: bool,
    /// `$00 V` modulator vibrato enable.
    pub mod_vibrato: bool,
    /// `$00 S` modulator sustain enable.
    pub mod_sustain: bool,
    /// `$00 K` modulator key-rate-scaling enable.
    pub mod_ksr: bool,
    /// `$00 M` modulator 4-bit fmult.
    pub mod_mult: u8,

    /// `$01 T` carrier tremolo enable.
    pub car_tremolo: bool,
    /// `$01 V` carrier vibrato enable.
    pub car_vibrato: bool,
    /// `$01 S` carrier sustain enable.
    pub car_sustain: bool,
    /// `$01 K` carrier key-rate-scaling enable.
    pub car_ksr: bool,
    /// `$01 M` carrier 4-bit fmult.
    pub car_mult: u8,

    // ---- $02: modulator KKOOOOOO
    /// `$02 KK` modulator key-level-scaling (0..=3).
    pub mod_ksl: u8,
    /// `$02 OOOOOO` modulator output level (0..=63, 0.75 dB per step).
    pub mod_tl: u8,

    // ---- $03: carrier KK-QWFFF
    /// `$03 KK` carrier key-level-scaling (0..=3).
    pub car_ksl: u8,
    /// `$03 Q` carrier waveform: 0 = sine, 1 = half-rectified sine.
    pub car_wave: u8,
    /// `$03 W` modulator waveform: 0 = sine, 1 = half-rectified sine.
    pub mod_wave: u8,
    /// `$03 FFF` modulator feedback (0..=7).
    pub feedback: u8,

    // ---- $04 / $05: AAAA DDDD
    /// `$04 AAAA` modulator attack rate.
    pub mod_attack: u8,
    /// `$04 DDDD` modulator decay rate.
    pub mod_decay: u8,
    /// `$05 AAAA` carrier attack rate.
    pub car_attack: u8,
    /// `$05 DDDD` carrier decay rate.
    pub car_decay: u8,

    // ---- $06 / $07: SSSS RRRR
    /// `$06 SSSS` modulator sustain level (0=loudest, 15=lowest, 3 dB
    /// per step).
    pub mod_sustain_level: u8,
    /// `$06 RRRR` modulator release rate.
    pub mod_release: u8,
    /// `$07 SSSS` carrier sustain level.
    pub car_sustain_level: u8,
    /// `$07 RRRR` carrier release rate.
    pub car_release: u8,
}

impl Vrc7Patch {
    /// Decode the 8-byte patch table per §"Custom Patch" bitfield
    /// layout — the same format used both for the user-programmable
    /// `regs[0x00..=0x07]` patch and for every entry in
    /// [`VRC7_INSTRUMENT_ROM`].
    pub fn from_bytes(b: &[u8; 8]) -> Self {
        Self {
            // $00 — TVSKMMMM
            mod_tremolo: b[0] & 0x80 != 0,
            mod_vibrato: b[0] & 0x40 != 0,
            mod_sustain: b[0] & 0x20 != 0,
            mod_ksr: b[0] & 0x10 != 0,
            mod_mult: b[0] & 0x0F,
            // $01 — TVSKMMMM
            car_tremolo: b[1] & 0x80 != 0,
            car_vibrato: b[1] & 0x40 != 0,
            car_sustain: b[1] & 0x20 != 0,
            car_ksr: b[1] & 0x10 != 0,
            car_mult: b[1] & 0x0F,
            // $02 — KKOOOOOO
            mod_ksl: (b[2] >> 6) & 0x03,
            mod_tl: b[2] & 0x3F,
            // $03 — KK-QWFFF: bits 7-6 KSL, bit 5 unused, bit 4 carrier
            // waveform Q, bit 3 modulator waveform W, bits 2-0 feedback.
            car_ksl: (b[3] >> 6) & 0x03,
            car_wave: (b[3] >> 4) & 0x01,
            mod_wave: (b[3] >> 3) & 0x01,
            feedback: b[3] & 0x07,
            // $04 — AAAA DDDD
            mod_attack: (b[4] >> 4) & 0x0F,
            mod_decay: b[4] & 0x0F,
            // $05
            car_attack: (b[5] >> 4) & 0x0F,
            car_decay: b[5] & 0x0F,
            // $06 — SSSS RRRR
            mod_sustain_level: (b[6] >> 4) & 0x0F,
            mod_release: b[6] & 0x0F,
            // $07
            car_sustain_level: (b[7] >> 4) & 0x0F,
            car_release: b[7] & 0x0F,
        }
    }
}

/// VRC7 is a stripped Yamaha YM2413 (OPLL): 6 FM channels, no rhythm.
///
/// Round 2 shipped a coarse approximation: channel volumes and
/// fundamental frequencies are honoured; the FM operator math uses a
/// 2-operator sinusoidal stand-in instead of OPLL's logarithmic LUTs.
///
/// Round 13 adds patch decoding — the hardwired §"Internal patch set"
/// ROM (15 named instruments + slot 0 user-programmable) is exposed
/// as [`VRC7_INSTRUMENT_ROM`], the user-programmable patch at
/// `regs[0x00..=0x07]` and each ROM slot decode to a [`Vrc7Patch`]
/// struct, and each channel's `$3X` high nibble selects the active
/// patch. The audible signal path still uses the sinusoidal
/// stand-in, but [`Vrc7Chan::patch_index`] / [`Vrc7::patch`] /
/// [`Vrc7::active_patch`] make the patch table testable and unblock
/// a real OPLL operator implementation (#861) without a further API
/// break.
///
/// Real bit-exact OPLL is still deferred; what we ship plays
/// VRC7-flagged NSFs at the correct pitch and per-channel volume,
/// with the correct patch-selection plumbing.
pub struct Vrc7 {
    pub enabled: bool,
    pub addr: u8,
    pub regs: [u8; 0x40],
    pub channels: [Vrc7Chan; 6],
}

impl Default for Vrc7 {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: 0,
            regs: [0u8; 0x40],
            channels: [Vrc7Chan::default(); 6],
        }
    }
}

#[derive(Default, Clone, Copy)]
pub struct Vrc7Chan {
    pub fnum: u16,
    pub block: u8,
    pub key_on: bool,
    pub volume: u8,
    pub phase: f32,
    /// `$2X` bit 5 (S) — when set, overrides the patch's release rate
    /// with the value `$5` per §Channels. Cached here so it survives
    /// a later `$3X`-only write.
    pub sustain: bool,
    /// `$3X` bits 7-4 (I) — instrument index `0..=15`. Slot 0 selects
    /// the user-programmable patch at `regs[0x00..=0x07]`; slots
    /// `1..=15` select an entry from [`VRC7_INSTRUMENT_ROM`].
    pub patch_index: u8,
}

impl Vrc7 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x9010 => self.addr = value & 0x3F,
            0x9030 => {
                let a = self.addr as usize;
                self.regs[a] = value;
                self.refresh_from_regs();
            }
            _ => {}
        }
    }

    fn refresh_from_regs(&mut self) {
        for ch in 0..6 {
            self.channels[ch].fnum =
                (self.regs[0x10 + ch] as u16) | (((self.regs[0x20 + ch] & 0x01) as u16) << 8);
            self.channels[ch].block = (self.regs[0x20 + ch] >> 1) & 0x07;
            // $2X bitfield --STOOOH: bit 4 = trigger / key-on,
            // bit 5 = sustain override (§Channels).
            self.channels[ch].key_on = self.regs[0x20 + ch] & 0x10 != 0;
            self.channels[ch].sustain = self.regs[0x20 + ch] & 0x20 != 0;
            // $3X bitfield IIIIVVVV: high nibble = instrument index,
            // low nibble = inverted volume.
            self.channels[ch].patch_index = (self.regs[0x30 + ch] >> 4) & 0x0F;
            self.channels[ch].volume = self.regs[0x30 + ch] & 0x0F;
        }
    }

    /// Return the decoded patch parameters for instrument slot
    /// `index`. Slot `0` reads from the user-programmable
    /// `regs[0x00..=0x07]`; slots `1..=15` read from
    /// [`VRC7_INSTRUMENT_ROM`]. Indices `>= 16` wrap modulo 16 (the
    /// `$3X` instrument field is only 4 bits wide so this is a
    /// defensive default, never a real write).
    pub fn patch(&self, index: u8) -> Vrc7Patch {
        let i = (index as usize) & 0x0F;
        if i == 0 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&self.regs[0x00..0x08]);
            Vrc7Patch::from_bytes(&b)
        } else {
            Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[i])
        }
    }

    /// Return the currently selected patch for channel `ch` (0..=5).
    pub fn active_patch(&self, ch: usize) -> Vrc7Patch {
        self.patch(self.channels[ch].patch_index)
    }

    pub fn tick(&mut self, cycles: u32) {
        // The OPLL master clock divides by 72 to reach the 49.7 kHz
        // operator clock. We approximate by stepping each channel's
        // phase accumulator linearly here.
        let dt = cycles as f32 / 1_789_773.0; // seconds
        for ch in &mut self.channels {
            if !ch.key_on || ch.fnum == 0 {
                continue;
            }
            // f = fnum * (2 ^ block) * 49716 / 2^19 (per OPLL datasheet).
            let f = ch.fnum as f32 * (1u32 << ch.block) as f32 * 49716.0 / 524288.0;
            ch.phase = (ch.phase + f * dt).fract();
        }
    }

    pub fn output(&self) -> f32 {
        let mut sum = 0.0f32;
        let mut active = 0u32;
        for ch in &self.channels {
            if !ch.key_on || ch.volume >= 0x0F {
                continue;
            }
            let amp = (1.0 - ch.volume as f32 / 15.0) * 0.25;
            sum += (ch.phase * std::f32::consts::TAU).sin() * amp;
            active += 1;
        }
        if active == 0 {
            return 0.0;
        }
        sum / active as f32
    }
}

// ---------------------------------------------------------------- FDS

/// Famicom Disk System: a single wavetable synth with a frequency
/// modulator. Wavetable RAM (64 6-bit samples) lives at
/// `$4040..=$407F`.
///
/// Round 7 wires the frequency-modulation unit per
/// `docs/audio/nsf/fds-audio-wiki.html` §"Modulation unit" +
/// §"Frequency calculation and timing": both the modulation unit and
/// the wave output unit advance their accumulators every 16 CPU
/// cycles. The mod accumulator adds the 12-bit modulation frequency
/// each tick and, on a carry out of bit 11, steps the 32-entry mod
/// table (each entry applied twice via the unused LSB of a 64-step
/// pointer) and updates the signed 7-bit mod counter by the table's
/// `{0,+1,+2,+4,reset,-4,-2,-1}` increment. The mod counter, the
/// 6-bit mod gain (`$4084`) and the 12-bit pitch (`$4082/$4083`) feed
/// the documented pitch formula to produce a 20-bit `wave_pitch`,
/// which the wave output unit adds to its accumulator each wave tick.
/// The wave position is the top 6 bits of that accumulator. Previously
/// the wave always advanced at the raw, unmodulated pitch, so FDS
/// vibrato/modulation was inaudible.
pub struct Fds {
    pub enabled: bool,
    pub wave: [u8; 64],
    pub mod_table: [u8; 32],
    pub volume: u8,
    pub master_volume_div: u8,
    pub freq: u16,
    pub mod_freq: u16,
    pub mod_disabled: bool,
    /// 6-bit mod gain from `$4084` (bits 0-5).
    pub mod_gain: u8,
    /// Signed 7-bit mod counter (`$4085`), range -64..=63.
    pub mod_counter: i8,
    /// 64-step mod-table playback pointer; the LSB is ignored when
    /// indexing the 32-entry table, so each entry is applied twice.
    pub mod_pos: u8,
    /// Mod accumulator low 12 bits — the 12-bit `mod_freq` is added
    /// each mod tick; a carry out of bit 11 steps the mod table.
    pub mod_acc: u16,
    pub wave_pos: u8,
    /// Wave accumulator (20 fractional bits below the 6-bit position);
    /// `wave_pitch` is added each wave tick.
    pub wave_acc: u32,
    pub wave_write_enable: bool,
    /// CPU-cycle remainder toward the next 16-cycle unit tick.
    pub cycle_acc: u32,

    // ---- envelope ramp generators (`$4080` / `$4084` / `$408A` / `$4083`)
    /// 6-bit volume-envelope speed `e` from `$4080` (set regardless of
    /// the mode bit).
    pub vol_env_speed: u8,
    /// `$4080` bit 6: 0 = decrease, 1 = increase.
    pub vol_env_increase: bool,
    /// `$4080` bit 7 (mode): true = envelope disabled (gain set
    /// directly), false = the ramp runs.
    pub vol_env_disabled: bool,
    /// CPU clocks remaining until the next volume-envelope tick.
    pub vol_env_timer: u32,
    /// 6-bit mod-envelope speed `e` from `$4084`.
    pub mod_env_speed: u8,
    /// `$4084` bit 6 direction.
    pub mod_env_increase: bool,
    /// `$4084` bit 7 (mode): true = mod gain set directly, no ramp.
    pub mod_env_disabled: bool,
    /// CPU clocks remaining until the next mod-envelope tick.
    pub mod_env_timer: u32,
    /// 8-bit master envelope speed `m` from `$408A` (0 disables both
    /// envelopes); BIOS initial value is `$E8`.
    pub master_env_speed: u8,
    /// `$4083` bit 7: run both envelopes 4x faster.
    pub env_fast: bool,
    /// `$4083` bit 6: halt both envelopes (and reset their timers).
    pub env_halt: bool,
    /// Volume gain pending latch: the §"Unit tick" PWM unit only commits
    /// a volume-gain change while the wave position is 0, so a ramp step
    /// stages the new gain here and `output`/`tick` commits it once
    /// `wave_pos == 0`. `None` means "no change pending".
    pub vol_pending: Option<u8>,
    /// `$4023` bit 1 (master sound enable). Per §"Master I/O enable", the
    /// sound registers only function while this bit is set; the BIOS
    /// writes `$00` then `$83`. While clear the waveform is halted — the
    /// wave + mod accumulators stop advancing, the wave position is frozen
    /// at 0 so the channel outputs the constant `$4040` value, and the
    /// envelopes are not ticked (§"Frequency high" halt note). Writes to
    /// `$4080` / `$4089` still affect the held output. Defaults to `true`
    /// so a rip that relies on the BIOS having already enabled sound (or
    /// that never re-writes `$4023`) still plays.
    pub sound_enabled: bool,
}

impl Default for Fds {
    fn default() -> Self {
        Self {
            enabled: false,
            wave: [0u8; 64],
            mod_table: [0u8; 32],
            volume: 0,
            master_volume_div: 0,
            freq: 0,
            mod_freq: 0,
            mod_disabled: true,
            mod_gain: 0,
            mod_counter: 0,
            mod_pos: 0,
            mod_acc: 0,
            wave_pos: 0,
            wave_acc: 0,
            wave_write_enable: false,
            cycle_acc: 0,
            vol_env_speed: 0,
            vol_env_increase: false,
            vol_env_disabled: true,
            vol_env_timer: 0,
            mod_env_speed: 0,
            mod_env_increase: false,
            mod_env_disabled: true,
            mod_env_timer: 0,
            // BIOS initialises $408A to $E8; default to that so a rip
            // that never writes it still ticks the envelopes.
            master_env_speed: 0xE8,
            env_fast: false,
            env_halt: false,
            vol_pending: None,
            // The BIOS enables sound ($4023 = $83) before any music runs,
            // so default to enabled for rips that never touch $4023.
            sound_enabled: true,
        }
    }
}

impl Fds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x4023 => {
                // Master I/O enable: bit 1 (S) gates the sound channel.
                // Per §"Master I/O enable" the sound registers only
                // function while it is set. Entering the halted state
                // (bit clear) freezes the wave position at 0 so the
                // channel holds the constant `$4040` value.
                let now = value & 0x02 != 0;
                if !now {
                    // Reset the wave position to the $4040 sample. The
                    // accumulator is parked too so the next enable starts
                    // a fresh wave period rather than mid-step.
                    self.wave_pos = 0;
                    self.wave_acc = 0;
                }
                self.sound_enabled = now;
            }
            0x4040..=0x407F if self.wave_write_enable => {
                self.wave[(addr - 0x4040) as usize] = value & 0x3F;
            }
            0x4080 => {
                // Volume envelope ($4080): MDVV VVVV. The 6-bit speed is
                // latched whether or not the ramp is enabled; the volume
                // gain is set directly only when the mode bit (M) is high.
                self.vol_env_speed = value & 0x3F;
                self.vol_env_increase = value & 0x40 != 0;
                self.vol_env_disabled = value & 0x80 != 0;
                if self.vol_env_disabled {
                    // Direct gain write. Muting (gain 0) takes effect
                    // immediately; a non-zero gain still honours the
                    // wave-position-0 PWM latch.
                    let g = value & 0x3F;
                    if g == 0 {
                        self.volume = 0;
                        self.vol_pending = None;
                    } else {
                        self.set_volume_gain(g);
                    }
                }
                // Writing resets this unit's tick timer.
                self.vol_env_timer = self.vol_env_period();
            }
            0x4082 => {
                self.freq = (self.freq & 0x0F00) | value as u16;
            }
            0x4083 => {
                self.freq = (self.freq & 0x00FF) | (((value & 0x0F) as u16) << 8);
                // Bit 6 halts both envelopes and resets their timers;
                // bit 7 runs both envelopes 4x faster.
                self.env_halt = value & 0x40 != 0;
                self.env_fast = value & 0x80 != 0;
                if self.env_halt {
                    self.vol_env_timer = self.vol_env_period();
                    self.mod_env_timer = self.mod_env_period();
                }
            }
            0x4084 => {
                // Mod envelope ($4084): MDSS SSSS — same layout as the
                // volume envelope. The speed is always latched; the mod
                // gain is set directly only when the mode bit is high.
                self.mod_env_speed = value & 0x3F;
                self.mod_env_increase = value & 0x40 != 0;
                self.mod_env_disabled = value & 0x80 != 0;
                if self.mod_env_disabled {
                    self.mod_gain = value & 0x3F;
                }
                self.mod_env_timer = self.mod_env_period();
            }
            0x4085 => {
                // Directly set the signed 7-bit mod counter.
                self.mod_counter = Self::to_signed7(value & 0x7F);
            }
            0x4086 => {
                self.mod_freq = (self.mod_freq & 0x0F00) | value as u16;
            }
            0x4087 => {
                self.mod_freq = (self.mod_freq & 0x00FF) | (((value & 0x0F) as u16) << 8);
                self.mod_disabled = value & 0x80 != 0;
                if self.mod_disabled {
                    // Reset mod accumulator (bits 0-12 forced to 0); the
                    // mod-table position address is left unaltered.
                    self.mod_acc = 0;
                }
            }
            0x4088 if self.mod_disabled => {
                // Mod-table write — only honoured while the mod unit is
                // disabled ($4087 bit 7). Replaces the entry at the
                // current position, then advances the 64-step pointer.
                let p = ((self.mod_pos >> 1) & 0x1F) as usize;
                self.mod_table[p] = value & 0x07;
                self.mod_pos = self.mod_pos.wrapping_add(2) & 0x3F;
            }
            0x4089 => {
                self.master_volume_div = value & 0x03;
                self.wave_write_enable = value & 0x80 != 0;
            }
            0x408A => {
                // Master envelope speed multiplier (`m`); 0 disables both
                // envelopes. The new value takes effect on the next tick.
                self.master_env_speed = value;
            }
            _ => {}
        }
    }

    /// CPU-side read of the FDS status registers at `$4090..=$4097`
    /// (write-only registers and unmapped addresses return `0xFF`,
    /// matching the bus open-bus default). Per §"Volume gain ($4090)" …
    /// §"Mod counter value ($4097)" in `docs/audio/nsf/fds-audio-wiki.html`,
    /// these mirror live internal state of the wave + mod units and
    /// preserve the documented open-bus pattern in the top bits
    /// (`01` for `$4090` / `$4092` / `$4096`; `0` for `$4093` / `$4097`).
    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            // Volume gain: top bits "01" (open bus), bottom 6 bits = volume.
            0x4090 => 0x40 | (self.volume & 0x3F),
            // Wave accumulator: bits 12-19 of the 24-bit `wave_acc`.
            0x4091 => ((self.wave_acc >> 12) & 0xFF) as u8,
            // Mod gain: top bits "01" (open bus), bottom 6 bits = mod_gain.
            0x4092 => 0x40 | (self.mod_gain & 0x3F),
            // Mod table address accumulator: bits 5-11 of the 12-bit `mod_acc`
            // (the mod-table address proper sits in bits 13-17 = `mod_pos`,
            // outside this read window). Top bit returns 0 per open bus.
            0x4093 => ((self.mod_acc >> 5) & 0x7F) as u8,
            // Mod counter * gain intermediate: bits 4-11 of `counter * gain`.
            // The sequential multiplier exposes its 16-bit accumulator here;
            // we sample the final product directly since the multiplier
            // completes within a single 16-CPU-cycle unit tick.
            0x4094 => {
                let product = (self.mod_counter as i32) * (self.mod_gain as i32);
                ((product >> 4) & 0xFF) as u8
            }
            // Next mod-counter increment: the mod-table entry at the current
            // pointer, translated into a 4-bit twos-complement display value
            // (0,1,2,3,4,5,6,7 → 0,1,2,4,C,C,E,F) per §"Mod counter
            // increment ($4095)". Top nibble is documented as "Unknown
            // counter" — return 0.
            0x4095 => {
                let entry = self.mod_table[((self.mod_pos >> 1) & 0x1F) as usize] & 0x07;
                Self::mod_increment_display(entry)
            }
            // Wavetable value at the current position, masked by the PWM
            // volume envelope (the wave-position-0 PWM latch already keeps
            // `self.volume` consistent with what is being fed to the DAC).
            // Per §"Wavetable value ($4096)" the field is the raw sample,
            // not the gain-scaled output; top bits "01" (open bus).
            0x4096 => 0x40 | (self.wave[self.wave_pos as usize] & 0x3F),
            // Mod counter value: signed 7-bit, top bit returns 0 per open bus.
            0x4097 => (self.mod_counter as u8) & 0x7F,
            _ => 0xFF,
        }
    }

    /// Display form of a mod-table entry for `$4095`. Maps the 3-bit
    /// table entry into the 4-bit twos-complement increment the register
    /// shows (per §"Mod counter increment ($4095)"):
    /// `0→0, 1→1, 2→2, 3→4, 4→C, 5→C, 6→E, 7→F`.
    fn mod_increment_display(entry: u8) -> u8 {
        match entry & 0x07 {
            0 => 0x0,
            1 => 0x1,
            2 => 0x2,
            3 => 0x4,
            4 => 0xC, // reset → renders as -4 in 4-bit twos-complement
            5 => 0xC, // -4
            6 => 0xE, // -2
            _ => 0xF, // 7 → -1
        }
    }

    /// CPU clocks per volume-envelope tick: `c = 8 * (e + 1) * (m + 1)`,
    /// halved twice (÷4) when the `$4083` fast bit is set. Returns 0 when
    /// the master speed disables the envelopes.
    fn vol_env_period(&self) -> u32 {
        Self::env_period(self.vol_env_speed, self.master_env_speed, self.env_fast)
    }

    /// CPU clocks per mod-envelope tick (same formula as the volume
    /// envelope but driven from the mod speed register).
    fn mod_env_period(&self) -> u32 {
        Self::env_period(self.mod_env_speed, self.master_env_speed, self.env_fast)
    }

    /// Shared envelope-period formula from §"Frequency calculation and
    /// timing → Envelopes": `c = 8 * (e + 1) * (m + 1)`. `m == 0`
    /// disables the envelopes (returns 0). The `$4083` fast bit runs them
    /// 4x faster.
    fn env_period(e: u8, m: u8, fast: bool) -> u32 {
        if m == 0 {
            return 0;
        }
        let c = 8 * (e as u32 + 1) * (m as u32 + 1);
        if fast {
            (c / 4).max(1)
        } else {
            c
        }
    }

    /// Stage a volume-gain change. The §"Unit tick" PWM unit only commits
    /// a gain change while the wave position is 0, so park the new gain in
    /// `vol_pending` and let `commit_pending_volume` apply it.
    fn set_volume_gain(&mut self, gain: u8) {
        if self.wave_pos == 0 {
            self.volume = gain;
            self.vol_pending = None;
        } else {
            self.vol_pending = Some(gain);
        }
    }

    /// Commit a staged volume-gain change once the wave position reaches 0.
    fn commit_pending_volume(&mut self) {
        if self.wave_pos == 0 {
            if let Some(g) = self.vol_pending.take() {
                self.volume = g;
            }
        }
    }

    /// Sign-extend a 7-bit value (`$40` = -64 .. `$3F` = 63).
    fn to_signed7(v: u8) -> i8 {
        let v = v & 0x7F;
        if v & 0x40 != 0 {
            // bit 6 is the sign bit: subtract 128 from the 8-bit value.
            (v as i16 - 0x80) as i8
        } else {
            v as i8
        }
    }

    /// Map a 3-bit mod-table entry to its mod-counter increment per
    /// §"Modulation unit": 0,1,2,3,4,5,6,7 → 0,+1,+2,+4,reset,-4,-2,-1.
    /// `None` means "reset counter to 0".
    fn mod_increment(entry: u8) -> Option<i8> {
        match entry & 0x07 {
            0 => Some(0),
            1 => Some(1),
            2 => Some(2),
            3 => Some(4),
            4 => None, // reset to 0
            5 => Some(-4),
            6 => Some(-2),
            _ => Some(-1), // 7
        }
    }

    /// The obtuse pitch formula from §"Modulation unit": fold the
    /// signed mod counter and mod gain into a 20-bit modulated pitch.
    fn wave_pitch(&self) -> u32 {
        let counter = self.mod_counter as i32;
        let gain = self.mod_gain as i32;
        // 1. multiply counter by gain.
        let mut temp = counter * gain;
        // 2. round up to 6 bits only if sign positive (ignoring bit 4).
        if (temp & 0x0f) != 0 && (temp & 0x800) == 0 {
            temp += 0x20;
        }
        // 3. drop 4 bits and center to 0x40.
        temp += 0x400;
        temp = (temp >> 4) & 0xff;
        // 4. multiply by pitch to get the 20-bit unsigned result.
        ((self.freq as i32 * temp) as u32) & 0xF_FFFF
    }

    /// Advance one 16-CPU-cycle unit tick: step the mod unit (when
    /// enabled), then the wave output unit at the modulated pitch.
    fn unit_tick(&mut self) {
        // Modulation unit: add the 12-bit mod_freq; a carry out of
        // bit 11 steps the mod table and updates the mod counter. The
        // `$4083` 4x-speed bit (`env_fast`) also halts the mod-table
        // accumulator per §"Frequency high".
        if !self.mod_disabled && self.mod_freq != 0 && !self.env_fast {
            let sum = self.mod_acc as u32 + self.mod_freq as u32;
            self.mod_acc = (sum & 0x0FFF) as u16;
            if sum & 0x1000 != 0 {
                // Carry out of bit 11: read the table entry at the
                // current (64-step) pointer, then advance the pointer.
                let entry = self.mod_table[((self.mod_pos >> 1) & 0x1F) as usize];
                match Self::mod_increment(entry) {
                    None => self.mod_counter = 0,
                    Some(d) => {
                        // Signed 7-bit wrap: 63 + 1 → -64, -64 - 1 → 63.
                        let next = (self.mod_counter as i32 + d as i32) & 0x7F;
                        self.mod_counter = Self::to_signed7(next as u8);
                    }
                }
                self.mod_pos = self.mod_pos.wrapping_add(1) & 0x3F;
            }
        }

        // Wave output unit: add the 20-bit modulated pitch into the
        // 24-bit accumulator (6 address bits 18-23 over 18 fractional
        // bits per the §"Wavetables" diagram); the wave position is the
        // top 6 bits.
        if self.freq != 0 {
            self.wave_acc = (self.wave_acc.wrapping_add(self.wave_pitch())) & 0xFF_FFFF;
            self.wave_pos = ((self.wave_acc >> 18) & 0x3F) as u8;
        }
    }

    /// Step the volume + mod envelope ramp generators by `cycles` CPU
    /// clocks. Each envelope counts its own `c = 8·(e+1)·(m+1)` timer; on
    /// underflow it ramps the gain ±1 (clamped 0..=32 on the active edge)
    /// per §"Unit tick → Envelopes". Disabled (master speed 0), halted
    /// (`$4083` bit 6), or mode-bit-set envelopes do not ramp.
    fn env_tick(&mut self, cycles: u32) {
        if self.env_halt || self.master_env_speed == 0 {
            return;
        }
        // Volume envelope.
        if !self.vol_env_disabled {
            let period = self.vol_env_period();
            if period != 0 {
                let mut rem = cycles;
                while rem > 0 {
                    if self.vol_env_timer == 0 {
                        self.vol_env_timer = period;
                    }
                    let step = rem.min(self.vol_env_timer);
                    self.vol_env_timer -= step;
                    rem -= step;
                    if self.vol_env_timer == 0 {
                        self.step_volume_env();
                        self.vol_env_timer = period;
                    }
                }
            }
        }
        // Mod envelope.
        if !self.mod_env_disabled {
            let period = self.mod_env_period();
            if period != 0 {
                let mut rem = cycles;
                while rem > 0 {
                    if self.mod_env_timer == 0 {
                        self.mod_env_timer = period;
                    }
                    let step = rem.min(self.mod_env_timer);
                    self.mod_env_timer -= step;
                    rem -= step;
                    if self.mod_env_timer == 0 {
                        self.step_mod_env();
                        self.mod_env_timer = period;
                    }
                }
            }
        }
    }

    /// One volume-envelope ramp step: increase (if gain < 32, +1) or
    /// decrease (if gain > 0, -1). The change is latched through the
    /// wave-position-0 PWM gate.
    fn step_volume_env(&mut self) {
        let cur = self.vol_pending.unwrap_or(self.volume);
        let next = if self.vol_env_increase {
            if cur < 32 {
                cur + 1
            } else {
                cur
            }
        } else if cur > 0 {
            cur - 1
        } else {
            cur
        };
        if next != cur {
            self.set_volume_gain(next);
        }
    }

    /// One mod-envelope ramp step: increase (if gain < 32, +1) or
    /// decrease (if gain > 0, -1). The mod gain feeds the pitch formula
    /// directly (no PWM latch).
    fn step_mod_env(&mut self) {
        if self.mod_env_increase {
            if self.mod_gain < 32 {
                self.mod_gain += 1;
            }
        } else if self.mod_gain > 0 {
            self.mod_gain -= 1;
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        // `$4023.D1 = 0` halts the waveform: the wave + mod accumulators
        // stop, the wave position holds at 0 (constant `$4040` output) and
        // the envelopes are not ticked (§"Frequency high" halt note).
        // Writes to `$4080` / `$4089` still affect the held output, which
        // is preserved because those register writes update `volume` /
        // `master_volume_div` directly outside this tick.
        if !self.sound_enabled {
            return;
        }
        // Envelope ramp generators run on their own CPU-cycle timers.
        self.env_tick(cycles);
        // Both wave + mod units tick every 16 CPU cycles; accumulate the
        // remainder so non-multiple-of-16 batches stay phase-correct.
        self.cycle_acc += cycles;
        while self.cycle_acc >= 16 {
            self.cycle_acc -= 16;
            self.unit_tick();
            // The wave position advanced; commit any staged volume gain
            // now that we may be at wave position 0.
            self.commit_pending_volume();
        }
    }

    pub fn output(&self) -> f32 {
        let s = self.wave[self.wave_pos as usize] as i32 - 32; // signed -32..31
        let div = match self.master_volume_div {
            0 => 1.0,
            1 => 2.0,
            2 => 3.0,
            _ => 4.0,
        };
        let v = self.volume.min(32) as f32 / 32.0;
        s as f32 / 32.0 * v / div * 0.5
    }
}

// ---------------------------------------------------------------- container

/// Aggregate of every chip we might activate; only the subset enabled
/// in the NSF header gets ticked. The host APU owns one of these.
pub struct Expansion {
    pub flags: ExpansionChips,
    pub vrc6: Vrc6,
    pub vrc7: Vrc7,
    pub mmc5: Mmc5,
    pub n163: N163,
    pub s5b: Sunsoft5b,
    pub fds: Fds,
}

impl Default for Expansion {
    fn default() -> Self {
        Self::new()
    }
}

impl Expansion {
    pub fn new() -> Self {
        Self {
            flags: ExpansionChips(0),
            vrc6: Vrc6::new(),
            vrc7: Vrc7::new(),
            mmc5: Mmc5::new(),
            n163: N163::new(),
            s5b: Sunsoft5b::new(),
            fds: Fds::new(),
        }
    }

    pub fn set_flags(&mut self, flags: ExpansionChips) {
        self.flags = flags;
        self.vrc6.enabled = flags.vrc6();
        self.vrc7.enabled = flags.vrc7();
        self.mmc5.enabled = flags.mmc5();
        self.n163.enabled = flags.n163();
        self.s5b.enabled = flags.s5b();
        self.fds.enabled = flags.fds();
    }

    pub fn tick(&mut self, cycles: u32) {
        if self.vrc6.enabled {
            self.vrc6.tick(cycles);
        }
        if self.vrc7.enabled {
            self.vrc7.tick(cycles);
        }
        if self.mmc5.enabled {
            self.mmc5.tick(cycles);
        }
        if self.s5b.enabled {
            self.s5b.tick(cycles);
        }
        if self.fds.enabled {
            self.fds.tick(cycles);
        }
        if self.n163.enabled {
            self.n163.tick(cycles);
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        // Each chip inspects its own register window; misses are silent.
        if self.vrc6.enabled {
            self.vrc6.write(addr, value);
        }
        if self.vrc7.enabled {
            self.vrc7.write(addr, value);
        }
        if self.mmc5.enabled {
            self.mmc5.write(addr, value);
        }
        if self.n163.enabled {
            self.n163.write(addr, value);
        }
        if self.s5b.enabled {
            self.s5b.write(addr, value);
        }
        if self.fds.enabled {
            self.fds.write(addr, value);
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        if self.mmc5.enabled {
            let v = self.mmc5.read(addr);
            if v != 0xFF {
                return v;
            }
        }
        if self.n163.enabled {
            let v = self.n163.read(addr);
            if v != 0xFF {
                return v;
            }
        }
        if self.fds.enabled {
            let v = self.fds.read(addr);
            if v != 0xFF {
                return v;
            }
        }
        0xFF
    }

    pub fn output(&self) -> f32 {
        let mut o = 0.0f32;
        if self.vrc6.enabled {
            o += self.vrc6.output();
        }
        if self.vrc7.enabled {
            o += self.vrc7.output();
        }
        if self.mmc5.enabled {
            o += self.mmc5.output();
        }
        if self.n163.enabled {
            o += self.n163.output();
        }
        if self.s5b.enabled {
            o += self.s5b.output();
        }
        if self.fds.enabled {
            o += self.fds.output();
        }
        o
    }

    /// Linearly-mixed expansion output with per-device gain applied
    /// from the NSFe `mixe` table. `device_gain` is indexed by the
    /// NSFe device id constants in [`crate::apu::mixe_device`]; only
    /// indexes 2..=7 (VRC6 / VRC7 / FDS / MMC5 / N163 / 5B) are read.
    pub fn output_with_device_gain(&self, device_gain: &[f32; 8]) -> f32 {
        let mut o = 0.0f32;
        if self.vrc6.enabled {
            o += self.vrc6.output() * device_gain[crate::apu::mixe_device::VRC6 as usize];
        }
        if self.vrc7.enabled {
            o += self.vrc7.output() * device_gain[crate::apu::mixe_device::VRC7 as usize];
        }
        if self.mmc5.enabled {
            o += self.mmc5.output() * device_gain[crate::apu::mixe_device::MMC5 as usize];
        }
        if self.n163.enabled {
            o += self.n163.output() * device_gain[crate::apu::mixe_device::N163 as usize];
        }
        if self.s5b.enabled {
            o += self.s5b.output() * device_gain[crate::apu::mixe_device::S5B as usize];
        }
        if self.fds.enabled {
            o += self.fds.output() * device_gain[crate::apu::mixe_device::FDS as usize];
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vrc6_pulse_register_decodes_volume_and_period() {
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x8F); // mode_digital, duty=0, vol=15
        chip.write(0x9001, 0x40);
        chip.write(0x9002, 0x83); // enabled, period high = 0x03
        assert!(chip.pulse[0].mode_digital);
        assert!(chip.pulse[0].enabled);
        assert_eq!(chip.pulse[0].volume, 15);
        assert_eq!(chip.pulse[0].timer_period, 0x340);
    }

    #[test]
    fn mmc5_pulse_status_reports_active_lengths() {
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x03); // enable both
        chip.write(0x5003, 0x08); // pulse 0 length set
        let s = chip.read(0x5015);
        assert_eq!(s & 0x03, 0x01);
    }

    #[test]
    fn s5b_register_indirection_writes_period() {
        let mut chip = Sunsoft5b::new();
        chip.write(0xC000, 0x00); // address = 0
        chip.write(0xE000, 0x42); // R0 = 0x42 (period lo channel A)
        chip.write(0xC000, 0x01);
        chip.write(0xE000, 0x03); // R1 = 0x03 (period hi channel A)
        assert_eq!(chip.channels[0].timer_period, 0x0342);
    }

    /// Helper: write `value` into Sunsoft 5B register `r` using the
    /// `$C000`/`$E000` address-port + data-port sequence.
    fn s5b_write_reg(chip: &mut Sunsoft5b, r: u8, value: u8) {
        chip.write(0xC000, r);
        chip.write(0xE000, value);
    }

    #[test]
    fn s5b_tone_flips_every_two_periods_of_sixteen_clocks() {
        // §Sound: tone counter increments every 16 clocks; flips
        // and resets when counter >= period. With period = 4, the
        // tone level toggles every 4 * 16 = 64 clocks.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0, 0x04); // period lo = 4
        s5b_write_reg(&mut chip, 1, 0x00); // period hi = 0
        assert_eq!(chip.channels[0].timer_period, 4);
        let start = chip.channels[0].level;
        chip.tick(48); // 3 intervals — not enough to flip
        assert_eq!(chip.channels[0].level, start);
        chip.tick(16); // total 64 = 4 intervals — flip
        assert_eq!(chip.channels[0].level, start ^ 1);
        chip.tick(64); // next 64 clocks — flip back
        assert_eq!(chip.channels[0].level, start);
    }

    #[test]
    fn s5b_tone_period_zero_behaves_as_one() {
        // §Sound period-zero note: period 0 acts as period 1, so
        // the tone flips every 16 clocks.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0, 0x00);
        s5b_write_reg(&mut chip, 1, 0x00);
        let start = chip.channels[0].level;
        chip.tick(16);
        assert_eq!(chip.channels[0].level, start ^ 1);
        chip.tick(16);
        assert_eq!(chip.channels[0].level, start);
    }

    #[test]
    fn s5b_noise_lfsr_advances_with_period_one() {
        // §Noise: new random bit every 32 clocks. With period 1 the
        // LFSR advances on every 32-clock boundary; ticking 32
        // clocks must change at least one bit of the LFSR state.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 6, 0x01); // noise period = 1
        let lfsr0 = chip.noise.lfsr;
        chip.tick(32);
        assert_ne!(chip.noise.lfsr, lfsr0);
    }

    #[test]
    fn s5b_noise_lfsr_period_is_full_cycle() {
        // The 17-bit LFSR taps at bits 16 and 13 produce a period
        // of (2^17 - 1) = 131071 states per the §Noise reference.
        // Step the LFSR by hand at period=0 (max rate) for one full
        // expected cycle and confirm we return to the seed exactly
        // once — no shorter sub-cycle.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 6, 0x00); // period 0 → 1
        let seed = chip.noise.lfsr;
        let mut returns = 0u32;
        // Each LFSR advance takes 32 clocks; 131_071 * 32 = 4_194_272.
        // Walk 131_071 advances and count how many times we land on
        // the seed mid-walk (should be exactly once, at the end).
        for _ in 0..131_071 {
            chip.tick(32);
            if chip.noise.lfsr == seed {
                returns += 1;
            }
        }
        assert_eq!(returns, 1);
    }

    #[test]
    fn s5b_envelope_shape_decay_one_shot_holds_silent() {
        // §Shape `$00..$03` — decay one-shot, attack=0, continue=0:
        // ramp falls from 31 to 0 and stays at 0 forever. With
        // period 1 the envelope advances once every 16 clocks. 31
        // step-downs (31 → 0) take 31 ticks; the 32nd tick lands
        // at the low edge and engages the hold.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x00);
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16 * 31);
        assert_eq!(chip.envelope.step, 0);
        assert!(!chip.envelope.holding);
        chip.tick(16); // edge-crossing tick → engages hold
        assert_eq!(chip.envelope.step, 0);
        assert!(chip.envelope.holding);
        chip.tick(16 * 100);
        assert_eq!(chip.envelope.step, 0);
    }

    #[test]
    fn s5b_envelope_shape_sawtooth_falling_wraps_to_top() {
        // §Shape `$08` — continue + falling sawtooth: ramp falls 31
        // → 0, then wraps back to 31 on the edge-crossing tick.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x08);
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16 * 31); // walk to step 0
        assert_eq!(chip.envelope.step, 0);
        assert!(!chip.envelope.holding);
        chip.tick(16); // edge tick — wraps to 31
        assert_eq!(chip.envelope.step, 31);
    }

    #[test]
    fn s5b_envelope_shape_triangle_alternates_direction() {
        // §Shape `$0A` — continue + falling + alternate (no hold):
        // ramp falls 31 → 0, then rises back. No hold.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x0A);
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16 * 31);
        assert_eq!(chip.envelope.step, 0);
        // Edge-crossing tick flips direction and starts at step 1.
        chip.tick(16);
        assert_eq!(chip.envelope.step, 1);
        chip.tick(16 * 30);
        assert_eq!(chip.envelope.step, 31);
        // High-edge crossing flips direction again.
        chip.tick(16);
        assert_eq!(chip.envelope.step, 30);
    }

    #[test]
    fn s5b_envelope_shape_attack_hold_stays_at_top() {
        // §Shape `$0D` — continue + attack + hold (no alternate):
        // ramp rises 0 → 31 and holds at 31 forever.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x0D);
        assert_eq!(chip.envelope.step, 0);
        chip.tick(16 * 31); // walk up to 31
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16); // edge tick → engage hold
        assert!(chip.envelope.holding);
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16 * 100);
        assert_eq!(chip.envelope.step, 31);
    }

    #[test]
    fn s5b_envelope_shape_attack_alternate_hold_flips_at_end() {
        // §Shape `$0F` — continue + attack + alternate + hold:
        // ramp rises 0 → 31, then *immediately flips to 0* per the
        // §Shape table (`/_______`), then holds.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x0F);
        chip.tick(16 * 31);
        assert_eq!(chip.envelope.step, 31);
        chip.tick(16); // edge tick → flip-then-hold
        assert_eq!(chip.envelope.step, 0);
        assert!(chip.envelope.holding);
        chip.tick(16 * 100);
        assert_eq!(chip.envelope.step, 0);
    }

    #[test]
    fn s5b_envelope_shape_write_resets_phase() {
        // §Shape: writing `$0D` resets the envelope phase to the
        // start of the selected shape. After walking partway
        // through a decay, writing `$0D` again should restart at
        // step 31.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x00);
        chip.tick(16 * 10); // walk down to 21
        assert_eq!(chip.envelope.step, 21);
        s5b_write_reg(&mut chip, 0x0D, 0x00);
        assert_eq!(chip.envelope.step, 31);
        assert_eq!(chip.envelope.timer, 0);
        assert!(!chip.envelope.attacked);
        assert!(!chip.envelope.holding);
    }

    #[test]
    fn s5b_envelope_route_overrides_volume_register() {
        // §Sound: bit 4 of `$08`..=`$0A` routes the envelope DAC
        // instead of the 4-bit volume. With the envelope held at
        // step 31 and only channel A enabled, the channel's
        // amplitude must equal `S5B_ENV_LIN[31]` / 3.
        let mut chip = Sunsoft5b::new();
        // Disable noise on every channel + enable tone on channel A
        // only. R7 layout: bits 0..2 tone-disable, bits 3..5 noise.
        s5b_write_reg(&mut chip, 7, 0b0011_1110);
        s5b_write_reg(&mut chip, 8, 0x10);
        s5b_write_reg(&mut chip, 0x0B, 0x01);
        s5b_write_reg(&mut chip, 0x0C, 0x00);
        s5b_write_reg(&mut chip, 0x0D, 0x0D); // attack + hold
        chip.tick(16 * 32); // walk + edge tick to engage hold
        assert_eq!(chip.envelope.step, 31);
        assert!(chip.envelope.holding);
        chip.channels[0].level = 1;
        let want = S5B_ENV_LIN[31] / 3.0;
        let got = chip.output();
        assert!((got - want).abs() < 1e-6, "want={want:.6}, got={got:.6}",);
    }

    #[test]
    fn s5b_mixer_constant_signal_when_both_disabled() {
        // §Sound: when both tone and noise are disabled on a
        // channel, the channel emits constant DC at its volume.
        let mut chip = Sunsoft5b::new();
        // R7 = 0b00_111_111 — every disable bit set for channel A
        // (bit 0 tone-dis + bit 3 noise-dis).
        s5b_write_reg(&mut chip, 7, 0b0011_1111);
        s5b_write_reg(&mut chip, 8, 0x0F); // channel A volume = 15
                                           // Channels B and C: tone disabled AND noise disabled both
                                           // (constant signal), volume = 0 so they contribute zero.
        s5b_write_reg(&mut chip, 9, 0x00);
        s5b_write_reg(&mut chip, 0x0A, 0x00);
        // Regardless of how we tick the chip, channel A's signal is
        // constant at `LIN_AY_VOL[15]` / 3 (the divide-by-3 happens
        // in `output()`).
        let want = LIN_AY_VOL[15] / 3.0;
        let got = chip.output();
        assert!((got - want).abs() < 1e-6);
        chip.tick(10_000);
        let got2 = chip.output();
        assert!((got2 - want).abs() < 1e-6);
    }

    #[test]
    fn s5b_mixer_noise_only_uses_lfsr_bit() {
        // §Sound: tone-disable=1, noise-disable=0 → channel signal
        // is the noise bit. Force the LFSR to a known value and
        // confirm the channel emits the volume when the LFSR's bit
        // 0 is high.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 7, 0b0000_0001); // chA tone-dis, noise-en
        s5b_write_reg(&mut chip, 8, 0x0F);
        s5b_write_reg(&mut chip, 9, 0x00);
        s5b_write_reg(&mut chip, 0x0A, 0x00);
        // LFSR forced with bit 0 = 1
        chip.noise.lfsr = 0x0000_0001;
        let want = LIN_AY_VOL[15] / 3.0;
        let got = chip.output();
        assert!((got - want).abs() < 1e-6);
        // Bit 0 = 0 → silent.
        chip.noise.lfsr = 0x0001_0000;
        let got = chip.output();
        assert_eq!(got, 0.0);
    }

    #[test]
    fn fds_wave_write_disabled_unless_armed() {
        let mut chip = Fds::new();
        // Without write-enable bit, $4040 writes do nothing.
        chip.write(0x4040, 0x3F);
        assert_eq!(chip.wave[0], 0);
        // Arm wave writes via $4089 bit 7.
        chip.write(0x4089, 0x80);
        chip.write(0x4040, 0x3F);
        assert_eq!(chip.wave[0], 0x3F);
    }

    #[test]
    fn fds_4084_sets_mod_gain_not_position() {
        let mut chip = Fds::new();
        chip.write(0x4084, 0x9F); // bit7 set (env off) + gain = 0x1F
        assert_eq!(chip.mod_gain, 0x1F);
        // $4085 sets the signed 7-bit mod counter.
        chip.write(0x4085, 0x7F); // -1
        assert_eq!(chip.mod_counter, -1);
        chip.write(0x4085, 0x40); // -64
        assert_eq!(chip.mod_counter, -64);
        chip.write(0x4085, 0x3F); // +63
        assert_eq!(chip.mod_counter, 63);
    }

    #[test]
    fn fds_mod_table_writes_only_when_disabled_and_advance_by_entry() {
        let mut chip = Fds::new();
        // Mod unit disabled by default (mod_disabled = true).
        chip.write(0x4088, 0x05);
        chip.write(0x4088, 0x02);
        assert_eq!(chip.mod_table[0], 0x05);
        assert_eq!(chip.mod_table[1], 0x02);
        assert_eq!(chip.mod_pos, 4); // advanced by 2 per write
                                     // Enabling the mod unit blocks further table writes.
        chip.write(0x4087, 0x00); // mod_freq hi = 0, clears disable bit
        assert!(!chip.mod_disabled);
        let before = chip.mod_table[2];
        chip.write(0x4088, 0x07);
        assert_eq!(chip.mod_table[2], before); // unchanged
    }

    #[test]
    fn fds_pitch_formula_matches_spec_c_code() {
        let mut chip = Fds::new();
        chip.freq = 100;
        // counter=0 -> centered 0x40 multiplier -> 100*64 = 6400.
        chip.mod_counter = 0;
        chip.mod_gain = 16;
        assert_eq!(chip.wave_pitch(), 6400);
        // counter=32, gain=16 -> 100*96 = 9600 (no round-up).
        chip.mod_counter = 32;
        assert_eq!(chip.wave_pitch(), 9600);
        // counter=1, gain=1 -> positive round-up branch -> 100*66 = 6600.
        chip.mod_counter = 1;
        chip.mod_gain = 1;
        assert_eq!(chip.wave_pitch(), 6600);
        // negative counter -> no round-up -> 100*56 = 5600.
        chip.mod_counter = -8;
        chip.mod_gain = 16;
        assert_eq!(chip.wave_pitch(), 5600);
    }

    #[test]
    fn fds_mod_table_steps_counter_on_bit11_carry() {
        let mut chip = Fds::new();
        // Fill the whole table with entry 1 (+1 increment).
        for _ in 0..32 {
            chip.write(0x4088, 0x01);
        }
        // mod_pos wraps back to 0 after 32 entry-writes (64 steps).
        assert_eq!(chip.mod_pos, 0);
        // Enable mod unit with the maximum 12-bit freq so every tick
        // carries out of bit 11.
        chip.write(0x4086, 0xFF);
        chip.write(0x4087, 0x0F); // mod_freq = 0xFFF, disable bit clear
        assert!(!chip.mod_disabled);
        chip.mod_counter = 0;
        // Seed the accumulator so the very next add (0x001 + 0xFFF)
        // carries out of bit 11 -> table entry applies -> counter += 1.
        chip.mod_acc = 0x001;
        chip.unit_tick();
        assert_eq!(chip.mod_counter, 1);
        chip.mod_acc = 0x001;
        chip.unit_tick();
        assert_eq!(chip.mod_counter, 2);
    }

    #[test]
    fn fds_mod_counter_wraps_signed_7bit() {
        let mut chip = Fds::new();
        for _ in 0..32 {
            chip.write(0x4088, 0x01); // +1
        }
        chip.write(0x4086, 0xFF);
        chip.write(0x4087, 0x0F);
        chip.mod_counter = 63;
        chip.mod_acc = 0x001;
        chip.unit_tick(); // 63 + 1 -> -64 (signed 7-bit wrap)
        assert_eq!(chip.mod_counter, -64);
    }

    #[test]
    fn fds_mod_table_entry4_resets_counter() {
        let mut chip = Fds::new();
        for _ in 0..32 {
            chip.write(0x4088, 0x04); // reset
        }
        chip.write(0x4086, 0xFF);
        chip.write(0x4087, 0x0F);
        chip.mod_counter = 50;
        chip.mod_acc = 0x001;
        chip.unit_tick();
        assert_eq!(chip.mod_counter, 0);
    }

    #[test]
    fn fds_modulation_changes_wave_advance_rate() {
        // Drive two FDS chips identically except for active modulation;
        // the modulated one must reach a different wave position.
        let mut plain = Fds::new();
        let mut modu = Fds::new();
        for chip in [&mut plain, &mut modu] {
            chip.freq = 0x200;
            chip.mod_gain = 0x20;
        }
        // Modulated chip: a non-zero, oscillating mod table + enabled
        // mod unit with a fast mod frequency.
        for v in [0x01u8, 0x07] {
            for _ in 0..16 {
                modu.write(0x4088, v);
            }
        }
        modu.write(0x4086, 0x80);
        modu.write(0x4087, 0x07); // mod_freq high nibble 7, enabled
        for _ in 0..4000 {
            plain.tick(16);
            modu.tick(16);
        }
        // The plain chip uses the centered 0x40 multiplier; the
        // modulated chip's counter has drifted, changing its pitch and
        // hence its accumulated wave position.
        assert_ne!(plain.wave_acc, modu.wave_acc);
    }

    #[test]
    fn fds_disabling_mod_unit_resets_accumulator() {
        let mut chip = Fds::new();
        chip.write(0x4086, 0x55);
        chip.write(0x4087, 0x02); // freq set, enabled
        chip.mod_acc = 0x0ABC;
        chip.write(0x4087, 0x82); // set disable bit -> reset accumulator
        assert!(chip.mod_disabled);
        assert_eq!(chip.mod_acc, 0);
    }

    #[test]
    fn fds_env_period_matches_spec_formula() {
        // c = 8 * (e + 1) * (m + 1); the $4083 fast bit divides by 4.
        // e = 5, m = 7: 8 * 6 * 8 = 384.
        assert_eq!(Fds::env_period(5, 7, false), 384);
        assert_eq!(Fds::env_period(5, 7, true), 96);
        // e = 0, m = 0xE8 (BIOS default): 8 * 1 * 233 = 1864.
        assert_eq!(Fds::env_period(0, 0xE8, false), 1864);
        // Master speed 0 disables both envelopes.
        assert_eq!(Fds::env_period(10, 0, false), 0);
    }

    #[test]
    fn fds_volume_envelope_decreases_to_zero() {
        let mut chip = Fds::new();
        // Park the wave position at 0 so gain changes commit promptly.
        chip.wave_pos = 0;
        chip.volume = 20;
        chip.master_env_speed = 1; // small master speed
                                   // $4080: M=0 (envelope on), D=0 (decrease), speed=0 -> c = 8*1*2 = 16.
        chip.write(0x4080, 0x00);
        assert!(!chip.vol_env_disabled);
        assert!(!chip.vol_env_increase);
        // One full period must drop the gain by 1.
        chip.tick(16);
        assert_eq!(chip.volume, 19);
        // Many periods clamp at 0.
        for _ in 0..40 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 0);
    }

    #[test]
    fn fds_volume_envelope_increases_and_clamps_at_32() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        chip.volume = 30;
        chip.master_env_speed = 1;
        // $4080: M=0, D=1 (increase), speed=0 -> c = 16.
        chip.write(0x4080, 0x40);
        assert!(chip.vol_env_increase);
        chip.tick(16);
        assert_eq!(chip.volume, 31);
        chip.tick(16);
        assert_eq!(chip.volume, 32);
        // Stays clamped at 32 — increase cannot push past it.
        for _ in 0..10 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 32);
    }

    #[test]
    fn fds_mod_envelope_ramps_mod_gain() {
        let mut chip = Fds::new();
        chip.mod_gain = 10;
        chip.master_env_speed = 1;
        // $4084: M=0 (mod envelope on), D=1 (increase), speed=0 -> c = 16.
        chip.write(0x4084, 0x40);
        assert!(!chip.mod_env_disabled);
        chip.tick(16);
        assert_eq!(chip.mod_gain, 11);
        // Decrease direction now.
        chip.write(0x4084, 0x00); // D=0, speed=0
        chip.tick(16);
        assert_eq!(chip.mod_gain, 10);
    }

    #[test]
    fn fds_master_speed_zero_freezes_envelopes() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        chip.volume = 20;
        chip.master_env_speed = 0; // disables both envelopes
        chip.write(0x4080, 0x00); // would otherwise decrease
        for _ in 0..100 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 20); // unchanged
    }

    #[test]
    fn fds_4083_bit6_halts_envelopes() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        chip.volume = 20;
        chip.master_env_speed = 1;
        chip.write(0x4080, 0x00); // decrease, c = 16
                                  // $4083 bit 6 set: halt both envelopes (and reset their timers).
        chip.write(0x4083, 0x40);
        assert!(chip.env_halt);
        for _ in 0..50 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 20); // frozen while halted
                                     // Clear the halt: the envelope resumes ramping.
        chip.write(0x4083, 0x00);
        chip.tick(16);
        assert_eq!(chip.volume, 19);
    }

    #[test]
    fn fds_4083_bit7_runs_envelopes_4x_faster() {
        let mut slow = Fds::new();
        let mut fast = Fds::new();
        for chip in [&mut slow, &mut fast] {
            chip.wave_pos = 0;
            chip.volume = 30;
            chip.master_env_speed = 1;
        }
        // speed e=1, m=1 -> c = 8*2*2 = 32 (slow); /4 = 8 (fast).
        slow.write(0x4080, 0x01); // M=0, D=0, e=1
        fast.write(0x4083, 0x80); // set fast bit first
        fast.write(0x4080, 0x01);
        // 32 cycles: slow ramps once, fast ramps four times.
        slow.tick(32);
        fast.tick(32);
        assert_eq!(slow.volume, 29);
        assert_eq!(fast.volume, 26);
    }

    #[test]
    fn fds_4083_bit7_halts_mod_accumulator() {
        let mut chip = Fds::new();
        // Enable the mod unit with a non-zero frequency.
        chip.write(0x4086, 0x55);
        chip.write(0x4087, 0x02); // mod_freq set, unit enabled
        let before = chip.mod_acc;
        // Set the $4083 fast bit (bit 7): the mod accumulator must freeze.
        chip.write(0x4083, 0x80);
        for _ in 0..100 {
            chip.unit_tick();
        }
        assert_eq!(chip.mod_acc, before);
        // Clearing the fast bit lets the accumulator advance again.
        chip.write(0x4083, 0x00);
        chip.unit_tick();
        assert_ne!(chip.mod_acc, before);
    }

    #[test]
    fn fds_direct_volume_write_with_mode_bit() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        // $4080 with M=1 (env off) sets the gain directly.
        chip.write(0x4080, 0x80 | 0x14); // gain = 0x14 = 20
        assert!(chip.vol_env_disabled);
        assert_eq!(chip.volume, 20);
        // Writing gain 0 mutes immediately even off wave-position 0.
        chip.wave_pos = 5;
        chip.write(0x4080, 0x80); // M=1, gain = 0
        assert_eq!(chip.volume, 0);
        assert!(chip.vol_pending.is_none());
    }

    #[test]
    fn fds_volume_change_latched_until_wave_pos_zero() {
        let mut chip = Fds::new();
        chip.volume = 10;
        chip.master_env_speed = 1;
        // Stage a decrease while the wave position is non-zero.
        chip.wave_pos = 7;
        chip.write(0x4080, 0x00); // decrease, c = 16
        chip.tick(16); // ramp steps, but a non-zero wave position holds it
        assert_eq!(chip.volume, 10);
        assert_eq!(chip.vol_pending, Some(9));
        // Freeze the envelope (master speed 0) so no further ramp steps
        // fire, then advance the wave unit until its position returns to
        // 0, at which point the staged gain commits through the PWM latch.
        chip.master_env_speed = 0;
        chip.freq = 0x100; // advance the wave accumulator
        for _ in 0..10_000 {
            chip.tick(16);
            if chip.wave_pos == 0 {
                break;
            }
        }
        assert_eq!(chip.wave_pos, 0);
        assert_eq!(chip.volume, 9);
        assert!(chip.vol_pending.is_none());
    }

    #[test]
    fn fds_mode_bit_blocks_volume_ramp() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        chip.master_env_speed = 1;
        // M=1 (env off): the ramp must never run.
        chip.write(0x4080, 0x80 | 0x14); // gain 20, env disabled
        for _ in 0..100 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 20);
    }

    #[test]
    fn fds_4023_disable_defaults_to_enabled() {
        // A fresh chip behaves as if the BIOS already wrote $4023 = $83.
        let chip = Fds::new();
        assert!(chip.sound_enabled);
    }

    #[test]
    fn fds_4023_sound_disable_halts_wave_and_freezes_position() {
        let mut chip = Fds::new();
        chip.freq = 0x200; // non-zero so the wave unit would advance
                           // Advance a bit so the position is non-zero, then disable sound.
        for _ in 0..32 {
            chip.tick(16);
        }
        assert_ne!(chip.wave_acc, 0);
        // $4023 bit 1 clear: halt. Position resets to $4040 (wave_pos 0).
        chip.write(0x4023, 0x00);
        assert!(!chip.sound_enabled);
        assert_eq!(chip.wave_pos, 0);
        assert_eq!(chip.wave_acc, 0);
        // Ticking while halted does not advance the wave accumulator.
        for _ in 0..1000 {
            chip.tick(16);
        }
        assert_eq!(chip.wave_acc, 0);
        assert_eq!(chip.wave_pos, 0);
        // Re-enabling ($83 -> bit 1 set) lets the wave unit run again.
        chip.write(0x4023, 0x83);
        assert!(chip.sound_enabled);
        chip.tick(16);
        assert_ne!(chip.wave_acc, 0);
    }

    #[test]
    fn fds_4023_sound_disable_halts_mod_accumulator() {
        let mut chip = Fds::new();
        chip.write(0x4086, 0x55);
        chip.write(0x4087, 0x02); // mod unit enabled, non-zero freq
        chip.tick(16);
        let before = chip.mod_acc;
        chip.write(0x4023, 0x00); // halt
        for _ in 0..200 {
            chip.tick(16);
        }
        // The mod accumulator does not advance while the waveform is halted.
        assert_eq!(chip.mod_acc, before);
    }

    #[test]
    fn fds_4023_sound_disable_freezes_envelopes() {
        let mut chip = Fds::new();
        chip.wave_pos = 0;
        chip.volume = 20;
        chip.master_env_speed = 1;
        chip.write(0x4080, 0x00); // decrease envelope, c = 16
                                  // Disable sound: the envelopes must not tick.
        chip.write(0x4023, 0x00);
        for _ in 0..50 {
            chip.tick(16);
        }
        assert_eq!(chip.volume, 20); // frozen while halted
                                     // Re-enable: the envelope resumes ramping.
        chip.write(0x4023, 0x82);
        chip.tick(16);
        assert_eq!(chip.volume, 19);
    }

    #[test]
    fn fds_4023_volume_writes_still_affect_held_output() {
        // While halted the channel holds the constant $4040 value, but
        // $4080 / $4089 writes still change the output level per spec.
        let mut chip = Fds::new();
        chip.wave[0] = 63; // $4040 sample
        chip.wave_pos = 0;
        chip.write(0x4089, 0x00); // master volume full, write-protect RAM
        chip.write(0x4023, 0x00); // halt
                                  // Direct volume write (M=1) sets the gain immediately.
        chip.write(0x4080, 0x80 | 0x20); // gain 32
        let loud = chip.output();
        chip.write(0x4080, 0x80 | 0x08); // gain 8
        let quiet = chip.output();
        assert!(loud.abs() > quiet.abs());
        // Master-volume divider also affects the held output.
        chip.write(0x4080, 0x80 | 0x20);
        let full = chip.output();
        chip.write(0x4089, 0x03); // master volume 2/5
        let attenuated = chip.output();
        assert!(full.abs() > attenuated.abs());
    }

    #[test]
    fn expansion_disabled_chips_emit_silence() {
        let mut x = Expansion::new();
        x.set_flags(ExpansionChips(0));
        assert!((x.output()).abs() < 1e-9);
    }

    #[test]
    fn expansion_routes_writes_only_to_enabled_chip() {
        let mut x = Expansion::new();
        x.set_flags(ExpansionChips(0x01)); // VRC6 only
        x.write(0x9000, 0x0F);
        // VRC6 should have picked up the volume; FDS shouldn't.
        assert_eq!(x.vrc6.pulse[0].volume, 15);
        assert_eq!(x.fds.volume, 0);
    }

    #[test]
    fn n163_wave_ram_writes_through_address_pointer() {
        let mut chip = N163::new();
        chip.write(0xF800, 0x80 | 0x10); // addr=0x10, auto-inc
        chip.write(0x4800, 0xAB);
        assert_eq!(chip.ram[0x10], 0xAB);
        assert_eq!(chip.addr, 0x11);
    }

    // ------- FDS read registers $4090..=$4097 (round 10) -------

    #[test]
    fn fds_4090_reads_volume_gain_with_open_bus_top_bits() {
        let mut chip = Fds::new();
        // Direct gain via $4080 M=1: volume = 0x14 = 20.
        chip.write(0x4080, 0x80 | 0x14);
        // Top 2 bits return "01"; bottom 6 bits are the volume gain.
        assert_eq!(chip.read(0x4090), 0x40 | 0x14);
        // Maximum volume gain (0x3F) still keeps the top bits at 01.
        chip.write(0x4080, 0x80 | 0x3F);
        assert_eq!(chip.read(0x4090), 0x7F);
    }

    #[test]
    fn fds_4091_reads_wave_accumulator_bits_12_to_19() {
        let mut chip = Fds::new();
        // Pick a value whose bits 12-19 are easy to read: 0xABCDE_F.
        chip.wave_acc = 0xABCDEF;
        // Bits 12-19 = 0xBC.
        assert_eq!(chip.read(0x4091), 0xBC);
        // Fresh chip: zero accumulator → zero readout.
        let fresh = Fds::new();
        assert_eq!(fresh.read(0x4091), 0x00);
    }

    #[test]
    fn fds_4092_reads_mod_gain_with_open_bus_top_bits() {
        let mut chip = Fds::new();
        // Direct mod gain via $4084 M=1: gain = 0x1F.
        chip.write(0x4084, 0x80 | 0x1F);
        assert_eq!(chip.read(0x4092), 0x40 | 0x1F);
        // Mod gain field is only 6 bits — top bits of $4092 always "01".
        chip.mod_gain = 0x3F;
        assert_eq!(chip.read(0x4092), 0x7F);
    }

    #[test]
    fn fds_4093_reads_mod_acc_bits_5_to_11_top_bit_zero() {
        let mut chip = Fds::new();
        // Set mod_acc to 0xABC (12-bit max field = 0xFFF).
        chip.mod_acc = 0xABC;
        // Bits 5-11 of 0xABC = (0xABC >> 5) & 0x7F = 0x55.
        assert_eq!(chip.read(0x4093), 0x55);
        // Top bit must always be 0 (open bus).
        chip.mod_acc = 0xFFF;
        assert_eq!(chip.read(0x4093) & 0x80, 0x00);
        assert_eq!(chip.read(0x4093), 0x7F);
    }

    #[test]
    fn fds_4094_reads_counter_times_gain_bits_4_to_11() {
        let mut chip = Fds::new();
        // counter = 16, gain = 16 → product = 256 = 0x100. (0x100 >> 4) = 0x10.
        chip.mod_counter = 16;
        chip.mod_gain = 16;
        assert_eq!(chip.read(0x4094), 0x10);
        // Negative counter: -8 * 16 = -128 = 0xFFFF_FF80 (i32). >> 4 = 0xFFFF_FFF8.
        // Mask to 0xFF → 0xF8.
        chip.mod_counter = -8;
        chip.mod_gain = 16;
        assert_eq!(chip.read(0x4094), 0xF8);
    }

    #[test]
    fn fds_4095_reads_next_mod_increment_in_display_form() {
        let mut chip = Fds::new();
        // Fill the table with entry 7 (-1 increment), then check the readout.
        for _ in 0..32 {
            chip.write(0x4088, 0x07);
        }
        // mod_pos wraps to 0 after 32 writes → reads back position-0 entry.
        assert_eq!(chip.mod_pos, 0);
        // Entry 7 displays as 0xF.
        assert_eq!(chip.read(0x4095) & 0x0F, 0x0F);
        // Top nibble is "Unknown counter" → return 0.
        assert_eq!(chip.read(0x4095) & 0xF0, 0x00);
        // Entry 3 displays as 0x4 (per the 0,1,2,3,4,5,6,7→0,1,2,4,C,C,E,F table).
        chip.mod_table[0] = 3;
        assert_eq!(chip.read(0x4095), 0x04);
        // Entry 4 (reset) displays as 0xC.
        chip.mod_table[0] = 4;
        assert_eq!(chip.read(0x4095), 0x0C);
        // Entry 6 displays as 0xE.
        chip.mod_table[0] = 6;
        assert_eq!(chip.read(0x4095), 0x0E);
    }

    #[test]
    fn fds_4096_reads_current_wavetable_sample() {
        let mut chip = Fds::new();
        chip.write(0x4089, 0x80); // arm wave-RAM write
                                  // Fill a recognisable pattern at position 0 + position 7.
        chip.write(0x4040, 0x2A);
        chip.write(0x4047, 0x15);
        chip.wave_pos = 0;
        assert_eq!(chip.read(0x4096), 0x40 | 0x2A);
        chip.wave_pos = 7;
        assert_eq!(chip.read(0x4096), 0x40 | 0x15);
        // Top bits always "01" open bus.
        chip.write(0x4040, 0x3F);
        chip.wave_pos = 0;
        assert_eq!(chip.read(0x4096), 0x7F);
    }

    #[test]
    fn fds_4097_reads_mod_counter_signed_7bit_top_bit_zero() {
        let mut chip = Fds::new();
        // Positive counter: 0x3F = 63.
        chip.mod_counter = 63;
        assert_eq!(chip.read(0x4097), 0x3F);
        // Negative counter: -1 = 0xFF as u8 → masked to 0x7F.
        chip.mod_counter = -1;
        assert_eq!(chip.read(0x4097), 0x7F);
        // -64 = 0xC0 as u8 → masked to 0x40.
        chip.mod_counter = -64;
        assert_eq!(chip.read(0x4097), 0x40);
        // Zero counter.
        chip.mod_counter = 0;
        assert_eq!(chip.read(0x4097), 0x00);
        // Top bit always 0 across the whole range.
        for c in -64..=63i8 {
            chip.mod_counter = c;
            assert_eq!(chip.read(0x4097) & 0x80, 0x00, "top bit set for c={c}");
        }
    }

    #[test]
    fn fds_unmapped_read_returns_open_bus() {
        let chip = Fds::new();
        // The status-read window is $4090..=$4097; addresses outside
        // it (including the write-only register file at $4080..$408A
        // and the wave RAM at $4040..$407F) must return the open-bus
        // sentinel so the upstream router can fall through.
        assert_eq!(chip.read(0x4080), 0xFF);
        assert_eq!(chip.read(0x408A), 0xFF);
        assert_eq!(chip.read(0x4040), 0xFF);
        assert_eq!(chip.read(0x4098), 0xFF);
        assert_eq!(chip.read(0x4099), 0xFF);
    }

    #[test]
    fn expansion_routes_fds_reads_only_when_enabled() {
        let mut x = Expansion::new();
        x.set_flags(ExpansionChips(0)); // no chips enabled
        assert_eq!(x.read(0x4090), 0xFF); // open bus when FDS off
                                          // Enable FDS and prime a volume gain readback.
        x.set_flags(ExpansionChips(0x04)); // FDS flag bit
        x.fds.volume = 0x12;
        assert_eq!(x.read(0x4090), 0x40 | 0x12);
        // A non-FDS address still falls through to the open-bus default.
        assert_eq!(x.read(0x4080), 0xFF);
    }

    // ------- N163 per-channel timer accumulator (round 11) -------

    /// Write the channel registers for channel 8 (base $78) directly
    /// into sound RAM via the auto-increment pointer path.
    fn n163_write_channel8(
        chip: &mut N163,
        freq: u32,
        phase: u32,
        wave_len_l_field: u8,
        wave_addr: u8,
        volume: u8,
        c_field: u8,
    ) {
        // Set address to $78 with auto-increment, then walk $78..$7F.
        chip.write(0xF800, 0x80 | 0x78);
        chip.write(0x4800, (freq & 0xFF) as u8); // $78 low freq
        chip.write(0x4800, (phase & 0xFF) as u8); // $79 low phase
        chip.write(0x4800, ((freq >> 8) & 0xFF) as u8); // $7A mid freq
        chip.write(0x4800, ((phase >> 8) & 0xFF) as u8); // $7B mid phase
        chip.write(
            0x4800,
            ((wave_len_l_field & 0x3F) << 2) | (((freq >> 16) & 0x03) as u8),
        ); // $7C
        chip.write(0x4800, ((phase >> 16) & 0xFF) as u8); // $7D high phase
        chip.write(0x4800, wave_addr); // $7E
        chip.write(0x4800, ((c_field & 0x07) << 4) | (volume & 0x0F)); // $7F
    }

    #[test]
    fn n163_writing_7f_decodes_channels_active() {
        // The `1+C` field at $7F bits 6-4 selects the number of enabled
        // channels. Wiki §"Sound RAM $7F - Volume": C=0 → 1 channel
        // (ch8 only); C=7 → 8 channels.
        let mut chip = N163::new();
        // C=0 → 1 enabled channel.
        chip.write(0xF800, 0x7F);
        chip.write(0x4800, 0x00);
        assert_eq!(chip.channels_active, 1);
        // C=7 → 8 enabled channels.
        chip.write(0xF800, 0x7F);
        chip.write(0x4800, 0x70 | 0x05);
        assert_eq!(chip.channels_active, 8);
        // The low-nibble volume bits should NOT bleed into the channel
        // count.
        chip.write(0xF800, 0x7F);
        chip.write(0x4800, 0x20 | 0x0F);
        assert_eq!(chip.channels_active, 3);
    }

    #[test]
    fn n163_active_channel_set_is_top_down() {
        // With N enabled channels, only channels (9-N)..=8 are clocked.
        // Wiki §"Sound RAM $7F - Volume": "When C=0, only channel 8
        // enabled; C=1 → channels 7+8; ... C=7 → channels 1..=8".
        let mut chip = N163::new();
        chip.channels_active = 1;
        assert_eq!(chip.active_channel(0), 8);
        chip.channels_active = 2;
        assert_eq!(chip.active_channel(0), 7);
        assert_eq!(chip.active_channel(1), 8);
        chip.channels_active = 8;
        for i in 0..8 {
            assert_eq!(chip.active_channel(i), i + 1);
        }
    }

    #[test]
    fn n163_address_pointer_stops_at_0x7f_no_wrap() {
        // Wiki §"Address Port": "it does not wrap, instead stopping
        // at $7F." (Footnote-cited correction to a previous version.)
        let mut chip = N163::new();
        chip.write(0xF800, 0x80 | 0x7E); // addr=$7E, auto-inc
        chip.write(0x4800, 0x11);
        assert_eq!(chip.addr, 0x7F);
        chip.write(0x4800, 0x22);
        assert_eq!(chip.addr, 0x7F, "address must stop at $7F, not wrap to $00");
        // The second write should still land at the held address.
        assert_eq!(chip.ram[0x7F], 0x22);
    }

    #[test]
    fn n163_tick_advances_phase_by_freq_each_15_cycles() {
        // Wiki §"Channel Update": every 15 CPU cycles, the chip adds
        // freq to phase, mod (wave_len << 16).
        let mut chip = N163::new();
        chip.enabled = true;
        // Channel 8: freq=0x100, phase=0, wave_len=4 samples → L=63.
        // wave_len = 256 - (L<<2) = 256 - 252 = 4. So L field = 63.
        n163_write_channel8(&mut chip, 0x100, 0, 63, 0, 0x0F, 0);
        // First channel update happens at cycle 15.
        chip.tick(14);
        // Phase should still be 0 — no full 15-cycle window yet.
        let phase_low = chip.ram[0x40 + 7 * 8 + 1]; // ch8 phase low @ $79
        assert_eq!(phase_low, 0);
        chip.tick(1); // total 15 — one tick triggers
                      // After one update: phase = (0 + 0x100) mod (4<<16) = 0x100.
        let p_lo = chip.ram[0x79];
        let p_mid = chip.ram[0x7B];
        let p_hi = chip.ram[0x7D];
        let phase = p_lo as u32 | ((p_mid as u32) << 8) | ((p_hi as u32) << 16);
        assert_eq!(phase, 0x100);
    }

    #[test]
    fn n163_phase_wraps_modulo_wave_length_shifted() {
        // phase = (phase + freq) % (wave_len << 16). At wave_len = 4,
        // modulus = 0x40000. Pick freq = 0x20000, phase = 0x30000 →
        // (0x30000 + 0x20000) % 0x40000 = 0x10000.
        let mut chip = N163::new();
        chip.enabled = true;
        n163_write_channel8(&mut chip, 0x20000, 0x30000, 63, 0, 0x0F, 0);
        chip.tick(15);
        let p_lo = chip.ram[0x79];
        let p_mid = chip.ram[0x7B];
        let p_hi = chip.ram[0x7D];
        let phase = p_lo as u32 | ((p_mid as u32) << 8) | ((p_hi as u32) << 16);
        assert_eq!(phase, 0x10000, "phase should wrap mod 0x40000");
    }

    #[test]
    fn n163_decodes_sample_at_high_phase_plus_wave_addr() {
        // Wiki §"Channel Update" speculative version:
        //   output = (sample(((phase >> 16) + wave_addr) & 0xFF) - 8)
        //            * volume
        // Lay down a recognisable wave at RAM offset 0: byte $00 = 0xA9.
        // Two nibbles → low=9, high=A. Volume = 1 to keep math simple.
        let mut chip = N163::new();
        chip.enabled = true;
        // Channel 8, wave_addr=0 (samples start at nibble index 0).
        // Wave length 4 samples (L=63), freq=0x10000 (advances 1 sample
        // per tick), phase initial = 0.
        n163_write_channel8(&mut chip, 0x10000, 0, 63, 0, 0x01, 0);
        chip.ram[0x00] = 0xA9; // low nibble = 9, high nibble = A
        chip.ram[0x01] = 0xC8; // low nibble = 8, high nibble = C
                               // First tick brings phase to 0x10000 → high byte = 1 → sample
                               // index 1 → high nibble of $00 = 0xA = 10 → (10 - 8) * 1 = +2.
        chip.tick(15);
        let out_after_first = chip.output();
        // Expected: signed=2, volume=1, normalised by /128 = 2/128.
        assert!(
            (out_after_first - 2.0 / 128.0).abs() < 1e-6,
            "got {out_after_first}"
        );
        // Next tick: phase = 0x20000 → high byte = 2 → sample index
        // 2 → low nibble of $01 = 8 → (8 - 8) = 0 → silence at this
        // sample.
        chip.tick(15);
        assert!(chip.output().abs() < 1e-9);
    }

    #[test]
    fn n163_round_robin_advances_through_active_channels() {
        // With N channels enabled, the chip cycles through them in
        // order. With 2 active channels (7 + 8), two consecutive ticks
        // should update channel 7 first (slot 0) then channel 8.
        let mut chip = N163::new();
        chip.enabled = true;
        // Pre-load channel 7 (base $70) and channel 8 (base $78) with
        // distinct freqs so we can tell which one updated.
        // Channel 7: freq=0x111, phase=0, wave_len=4 → L=63.
        chip.write(0xF800, 0x80 | 0x70); // auto-inc from $70
        chip.write(0x4800, 0x11); // $70 low freq
        chip.write(0x4800, 0x00); // $71 low phase
        chip.write(0x4800, 0x01); // $72 mid freq
        chip.write(0x4800, 0x00); // $73 mid phase
        chip.write(0x4800, 63 << 2); // $74 wave len, freq high=0
        chip.write(0x4800, 0x00); // $75 high phase
        chip.write(0x4800, 0x00); // $76 wave address
        chip.write(0x4800, 0x00); // $77 volume (channel-7 volume)
                                  // Channel 8: freq=0x222, two enabled channels (C=1).
        n163_write_channel8(&mut chip, 0x222, 0, 63, 0, 0x0F, 1);
        assert_eq!(chip.channels_active, 2);
        // First tick: slot 0 = channel 7 should advance.
        chip.tick(15);
        let ch7_phase_lo = chip.ram[0x71];
        let ch8_phase_lo = chip.ram[0x79];
        assert_eq!(ch7_phase_lo, 0x11, "ch7 should have ticked first");
        assert_eq!(ch8_phase_lo, 0x00, "ch8 should not have ticked yet");
        // Second tick: slot 1 = channel 8.
        chip.tick(15);
        let ch7_phase_lo = chip.ram[0x71];
        let ch8_phase_lo = chip.ram[0x79];
        assert_eq!(ch7_phase_lo, 0x11, "ch7 should still hold its phase");
        assert_eq!(ch8_phase_lo, 0x22, "ch8 should have ticked second");
        // Third tick: wraps back to slot 0 = channel 7.
        chip.tick(15);
        assert_eq!(chip.ram[0x71], 0x22, "ch7 should have ticked twice");
    }

    #[test]
    fn n163_output_holds_until_next_tick() {
        // Per §"Channel Update": "The output will be held until the
        // next channel update." A `tick(0)` should leave the output
        // unchanged.
        let mut chip = N163::new();
        chip.enabled = true;
        n163_write_channel8(&mut chip, 0x10000, 0, 63, 0, 0x01, 0);
        chip.ram[0x00] = 0xA9;
        chip.tick(15);
        let held = chip.output();
        chip.tick(0); // no cycles → no new update
        assert!((chip.output() - held).abs() < 1e-9);
        // Another sub-15-cycle batch should also be a no-op.
        chip.tick(14);
        assert!((chip.output() - held).abs() < 1e-9);
    }

    #[test]
    fn n163_silent_when_disabled() {
        // The chip should not tick or emit output when the expansion
        // flag is clear.
        let mut chip = N163::new();
        chip.enabled = false;
        n163_write_channel8(&mut chip, 0x10000, 0, 63, 0, 0x0F, 0);
        chip.ram[0x00] = 0xFF;
        chip.tick(1500); // would be 100 channel updates if enabled
        assert!(chip.output().abs() < 1e-9);
        // Phase shouldn't have moved either.
        assert_eq!(chip.ram[0x79], 0x00);
    }

    #[test]
    fn n163_cycle_accumulator_carries_leftover_cycles() {
        // Cycles smaller than 15 should accumulate across calls.
        let mut chip = N163::new();
        chip.enabled = true;
        n163_write_channel8(&mut chip, 0x100, 0, 63, 0, 0x0F, 0);
        chip.tick(7);
        chip.tick(7);
        // 14 total — no update yet.
        assert_eq!(chip.ram[0x79], 0);
        chip.tick(1); // pushes us to 15
        assert_eq!(chip.ram[0x79], 0x00);
        // The high phase byte stays 0 (freq = 0x100 < 0x10000), but
        // the low byte ticks to 0x00 and mid byte to 0x01.
        assert_eq!(chip.ram[0x7B], 0x01);
    }

    // ------------------------------------------------------------- VRC7

    /// Helper: write `value` into VRC7 internal register `r` via the
    /// `$9010` select / `$9030` data ports.
    fn vrc7_write_reg(chip: &mut Vrc7, r: u8, value: u8) {
        chip.write(0x9010, r);
        chip.write(0x9030, value);
    }

    #[test]
    fn vrc7_instrument_rom_table_size_is_sixteen() {
        // Per §"Internal patch set": "There are 16 different
        // instrument patches available on the VRC7." Slot 0 is the
        // user-programmable placeholder; slots 1..=15 are hardwired.
        assert_eq!(VRC7_INSTRUMENT_ROM.len(), 16);
    }

    #[test]
    fn vrc7_buzzy_bell_patch_decodes_from_rom() {
        // Patch 1 "Buzzy Bell" = `03 21 05 06 E8 81 42 27` per the
        // dumped table in vrc7-audio-wiki.html.
        let p = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[1]);
        // $00 = 0x03: T=0 V=0 S=0 K=0 M=3 — pure modulator, no LFOs,
        // multiplier 3.
        assert!(!p.mod_tremolo);
        assert!(!p.mod_vibrato);
        assert!(!p.mod_sustain);
        assert!(!p.mod_ksr);
        assert_eq!(p.mod_mult, 3);
        // $01 = 0x21: T=0 V=0 S=1 K=0 M=1.
        assert!(p.car_sustain);
        assert_eq!(p.car_mult, 1);
        // $02 = 0x05: KSL=0, TL=5 (modulator output level).
        assert_eq!(p.mod_ksl, 0);
        assert_eq!(p.mod_tl, 5);
        // $03 = 0x06: KSL=0, Q=0, W=0, FB=6.
        assert_eq!(p.car_ksl, 0);
        assert_eq!(p.car_wave, 0);
        assert_eq!(p.mod_wave, 0);
        assert_eq!(p.feedback, 6);
        // $04 = 0xE8: mod attack 0xE, decay 0x8.
        assert_eq!(p.mod_attack, 0xE);
        assert_eq!(p.mod_decay, 0x8);
        // $05 = 0x81: car attack 0x8, decay 0x1.
        assert_eq!(p.car_attack, 0x8);
        assert_eq!(p.car_decay, 0x1);
        // $06 = 0x42: mod sustain 0x4, release 0x2.
        assert_eq!(p.mod_sustain_level, 0x4);
        assert_eq!(p.mod_release, 0x2);
        // $07 = 0x27: car sustain 0x2, release 0x7.
        assert_eq!(p.car_sustain_level, 0x2);
        assert_eq!(p.car_release, 0x7);
    }

    #[test]
    fn vrc7_vibes_patch_has_high_bit_tremolo() {
        // Patch A "Vibes" = `B5 01 0F 0F A8 A5 51 02`. The wiki
        // spells the §"Custom Patch" $00 bitfield TVSKMMMM, so
        // 0xB5 = 1011_0101: T=1 V=0 S=1 K=1 M=5.
        let p = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[0xA]);
        assert!(p.mod_tremolo);
        assert!(!p.mod_vibrato);
        assert!(p.mod_sustain);
        assert!(p.mod_ksr);
        assert_eq!(p.mod_mult, 5);
        // $03 = 0x0F = 0000_1111: KK=0, unused=0, Q=0, W=1, FB=7.
        // The §"Custom Patch" $03 bitfield is KK-QWFFF (bit 7-6 KSL,
        // bit 5 unused, bit 4 carrier waveform, bit 3 modulator
        // waveform, bits 2-0 feedback).
        assert_eq!(p.car_ksl, 0);
        assert_eq!(p.car_wave, 0);
        assert_eq!(p.mod_wave, 1);
        assert_eq!(p.feedback, 7);
    }

    #[test]
    fn vrc7_patch_zero_reads_user_programmable_regs() {
        // Slot 0 is the "custom patch" placeholder. The decoder must
        // pull from regs[0x00..=0x07] rather than VRC7_INSTRUMENT_ROM.
        let mut chip = Vrc7::new();
        // Pre-program: TVSKMMMM = 1101_1010 → tremolo on, vibrato
        // on, sustain off, KSR on, mult=0xA.
        vrc7_write_reg(&mut chip, 0x00, 0xDA);
        // Carrier bytes are 0 — explicit so the decode is unambiguous.
        for r in 0x01..=0x07 {
            vrc7_write_reg(&mut chip, r, 0x00);
        }
        let p = chip.patch(0);
        assert!(p.mod_tremolo);
        assert!(p.mod_vibrato);
        assert!(!p.mod_sustain);
        assert!(p.mod_ksr);
        assert_eq!(p.mod_mult, 0xA);
        // Carrier bytes were all zero.
        assert!(!p.car_tremolo);
        assert_eq!(p.car_mult, 0);
    }

    #[test]
    fn vrc7_channel_register_3x_decodes_instrument_and_volume() {
        // $3X = IIIIVVVV: high nibble = instrument index, low nibble =
        // inverted volume. Writing 0x73 to channel 2's $32 means
        // instrument = 7 (Trumpet), volume = 3.
        let mut chip = Vrc7::new();
        vrc7_write_reg(&mut chip, 0x32, 0x73);
        assert_eq!(chip.channels[2].patch_index, 7);
        assert_eq!(chip.channels[2].volume, 3);
        // active_patch routes through patch(7) and matches the ROM
        // entry exactly.
        let active = chip.active_patch(2);
        let trumpet = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[7]);
        assert_eq!(active, trumpet);
    }

    #[test]
    fn vrc7_channel_register_2x_decodes_sustain_and_key_on() {
        // $2X = --STOOOH. Writing 0x30 to $20 means S=1 T=1 octave=0
        // fnum-high=0. The channel should report both sustain and
        // key-on.
        let mut chip = Vrc7::new();
        vrc7_write_reg(&mut chip, 0x20, 0x30);
        assert!(chip.channels[0].sustain);
        assert!(chip.channels[0].key_on);
        // 0x10 = key-on without sustain.
        vrc7_write_reg(&mut chip, 0x21, 0x10);
        assert!(!chip.channels[1].sustain);
        assert!(chip.channels[1].key_on);
        // 0x20 = sustain without trigger — a release-in-progress with
        // patch-release override per §Channels.
        vrc7_write_reg(&mut chip, 0x22, 0x20);
        assert!(chip.channels[2].sustain);
        assert!(!chip.channels[2].key_on);
    }

    #[test]
    fn vrc7_channel_patch_defaults_to_custom_slot_zero() {
        // Fresh chip: no $3X writes yet, so every channel sees
        // patch_index=0 (custom) and its custom patch bytes are
        // still all zero.
        let chip = Vrc7::new();
        for ch in 0..6 {
            assert_eq!(chip.channels[ch].patch_index, 0);
        }
        let p = chip.active_patch(0);
        assert_eq!(p, Vrc7Patch::default());
    }

    #[test]
    fn vrc7_patch_index_out_of_range_wraps_mod_sixteen() {
        // The $3X high nibble is only 4 bits, so this is defensive,
        // but patch(20) should resolve to patch(4) without panicking.
        let chip = Vrc7::new();
        let p = chip.patch(20);
        let expected = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[4]);
        assert_eq!(p, expected);
    }
}
