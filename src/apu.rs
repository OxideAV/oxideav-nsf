//! Clean-room 2A03 APU emulator.
//!
//! The NES APU sits inside the 2A03 die alongside the 6502 CPU. It owns
//! five sound channels:
//!
//! | Channel  | Registers       | Description                                |
//! |----------|-----------------|--------------------------------------------|
//! | Pulse 1  | `$4000..=$4003` | square wave, sweep + envelope + length     |
//! | Pulse 2  | `$4004..=$4007` | second square wave (no negate-bug variant) |
//! | Triangle | `$4008..=$400B` | 32-step triangle, linear + length counter  |
//! | Noise    | `$400C..=$400F` | LFSR with two tap modes                    |
//! | DMC      | `$4010..=$4013` | delta-modulation 7-bit DAC                 |
//!
//! Plus the global `$4015` status / channel-enable register and the
//! `$4017` frame-counter mode register.
//!
//! ## Output mixer
//!
//! Per nesdev.org/wiki/APU_Mixer the analogue mixer is non-linear:
//!
//! ```text
//!     pulse_out = 95.88 / (8128 / (p1 + p2) + 100)
//!     tnd_out   = 159.79 / (1 / (triangle/8227 + noise/12241 + dmc/22638) + 100)
//!     out       = pulse_out + tnd_out          (range ≈ 0.0 .. 1.0)
//! ```
//!
//! We compute that closed form once per 44.1 kHz output sample. There
//! is no lookup table in round 1 — it costs four divisions per sample,
//! which is fine for a 44.1 kHz mono stream.
//!
//! ## Frame counter
//!
//! The frame counter ticks in either 4-step or 5-step mode (chosen by
//! bit 7 of `$4017`). Each step advances envelopes / linear counter /
//! length counter / sweep. Mode 0 (4-step) clocks at NTSC ~240 Hz with
//! the IRQ on step 4; mode 1 (5-step) is silent and avoids the IRQ.
//! NSF playback never needs the IRQ — the player calls the play
//! routine directly off the host's wall clock — so we omit the IRQ
//! line entirely.

/// CPU clock frequency assumed by the APU timer divisors. The NES NTSC
/// CPU runs at 1.789773 MHz; PAL at 1.662607 MHz. We default to NTSC
/// and let the player adjust through `set_cpu_hz` if needed.
const NTSC_CPU_HZ: u32 = 1_789_773;

/// Length counter lookup table (nesdev.org/wiki/APU_Length_Counter).
const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

/// Pulse duty-cycle waveform table (nesdev.org/wiki/APU_Pulse).
/// Each row is one step in a 32-cycle cycle; `1` = high, `0` = low.
const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0], // 12.5 %
    [0, 1, 1, 0, 0, 0, 0, 0], // 25 %
    [0, 1, 1, 1, 1, 0, 0, 0], // 50 %
    [1, 0, 0, 1, 1, 1, 1, 1], // 75 % (negated 25 %)
];

/// Triangle channel sequencer table (32 entries, 0..=15).
const TRIANGLE_TABLE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15,
];

/// Noise period table (NTSC).
const NOISE_PERIOD_NTSC: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];

#[derive(Default)]
struct Envelope {
    start: bool,
    loop_flag: bool,
    constant: bool,
    volume: u8,  // also serves as period
    decay: u8,   // current decay level (0..=15)
    divider: u8, // counts down from `volume`; reload on 0
}

impl Envelope {
    fn write(&mut self, value: u8) {
        self.loop_flag = value & 0x20 != 0;
        self.constant = value & 0x10 != 0;
        self.volume = value & 0x0F;
    }

    fn clock(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.volume;
        } else if self.divider == 0 {
            self.divider = self.volume;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.loop_flag {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }

    fn output(&self) -> u8 {
        if self.constant {
            self.volume
        } else {
            self.decay
        }
    }
}

#[derive(Default)]
struct LengthCounter {
    halt: bool,
    counter: u8,
}

impl LengthCounter {
    fn write(&mut self, value: u8, enabled: bool) {
        if enabled {
            self.counter = LENGTH_TABLE[((value >> 3) & 0x1F) as usize];
        }
    }

