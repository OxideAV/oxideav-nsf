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
/// a noise generator, and an envelope generator. Round 2 implements
/// the three squares + amplitude envelope (no shape generator yet —
/// it averages to the 16-step decay shape).
#[derive(Default)]
pub struct Sunsoft5b {
    pub enabled: bool,
    pub addr: u8,
    pub regs: [u8; 16],
    pub channels: [S5bChan; 3],
}

#[derive(Default, Clone, Copy)]
pub struct S5bChan {
    pub timer_period: u16,
    pub timer: u16,
    pub level: u8,
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
                // Update channel periods + volumes.
                for ch in 0..3 {
                    let lo = self.regs[ch * 2] as u16;
                    let hi = (self.regs[ch * 2 + 1] & 0x0F) as u16;
                    self.channels[ch].timer_period = (hi << 8) | lo;
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        // Sunsoft 5B clocks at the CPU clock / 16.
        for ch in &mut self.channels {
            if ch.timer_period == 0 {
                continue;
            }
            let div = ch.timer_period.max(1);
            let mut left = cycles / 16;
            while left > 0 {
                if ch.timer == 0 {
                    ch.timer = div;
                    ch.level ^= 1;
                } else {
                    ch.timer -= 1;
                }
                left -= 1;
            }
        }
    }

    pub fn output(&self) -> f32 {
        let mut sum = 0.0f32;
        // Mixer enable byte at register 7 (R7): low 3 bits = tone enable
        // (active-low). High bits select noise — ignored.
        let r7 = self.regs[7];
        for (i, ch) in self.channels.iter().enumerate() {
            let tone_on = (r7 >> i) & 1 == 0;
            if !tone_on {
                continue;
            }
            let vol = self.regs[8 + i] & 0x0F;
            let vol_lin = LIN_AY_VOL[vol as usize];
            sum += if ch.level != 0 { vol_lin } else { 0.0 };
        }
        sum / 3.0
    }
}

// AY-style logarithmic volume table → linear amplitude (16 steps).
const LIN_AY_VOL: [f32; 16] = [
    0.0, 0.011, 0.022, 0.033, 0.046, 0.066, 0.094, 0.133, 0.188, 0.265, 0.375, 0.529, 0.747, 1.057,
    1.494, 2.114,
];

// ---------------------------------------------------------------- N163

/// Namco 163 wavetable synthesis. Up to 8 channels share a 128-byte
/// wave RAM; each channel reads 4-bit samples through its own pointer.
pub struct N163 {
    pub enabled: bool,
    pub addr: u8,
    pub addr_inc: bool,
    pub ram: [u8; 0x80],
    pub channels_active: u8, // 1..=8
}

impl Default for N163 {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: 0,
            addr_inc: false,
            ram: [0u8; 0x80],
            channels_active: 1,
        }
    }
}

impl N163 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xF800 => {
                self.addr = value & 0x7F;
                self.addr_inc = value & 0x80 != 0;
            }
            0x4800 => {
                self.ram[self.addr as usize] = value;
                if self.addr_inc {
                    self.addr = (self.addr + 1) & 0x7F;
                }
                if (0x40..0x80).contains(&(self.addr as u32 as usize)) {
                    // Byte at $7F = control: low 3 bits = (chan_active - 1).
                    self.channels_active = (self.ram[0x7F] & 0x07) + 1;
                }
            }
            _ => {}
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0x4800 => self.ram[self.addr as usize],
            _ => 0xFF,
        }
    }

    pub fn output(&self) -> f32 {
        // Round 2: the channel iteration is a thin approximation. We
        // sample every active channel's wavetable at its current
        // frequency-accumulator phase (stored at $40+8*ch). Volume is
        // at $47+8*ch. This is enough to hear N163 rips chime in but
        // does not implement per-channel timer accumulation.
        if self.channels_active == 0 {
            return 0.0;
        }
        let mut sum = 0.0f32;
        for ch in 0..self.channels_active as usize {
            let base = 0x40 + ch * 8;
            let vol = self.ram[base + 7] & 0x0F;
            let phase = self.ram[base];
            let wave_off = self.ram[base + 6] as usize & 0xFF;
            let nibble_index = (phase as usize).wrapping_add(wave_off) & 0xFF;
            let byte = self.ram[(nibble_index >> 1) & 0x7F];
            let nib = if nibble_index & 1 == 0 {
                byte & 0x0F
            } else {
                byte >> 4
            };
            let signed = (nib as i32) - 8;
            sum += signed as f32 * vol as f32 / 64.0;
        }
        sum / self.channels_active as f32
    }
}

// ---------------------------------------------------------------- VRC7

/// VRC7 is a stripped Yamaha YM2413 (OPLL): 6 FM channels, no rhythm.
/// Round 2 ships a coarse approximation: the channel volumes and
/// fundamental frequencies are honoured; the FM operator math uses a
/// 2-operator sinusoidal stand-in instead of OPLL's logarithmic LUTs.
/// Real bit-exact OPLL is deferred; what we ship is enough to test
/// the channel-mix arithmetic and produce non-crashing audio for
/// VRC7-flagged NSFs.
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
            self.channels[ch].key_on = self.regs[0x20 + ch] & 0x10 != 0;
            self.channels[ch].volume = self.regs[0x30 + ch] & 0x0F;
        }
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
        // N163 timing is tied to the wavetable write cadence — no
        // per-cycle tick.
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
}