    fn clock(&mut self) {
        if !self.halt && self.counter > 0 {
            self.counter -= 1;
        }
    }

    fn active(&self) -> bool {
        self.counter > 0
    }

    fn silence_if_disabled(&mut self, enabled: bool) {
        if !enabled {
            self.counter = 0;
        }
    }
}

#[derive(Default)]
struct PulseChannel {
    enabled: bool,
    duty: u8,
    duty_step: u8,
    timer_period: u16,
    timer: u16,
    envelope: Envelope,
    length: LengthCounter,

    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_reload: bool,
    sweep_divider: u8,

    /// Pulse 2 differs from Pulse 1 only in how the sweep negate adds
    /// the shifted period back: pulse 1 uses ones-complement (-target),
    /// pulse 2 uses twos-complement.
    is_pulse_two: bool,
}

impl PulseChannel {
    fn new(is_pulse_two: bool) -> Self {
        Self {
            is_pulse_two,
            ..Self::default()
        }
    }

    fn write_main(&mut self, value: u8) {
        // $4000 / $4004: DDLC NNNN
        self.duty = (value >> 6) & 0x03;
        self.length.halt = value & 0x20 != 0;
        self.envelope.write(value);
    }

    fn write_sweep(&mut self, value: u8) {
        self.sweep_enabled = value & 0x80 != 0;
        self.sweep_period = (value >> 4) & 0x07;
        self.sweep_negate = value & 0x08 != 0;
        self.sweep_shift = value & 0x07;
        self.sweep_reload = true;
    }

    fn write_period_lo(&mut self, value: u8) {
        self.timer_period = (self.timer_period & 0xFF00) | value as u16;
    }

    fn write_period_hi(&mut self, value: u8) {
        self.timer_period = (self.timer_period & 0x00FF) | (((value & 0x07) as u16) << 8);
        if self.enabled {
            self.length.write(value, true);
        }
        self.duty_step = 0;
        self.envelope.start = true;
    }

    fn tick_timer(&mut self, cycles: u32) {
        // The pulse channel has its own /2 prescaler off the CPU clock.
        for _ in 0..cycles {
            if self.timer == 0 {
                self.timer = self.timer_period;
                self.duty_step = (self.duty_step + 1) & 0x07;
            } else {
                self.timer -= 1;
            }
        }
    }

    fn clock_envelope(&mut self) {
        self.envelope.clock();
    }

    fn clock_length(&mut self) {
        self.length.clock();
    }

    fn target_period(&self) -> u16 {
        let shifted = self.timer_period >> self.sweep_shift;
        if self.sweep_negate {
            // Pulse 1: -shifted - 1; Pulse 2: -shifted.
            let bias = if self.is_pulse_two { 0 } else { 1 };
            self.timer_period.wrapping_sub(shifted).wrapping_sub(bias)
        } else {
            self.timer_period.wrapping_add(shifted)
        }
    }

    fn clock_sweep(&mut self) {
        let target = self.target_period();
        let mute = self.timer_period < 8 || target > 0x07FF;
        if self.sweep_divider == 0 && self.sweep_enabled && self.sweep_shift > 0 && !mute {
            self.timer_period = target;
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }

    fn output(&self) -> u8 {
        if !self.enabled
            || !self.length.active()
            || self.timer_period < 8
            || self.target_period() > 0x07FF
        {
            return 0;
        }
        let step = DUTY_TABLE[self.duty as usize][self.duty_step as usize];
        if step == 0 {
            0
        } else {
            self.envelope.output()
        }
    }
}

#[derive(Default)]
struct TriangleChannel {
    enabled: bool,
    timer_period: u16,
    timer: u16,
    seq_step: u8,
    length: LengthCounter,

    linear_counter: u8,
    linear_reload_value: u8,
    linear_reload: bool,
    control_flag: bool, // shares the length-halt bit
}

impl TriangleChannel {
    fn write_linear(&mut self, value: u8) {
        self.control_flag = value & 0x80 != 0;
        self.length.halt = self.control_flag;
        self.linear_reload_value = value & 0x7F;
    }

    fn write_period_lo(&mut self, value: u8) {
        self.timer_period = (self.timer_period & 0xFF00) | value as u16;
    }

    fn write_period_hi(&mut self, value: u8) {
        self.timer_period = (self.timer_period & 0x00FF) | (((value & 0x07) as u16) << 8);
        if self.enabled {
            self.length.write(value, true);
        }
        self.linear_reload = true;
    }

    fn tick_timer(&mut self, cycles: u32) {
        for _ in 0..cycles {
            if self.timer == 0 {
                self.timer = self.timer_period;
                if self.length.active() && self.linear_counter > 0 && self.timer_period >= 2 {
                    self.seq_step = (self.seq_step + 1) & 0x1F;
                }
            } else {
                self.timer -= 1;
            }
        }
    }

    fn clock_linear(&mut self) {
        if self.linear_reload {
            self.linear_counter = self.linear_reload_value;
        } else if self.linear_counter > 0 {
            self.linear_counter -= 1;
        }
        if !self.control_flag {
            self.linear_reload = false;
        }
    }

    fn clock_length(&mut self) {
        self.length.clock();
    }

    fn output(&self) -> u8 {
        if !self.enabled || self.timer_period < 2 {
            return 0;
        }
        TRIANGLE_TABLE[self.seq_step as usize]
    }
}

#[derive(Default)]
struct NoiseChannel {
    enabled: bool,
    mode: bool, // true = short-period (tap bit 6), false = long (tap bit 1)
    timer_period: u16,
    timer: u16,
    shift: u16,
    envelope: Envelope,
    length: LengthCounter,
}

impl NoiseChannel {
    fn new() -> Self {
        Self {
            shift: 1, // power-on state: a single 1 in the low bit
            ..Self::default()
        }
    }

    fn write_main(&mut self, value: u8) {
        self.length.halt = value & 0x20 != 0;
        self.envelope.write(value);
    }

    fn write_period(&mut self, value: u8) {
        self.mode = value & 0x80 != 0;
        self.timer_period = NOISE_PERIOD_NTSC[(value & 0x0F) as usize];
    }

    fn write_length(&mut self, value: u8) {
        if self.enabled {
            self.length.write(value, true);
        }
        self.envelope.start = true;
    }

    fn tick_timer(&mut self, cycles: u32) {
        for _ in 0..cycles {
            if self.timer == 0 {
                self.timer = self.timer_period;
                let tap_bit = if self.mode { 6 } else { 1 };
                let feedback = (self.shift & 1) ^ ((self.shift >> tap_bit) & 1);
                self.shift = (self.shift >> 1) | (feedback << 14);
            } else {
                self.timer -= 1;
            }
        }
    }

    fn clock_envelope(&mut self) {
        self.envelope.clock();
    }

    fn clock_length(&mut self) {
        self.length.clock();
    }

    fn output(&self) -> u8 {
        if !self.enabled || !self.length.active() || self.shift & 1 != 0 {
            return 0;
        }
        self.envelope.output()
    }
}

#[derive(Default)]
struct DmcChannel {
    enabled: bool,
    /// 7-bit DAC value at $4011 (low 7 bits).
    dac: u8,
    // Round 1: no sample fetcher; just expose the DAC.
}

/// 2A03 APU — five channels + frame counter + status / mixer.
pub struct Apu2A03 {
    cpu_hz: u32,
    pulse1: PulseChannel,
    pulse2: PulseChannel,
    triangle: TriangleChannel,
    noise: NoiseChannel,
    dmc: DmcChannel,

    /// `$4017` mode: false = 4-step, true = 5-step.
    five_step: bool,
    /// CPU cycles since the last frame-counter event.
    frame_acc: u32,
    /// Step counter (0..=3 in 4-step mode; 0..=4 in 5-step mode).
    frame_step: u8,
}

impl Default for Apu2A03 {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu2A03 {
    pub fn new() -> Self {
        Self {
            cpu_hz: NTSC_CPU_HZ,
            pulse1: PulseChannel::new(false),
            pulse2: PulseChannel::new(true),
            triangle: TriangleChannel::default(),
            noise: NoiseChannel::new(),
            dmc: DmcChannel::default(),
            five_step: false,
            frame_acc: 0,
            frame_step: 0,
        }
    }

    pub fn set_cpu_hz(&mut self, hz: u32) {
        self.cpu_hz = hz;
    }

    pub fn cpu_hz(&self) -> u32 {
        self.cpu_hz
    }

    /// Memory-mapped writes from the CPU (`$4000..=$4013`).
    pub fn write_register(&mut self, addr: u16, value: u8) {
        match addr {
            0x4000 => self.pulse1.write_main(value),
            0x4001 => self.pulse1.write_sweep(value),
            0x4002 => self.pulse1.write_period_lo(value),
            0x4003 => self.pulse1.write_period_hi(value),
            0x4004 => self.pulse2.write_main(value),
            0x4005 => self.pulse2.write_sweep(value),
            0x4006 => self.pulse2.write_period_lo(value),
            0x4007 => self.pulse2.write_period_hi(value),
            0x4008 => self.triangle.write_linear(value),
            0x400A => self.triangle.write_period_lo(value),
            0x400B => self.triangle.write_period_hi(value),
            0x400C => self.noise.write_main(value),
            0x400E => self.noise.write_period(value),
            0x400F => self.noise.write_length(value),
            0x4010 => { /* DMC IRQ + loop + rate — recorded for round 2 */ }
            0x4011 => self.dmc.dac = value & 0x7F,
            0x4012 | 0x4013 => { /* DMC sample addr / length — round 2 */ }
            _ => {}
        }
    }

    /// `$4015` write — channel enables.
    pub fn write_status(&mut self, value: u8) {
        self.pulse1.enabled = value & 0x01 != 0;
        self.pulse2.enabled = value & 0x02 != 0;
        self.triangle.enabled = value & 0x04 != 0;
        self.noise.enabled = value & 0x08 != 0;
        self.dmc.enabled = value & 0x10 != 0;
        self.pulse1.length.silence_if_disabled(self.pulse1.enabled);
        self.pulse2.length.silence_if_disabled(self.pulse2.enabled);
        self.triangle
            .length
            .silence_if_disabled(self.triangle.enabled);
        self.noise.length.silence_if_disabled(self.noise.enabled);
    }

    /// `$4015` read — status: which channel length counters are non-zero.
    pub fn read_status(&self) -> u8 {
        let mut s = 0u8;
        if self.pulse1.length.active() {
            s |= 0x01;
        }
        if self.pulse2.length.active() {
            s |= 0x02;
        }
        if self.triangle.length.active() {
            s |= 0x04;
        }
        if self.noise.length.active() {
            s |= 0x08;
        }
        s
    }

    /// `$4017` write — frame-counter mode.
    pub fn write_frame_counter(&mut self, value: u8) {
        self.five_step = value & 0x80 != 0;
        self.frame_acc = 0;
        self.frame_step = 0;
        if self.five_step {
            // 5-step: an immediate envelope + length tick on write.
            self.tick_quarter_frame();
            self.tick_half_frame();
        }
    }

    /// Advance every channel timer + the frame counter by `cycles`
    /// CPU clocks.
    pub fn tick_cpu_cycles(&mut self, cycles: u32) {
        // Pulse / noise channels use a /2 prescaler off the CPU clock.
        // Triangle ticks every CPU clock.
        let pulse_cycles = cycles / 2;
        self.pulse1.tick_timer(pulse_cycles);
        self.pulse2.tick_timer(pulse_cycles);
        self.noise.tick_timer(pulse_cycles);
        self.triangle.tick_timer(cycles);

        // Frame counter: NTSC has 4 evenly-spaced quarter-frame ticks
        // every 7457 CPU cycles (≈ 240 Hz). We don't model the actual
        // jitter — round 1 just counts cycles per event.
        const QUARTER_FRAME_CPU: u32 = 7457;
        self.frame_acc += cycles;
        while self.frame_acc >= QUARTER_FRAME_CPU {
            self.frame_acc -= QUARTER_FRAME_CPU;
            self.advance_frame_counter();
        }
    }

    fn advance_frame_counter(&mut self) {
        // 4-step: events at steps 0, 1, 2, 3; quarter on every step,
        // half on steps 1 and 3.
        // 5-step: events at steps 0, 1, 2, 3, 4; quarter on 0/1/2/3,
        // half on 1 and 4. Step 4 has no envelope tick.
        if self.five_step {
            match self.frame_step {
                0 | 2 => {
                    self.tick_quarter_frame();
                }
                1 => {
                    self.tick_quarter_frame();
                    self.tick_half_frame();
                }
                3 => {
                    self.tick_quarter_frame();
                }
                4 => {
                    self.tick_half_frame();
                }
                _ => {}
            }
            self.frame_step = (self.frame_step + 1) % 5;
        } else {
            match self.frame_step {
                0 | 2 => {
                    self.tick_quarter_frame();
                }
                1 | 3 => {
                    self.tick_quarter_frame();
                    self.tick_half_frame();
                }
                _ => {}
            }
            self.frame_step = (self.frame_step + 1) % 4;
        }
    }

    fn tick_quarter_frame(&mut self) {
        self.pulse1.clock_envelope();
        self.pulse2.clock_envelope();
        self.noise.clock_envelope();
        self.triangle.clock_linear();
    }

    fn tick_half_frame(&mut self) {
        self.pulse1.clock_length();
        self.pulse1.clock_sweep();
        self.pulse2.clock_length();
        self.pulse2.clock_sweep();
        self.triangle.clock_length();
        self.noise.clock_length();
    }

    /// Closed-form non-linear mix per nesdev.org/wiki/APU_Mixer.
    /// Output is in the range 0.0 .. ~1.0.
    pub fn output_sample(&self) -> f32 {
        let p1 = self.pulse1.output() as f32;
        let p2 = self.pulse2.output() as f32;
        let pulse_out = if (p1 + p2) <= 0.0 {
            0.0
        } else {
            95.88 / (8128.0 / (p1 + p2) + 100.0)
        };
        let t = self.triangle.output() as f32 / 8227.0;
        let n = self.noise.output() as f32 / 12241.0;
        let d = self.dmc.dac as f32 / 22638.0;
        let tnd_sum = t + n + d;
        let tnd_out = if tnd_sum <= 0.0 {
            0.0
        } else {
            159.79 / (1.0 / tnd_sum + 100.0)
        };
        pulse_out + tnd_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_writes_and_outputs_nonzero() {
        let mut apu = Apu2A03::new();
        apu.write_status(0x01); // enable pulse 1
        apu.write_register(0x4000, 0xBF); // duty 50%, halt, constant vol 15
        apu.write_register(0x4002, 0x10); // period lo
        apu.write_register(0x4003, 0x00); // period hi (timer = 0x010 = 16)
                                          // Spin a few thousand CPU cycles; the duty step should rotate.
        for _ in 0..1000 {
            apu.tick_cpu_cycles(8);
        }
        // Toggle a high duty step and observe non-zero output sometimes.
        // Force the duty step into a high slot to be deterministic:
        let mut saw_nonzero = false;
        for _ in 0..16 {
            apu.tick_cpu_cycles(64);
            if apu.output_sample() > 0.0 {
                saw_nonzero = true;
            }
        }
        assert!(saw_nonzero, "pulse channel never emitted output");
    }

    #[test]
    fn status_reflects_length_counter() {
        let mut apu = Apu2A03::new();
        apu.write_status(0x01);
        apu.write_register(0x4002, 0x40);
        apu.write_register(0x4003, 0x08); // length index 1 → counter 254
        assert_eq!(apu.read_status() & 0x01, 0x01);
        // Disable the channel: status bit clears.
        apu.write_status(0x00);
        assert_eq!(apu.read_status() & 0x01, 0x00);
    }

    #[test]
    fn noise_lfsr_advances() {
        let mut apu = Apu2A03::new();
        apu.write_status(0x08);
        apu.write_register(0x400C, 0x30); // halt + constant vol 0
        apu.write_register(0x400E, 0x00); // shortest period
        apu.write_register(0x400F, 0x08); // start
                                          // Tick enough cycles to flip the LFSR.
        for _ in 0..64 {
            apu.tick_cpu_cycles(8);
        }
        // The LFSR is now in some non-1 state.
        assert_ne!(apu.noise.shift, 1);
    }

    #[test]
    fn mixer_rests_at_zero() {
        let apu = Apu2A03::new();
        assert!(apu.output_sample().abs() < 1e-9);
    }
}
