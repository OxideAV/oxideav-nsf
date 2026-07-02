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
pub(crate) const LENGTH_TABLE: [u8; 32] = [
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

/// Noise period table (NTSC) — nesdev.org/wiki/APU_Noise §"Period".
const NOISE_PERIOD_NTSC: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];

/// Noise period table (PAL) — nesdev.org/wiki/APU_Noise §"Period". The
/// PAL 2A07 uses a distinct divider table because the channel timer
/// counts CPU cycles and the PAL CPU runs at a different rate; without
/// it a PAL rip's noise channel plays at the wrong pitch.
const NOISE_PERIOD_PAL: [u16; 16] = [
    4, 8, 14, 30, 60, 88, 118, 148, 188, 236, 354, 472, 708, 944, 1890, 3778,
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

    /// The sweep unit silences the channel when its current timer period
    /// is below 8 or the sweep adder's target overflows the 11-bit range
    /// (docs/audio/nsf/apu-pulse-wiki.html §"Pulse channel output to
    /// mixer": "the timer has a value less than eight" and "overflow
    /// from the sweep unit's adder is silencing the channel").
    ///
    /// The adder-overflow half only applies when the sweep shift count
    /// is non-zero: with a zero shift the adder produces no change
    /// amount, so it can never push a legitimately-low note's period
    /// out of range. (Without this guard a pulse that never configures a
    /// sweep — leaving shift at 0 — would mute every period above 0x3FF
    /// in add mode, silencing audible bass notes the §"Sequencer
    /// behavior" frequency range says should play.)
    fn sweep_mutes(&self) -> bool {
        self.timer_period < 8 || (self.sweep_shift > 0 && self.target_period() > 0x07FF)
    }

    fn clock_sweep(&mut self) {
        let target = self.target_period();
        if self.sweep_divider == 0
            && self.sweep_enabled
            && self.sweep_shift > 0
            && !self.sweep_mutes()
        {
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
        if !self.enabled || !self.length.active() || self.sweep_mutes() {
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

    /// Sequencer output expressed in half-steps (units of ½ level) so the
    /// ultrasonic "7.5" averaged value the spec describes can be returned
    /// without rounding bias. A normal step `s` is `2 * TRIANGLE_TABLE[s]`.
    fn output_halfsteps(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        // The triangle has no volume gate: its level is purely the
        // sequencer's current step. When the length counter or linear
        // counter is zero the sequencer stops advancing (see
        // `tick_timer`) and the channel *holds* its current step rather
        // than snapping to silence — the hardware's "halt it in whatever
        // its current output position is" behaviour
        // (docs/audio/nsf/apu-triangle-wiki.html §"silenced by several
        // methods"). When the period is ultrasonic (< 2) the sequencer
        // sweeps faster than the output rate can resolve and the lowpass
        // average is "halfway between 7 and 8" per the same section, so
        // we report the documented 7.5 (= 15 half-steps) instead of the
        // held step.
        if self.timer_period < 2 {
            return 15; // 7.5 in level units
        }
        2 * TRIANGLE_TABLE[self.seq_step as usize]
    }
}

#[derive(Default)]
struct NoiseChannel {
    enabled: bool,
    mode: bool, // true = short-period (tap bit 6), false = long (tap bit 1)
    /// Period selector (`$400E` low nibble) — kept so the period can be
    /// re-derived if the region (NTSC/PAL) flips after the write.
    period_index: u8,
    timer_period: u16,
    timer: u16,
    shift: u16,
    envelope: Envelope,
    length: LengthCounter,
    /// True when the PAL period table should be used.
    pal: bool,
}

impl NoiseChannel {
    fn new() -> Self {
        Self {
            shift: 1, // power-on state: a single 1 in the low bit
            ..Self::default()
        }
    }

    fn period_for(period_index: u8, pal: bool) -> u16 {
        let idx = (period_index & 0x0F) as usize;
        if pal {
            NOISE_PERIOD_PAL[idx]
        } else {
            NOISE_PERIOD_NTSC[idx]
        }
    }

    fn set_pal(&mut self, pal: bool) {
        self.pal = pal;
        self.timer_period = Self::period_for(self.period_index, pal);
    }

    fn write_main(&mut self, value: u8) {
        self.length.halt = value & 0x20 != 0;
        self.envelope.write(value);
    }

    fn write_period(&mut self, value: u8) {
        self.mode = value & 0x80 != 0;
        self.period_index = value & 0x0F;
        self.timer_period = Self::period_for(self.period_index, self.pal);
    }

    fn write_length(&mut self, value: u8) {
        if self.enabled {
            self.length.write(value, true);
        }
        self.envelope.start = true;
    }

    fn tick_timer(&mut self, cycles: u32) {
        // The `$400E` period table is in CPU cycles — "The period
        // determines how many CPU cycles happen between shift register
        // clocks" (docs/audio/nsf/apu-noise-wiki.html §Registers). So
        // the noise timer is driven at the full CPU clock and reloads
        // with `period - 1`, giving exactly `period` CPU cycles between
        // shifts. (Register $80 / period 4 then clocks at 1789773/4 ≈
        // 447443 Hz, matching the §"Pitches of 93-step noise" table.)
        for _ in 0..cycles {
            if self.timer == 0 {
                self.timer = self.timer_period.saturating_sub(1);
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

/// DMC rate table (NTSC). Each entry is the number of CPU cycles per
/// output bit. Index from `$4010` low nibble.
const DMC_RATE_NTSC: [u16; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];

/// DMC rate table (PAL).
const DMC_RATE_PAL: [u16; 16] = [
    398, 354, 316, 298, 276, 236, 210, 198, 176, 148, 132, 118, 98, 78, 66, 50,
];

/// Delta Modulation Channel.
///
/// nesdev.org/wiki/APU_DMC: a 1-bit delta sample stream that drives a
/// 7-bit DAC. Sample bytes are fetched from main memory through the
/// CPU bus by stalling the CPU for 4 cycles per fetch (round 2 omits
/// the stall — it would change the player schedule but not the sample
/// values for our music-only use case).
#[derive(Default)]
struct DmcChannel {
    enabled: bool,
    irq_enable: bool,
    loop_flag: bool,
    rate_index: u8,
    /// 7-bit DAC value at $4011.
    dac: u8,
    sample_addr_seed: u16,
    sample_len_seed: u16,

    current_addr: u16,
    bytes_remaining: u16,

    sample_buffer: u8,
    sample_buffer_filled: bool,

    output_shift: u8,
    output_bits: u8,
    output_silence: bool,

    timer: u16,
    timer_period: u16,

    /// Set true when a fetch is needed and `bytes_remaining > 0` and
    /// the buffer isn't already full. The bus drains pending fetches
    /// after each `tick_cpu_cycles` chunk.
    pending_fetch: bool,
    pending_fetch_addr: u16,

    /// DMC interrupt flag. Set when the bytes-remaining counter hits
    /// zero with the `$4010` IRQ-enable bit set; cleared by a `$4015`
    /// *write* or by clearing the `$4010` IRQ-enable bit — NOT by a
    /// `$4015` read (`docs/audio/nsf/apu-nesdev-wiki.html` §"Status
    /// ($4015)").
    irq_flag: bool,
}

impl DmcChannel {
    fn rate_for(rate_index: u8, pal: bool) -> u16 {
        let idx = (rate_index & 0x0F) as usize;
        if pal {
            DMC_RATE_PAL[idx]
        } else {
            DMC_RATE_NTSC[idx]
        }
    }

    fn write_control(&mut self, value: u8, pal: bool) {
        self.irq_enable = value & 0x80 != 0;
        self.loop_flag = value & 0x40 != 0;
        self.rate_index = value & 0x0F;
        self.timer_period = Self::rate_for(self.rate_index, pal);
        if !self.irq_enable {
            self.irq_flag = false;
        }
    }

    fn write_dac(&mut self, value: u8) {
        self.dac = value & 0x7F;
    }

    fn write_addr(&mut self, value: u8) {
        self.sample_addr_seed = 0xC000 | ((value as u16) << 6);
    }

    fn write_len(&mut self, value: u8) {
        self.sample_len_seed = ((value as u16) << 4) | 1;
    }

    fn restart_sample(&mut self) {
        self.current_addr = self.sample_addr_seed;
        self.bytes_remaining = self.sample_len_seed;
    }

    fn enable(&mut self, enable: bool) {
        self.enabled = enable;
        if !enable {
            self.bytes_remaining = 0;
        } else if self.bytes_remaining == 0 {
            self.restart_sample();
        }
    }

    /// Drain one CPU cycle's worth of DMC progress.
    fn tick_one(&mut self) {
        // Fetcher: re-fill the sample buffer if it's empty + bytes remain.
        if !self.sample_buffer_filled && self.bytes_remaining > 0 && !self.pending_fetch {
            self.pending_fetch = true;
            self.pending_fetch_addr = self.current_addr;
        }
        // Output unit: counts down `timer_period` then shifts a bit out.
        if self.timer == 0 {
            self.timer = self.timer_period.saturating_sub(1);
            self.shift_one_bit();
        } else {
            self.timer -= 1;
        }
    }

    fn shift_one_bit(&mut self) {
        if !self.output_silence {
            let bit_set = self.output_shift & 0x01 != 0;
            if bit_set && self.dac <= 125 {
                self.dac += 2;
            } else if !bit_set && self.dac >= 2 {
                self.dac -= 2;
            }
        }
        self.output_shift >>= 1;
        if self.output_bits > 0 {
            self.output_bits -= 1;
        }
        if self.output_bits == 0 {
            self.output_bits = 8;
            if !self.sample_buffer_filled {
                self.output_silence = true;
            } else {
                self.output_silence = false;
                self.output_shift = self.sample_buffer;
                self.sample_buffer_filled = false;
            }
        }
    }

    /// Bus calls this to surface a pending fetch address to the CPU bus.
    fn pending_fetch(&self) -> Option<u16> {
        if self.pending_fetch {
            Some(self.pending_fetch_addr)
        } else {
            None
        }
    }

    /// Bus calls this to deliver the byte that was at `pending_fetch_addr`.
    fn supply_byte(&mut self, byte: u8) {
        self.pending_fetch = false;
        self.sample_buffer = byte;
        self.sample_buffer_filled = true;
        self.current_addr = if self.current_addr == 0xFFFF {
            0x8000
        } else {
            self.current_addr + 1
        };
        if self.bytes_remaining > 0 {
            self.bytes_remaining -= 1;
        }
        if self.bytes_remaining == 0 {
            if self.loop_flag {
                self.restart_sample();
            } else if self.irq_enable {
                self.irq_flag = true;
            }
        }
    }
}

/// Number of distinct NSFe `mixe` device-id slots per
/// `docs/audio/nsf/nsfe-nesdev-wiki.html` §mixe. The spec enumerates
/// 0..=7:
/// `0 APU Squares / 1 APU Triangle+Noise+DPCM / 2 VRC6 / 3 VRC7 /
///  4 FDS / 5 MMC5 / 6 N163 / 7 Sunsoft 5B`.
pub const MIXE_DEVICE_COUNT: usize = 8;

/// `mixe` device-id constants for callers that want to construct a
/// table directly instead of going through the parsed NSFe metadata.
pub mod mixe_device {
    pub const APU_SQUARES: u8 = 0;
    pub const APU_TND: u8 = 1;
    pub const VRC6: u8 = 2;
    pub const VRC7: u8 = 3;
    pub const FDS: u8 = 4;
    pub const MMC5: u8 = 5;
    pub const N163: u8 = 6;
    pub const S5B: u8 = 7;
}

/// Per-device default mix levels in signed millibels, indexed by the
/// NSFe device id, per `docs/audio/nsf/nsfe-nesdev-wiki.html` §mixe
/// "Device byte values" — "Any omitted device should instead use a
/// default mix." Each entry is the device's loudness relative to the
/// APU square channel at its default volume (the §mixe comparison
/// reference), expressed in 1/100 dB:
///
/// * `0` APU Squares — Default: `0`
/// * `1` APU Triangle / Noise / DPCM — Default: `-20`
/// * `2` VRC6 — Default: `0`
/// * `3` VRC7 — Default: `1100`
/// * `4` FDS — Default: `700`
/// * `5` MMC5 — Default: `0`
/// * `6` N163 — Default: `1100` (1-channel-mode comparison)
/// * `7` Sunsoft 5B — Default: `-130`
///
/// DOCS-GAP — the §mixe table lists the N163 default as the literal
/// string "1100 or 1900" without resolving which value a player
/// should pick. The first-listed, more-conservative `1100` is used
/// here (it matches the §mixe "compared in 1-channel mode" note and
/// the VRC7 default magnitude); the staged wiki mirror does not
/// disambiguate, so the `1900` alternative is left for a clean-room
/// trace to settle.
pub const MIXE_DEFAULT_MILLIBELS: [i16; MIXE_DEVICE_COUNT] = [0, -20, 0, 1100, 700, 0, 1100, -130];

/// Convert a signed-millibel `mixe` comparison to a linear gain via
/// `10^(mB/2000)` (the `dB = 20·log10(linear)` convention from the
/// §mixe spec: millibels are 1/100 dB, so `mB/100` dB ÷ 20 = `mB/2000`
/// as the base-10 exponent).
fn mixe_millibel_to_linear(mb: i16) -> f32 {
    10.0f32.powf(mb as f32 / 2000.0)
}

/// 2A03 APU — five channels + frame counter + status / mixer + the
/// expansion-chip aggregate.
pub struct Apu2A03 {
    cpu_hz: u32,
    pulse1: PulseChannel,
    pulse2: PulseChannel,
    triangle: TriangleChannel,
    noise: NoiseChannel,
    dmc: DmcChannel,

    /// `$4017` mode: false = 4-step, true = 5-step.
    five_step: bool,
    /// `$4017` bit 6 — frame-interrupt inhibit. While set the
    /// frame-counter IRQ is suppressed AND any latched flag is
    /// cleared on the next `write_frame_counter` per nesdev spec.
    frame_irq_inhibit: bool,
    /// Frame-counter IRQ flag: in 4-step mode the flag latches at
    /// the end of every frame (just after step 3) when `irq_inhibit`
    /// is clear. Cleared by writing $4017 with bit 6 set or by
    /// reading `$4015` (which also acknowledges the DMC IRQ flag).
    /// 5-step mode never sets the flag per spec.
    frame_irq_flag: bool,
    /// CPU cycles since the last frame-counter event.
    frame_acc: u32,
    /// Step counter (0..=3 in 4-step mode; 0..=4 in 5-step mode).
    frame_step: u8,

    /// PAL flag — toggles the DMC rate table.
    pal: bool,

    /// Carry bit for the pulse channels' /2 (APU-cycle) prescaler. The
    /// CPU drives the APU one instruction at a time with cycle counts
    /// that are frequently odd, so dividing `cycles / 2` per call would
    /// drop up to half an APU cycle each instruction and slowly detune
    /// the pulse channels. Accumulating the dropped low bit here and
    /// releasing it on the next odd call keeps the pulse timer exact no
    /// matter how the CPU chunks its cycles.
    pulse_prescaler_carry: u32,

    /// Linear gain per NSFe `mixe` device id. Index 0 = APU squares,
    /// 1 = APU triangle/noise/DPCM, 2..=7 = VRC6 / VRC7 / FDS / MMC5 /
    /// N163 / 5B. Seeded from the §mixe per-device default mix levels
    /// ([`MIXE_DEFAULT_MILLIBELS`]) — the spec's "Any omitted device
    /// should instead use a default mix" — and overridden per device
    /// from a `Vec<NsfeMixerEntry>` via
    /// [`Apu2A03::apply_mixe_overrides`]. An override of `X` millibels
    /// produces `10^(X/2000)` (the `dB = 20 * log10(linear)`
    /// convention from the §mixe spec).
    device_gain: [f32; MIXE_DEVICE_COUNT],

    /// Aggregate of the active expansion chips.
    pub expansion: crate::expansion::Expansion,
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
            frame_irq_inhibit: false,
            frame_irq_flag: false,
            frame_acc: 0,
            frame_step: 0,
            pal: false,
            pulse_prescaler_carry: 0,
            device_gain: Self::default_device_gains(),
            expansion: crate::expansion::Expansion::new(),
        }
    }

    /// The §mixe per-device default gain table — each documented
    /// default mix level ([`MIXE_DEFAULT_MILLIBELS`]) converted to a
    /// linear gain. Used to seed [`Apu2A03::device_gain`] so that, per
    /// the spec's "Any omitted device should instead use a default
    /// mix", a device with no `mixe` entry plays at its documented
    /// level rather than a flat `1.0`.
    pub fn default_device_gains() -> [f32; MIXE_DEVICE_COUNT] {
        let mut g = [1.0f32; MIXE_DEVICE_COUNT];
        for (slot, &mb) in g.iter_mut().zip(MIXE_DEFAULT_MILLIBELS.iter()) {
            *slot = mixe_millibel_to_linear(mb);
        }
        g
    }

    /// Apply NSFe `mixe` per-device millibel overrides. The spec says
    /// each entry is a signed 16-bit millibel comparison with the
    /// reference square at maximum volume — the player converts it to
    /// a linear gain via `10^(mB/2000)` and multiplies the channel's
    /// post-mixer contribution by that gain. Devices not mentioned by
    /// the entries keep their §mixe default mix level (seeded at
    /// construction from [`MIXE_DEFAULT_MILLIBELS`]).
    pub fn apply_mixe_overrides(&mut self, entries: &[crate::nsfe::NsfeMixerEntry]) {
        for entry in entries {
            if (entry.device as usize) < MIXE_DEVICE_COUNT {
                self.device_gain[entry.device as usize] = mixe_millibel_to_linear(entry.millibel);
            }
        }
    }

    /// Inspect the current per-device gain table (mostly for tests).
    pub fn device_gains(&self) -> [f32; MIXE_DEVICE_COUNT] {
        self.device_gain
    }

    pub fn set_cpu_hz(&mut self, hz: u32) {
        self.cpu_hz = hz;
        self.pal = hz < 1_700_000;
        // Refresh the DMC + noise timer periods under the new rate /
        // period tables (both are CPU-rate-dependent on the PAL 2A07).
        self.dmc.timer_period = DmcChannel::rate_for(self.dmc.rate_index, self.pal);
        self.noise.set_pal(self.pal);
    }

    pub fn set_expansion(&mut self, flags: crate::header::ExpansionChips) {
        self.expansion.set_flags(flags);
    }

    pub fn write_expansion(&mut self, addr: u16, value: u8) {
        self.expansion.write(addr, value);
    }

    pub fn read_expansion(&mut self, addr: u16) -> u8 {
        self.expansion.read(addr)
    }

    /// Bus hook for `$8000..=$BFFF` reads — routed through to the
    /// expansion router so chips like MMC5 can implement the
    /// "Write-by-read writes to this register in PCM read-mode"
    /// semantic (`docs/audio/nsf/mmc5-audio-wiki.html` §"Raw PCM").
    pub fn observe_prg_read(&mut self, addr: u16, byte: u8) {
        self.expansion.observe_prg_read(addr, byte);
    }

    /// Bus pulls this every tick to see if a DMC sample byte is needed.
    pub fn dmc_pending_fetch(&self) -> Option<u16> {
        self.dmc.pending_fetch()
    }

    /// Bus calls this with the byte that was at the pending address.
    pub fn dmc_supply_byte(&mut self, byte: u8) {
        self.dmc.supply_byte(byte);
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
            0x4010 => self.dmc.write_control(value, self.pal),
            0x4011 => self.dmc.write_dac(value),
            0x4012 => self.dmc.write_addr(value),
            0x4013 => self.dmc.write_len(value),
            _ => {}
        }
    }

    /// `$4015` write — channel enables.
    ///
    /// Per `docs/audio/nsf/apu-nesdev-wiki.html` §"Status ($4015)":
    /// "Writing to this register clears the DMC interrupt flag." (The
    /// frame interrupt flag is NOT touched by a `$4015` write — it is
    /// cleared only by a `$4015` read or by setting the `$4017`
    /// interrupt-inhibit flag.)
    pub fn write_status(&mut self, value: u8) {
        self.dmc.irq_flag = false;
        self.pulse1.enabled = value & 0x01 != 0;
        self.pulse2.enabled = value & 0x02 != 0;
        self.triangle.enabled = value & 0x04 != 0;
        self.noise.enabled = value & 0x08 != 0;
        let dmc_enable = value & 0x10 != 0;
        self.dmc.enable(dmc_enable);
        self.pulse1.length.silence_if_disabled(self.pulse1.enabled);
        self.pulse2.length.silence_if_disabled(self.pulse2.enabled);
        self.triangle
            .length
            .silence_if_disabled(self.triangle.enabled);
        self.noise.length.silence_if_disabled(self.noise.enabled);
    }

    /// `$4015` read — status: which channel length counters are non-zero,
    /// plus DMC bytes-remaining + DMC IRQ flag.
    pub fn read_status(&mut self) -> u8 {
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
        if self.dmc.bytes_remaining > 0 {
            s |= 0x10;
        }
        if self.frame_irq_flag {
            s |= 0x40;
        }
        if self.dmc.irq_flag {
            s |= 0x80;
        }
        // Per docs/audio/nsf/apu-nesdev-wiki.html §"$4015 read":
        // "Reading this register clears the frame interrupt flag (but
        // not the DMC interrupt flag)." The DMC flag is cleared only by
        // a $4015 *write* or by clearing the $4010 IRQ-enable bit.
        self.frame_irq_flag = false;
        s
    }

    /// True iff the APU is currently asserting the CPU's IRQ line.
    /// Three sources: the frame-counter (clocked from `$4017`'s
    /// inhibit bit + 4-step mode), the DMC (end-of-sample +
    /// `$4010` bit 7 set), and the MMC5 PCM IRQ line wired through
    /// the expansion router per `docs/audio/nsf/mmc5-audio-wiki.html`
    /// §"IRQ operation" (round 18). The non-MMC5 expansion chips
    /// have no IRQ source.
    pub fn irq_line(&self) -> bool {
        self.frame_irq_flag || self.dmc.irq_flag || self.expansion.irq_line()
    }

    /// `$4017` write — frame-counter mode + IRQ inhibit.
    pub fn write_frame_counter(&mut self, value: u8) {
        self.five_step = value & 0x80 != 0;
        self.frame_irq_inhibit = value & 0x40 != 0;
        self.frame_acc = 0;
        self.frame_step = 0;
        if self.frame_irq_inhibit {
            // Spec §$4017: "If set, the frame interrupt flag is
            // cleared, otherwise it is unaffected."
            self.frame_irq_flag = false;
        }
        if self.five_step {
            // 5-step: an immediate envelope + length tick on write.
            self.tick_quarter_frame();
            self.tick_half_frame();
        }
    }

    /// Advance every channel timer + the frame counter by `cycles`
    /// CPU clocks.
    pub fn tick_cpu_cycles(&mut self, cycles: u32) {
        // Pulse channels use a /2 prescaler off the CPU clock (their
        // 11-bit timer counts APU cycles). Triangle ticks every CPU
        // clock. The noise channel's `$400E` period table is already
        // expressed in CPU cycles (the entries are even because an APU
        // cycle is 2 CPU cycles), so the noise timer is driven at the
        // full CPU clock — see `NoiseChannel::tick_timer`.
        let total = cycles + self.pulse_prescaler_carry;
        let pulse_cycles = total / 2;
        self.pulse_prescaler_carry = total & 1;
        self.pulse1.tick_timer(pulse_cycles);
        self.pulse2.tick_timer(pulse_cycles);
        self.noise.tick_timer(cycles);
        self.triangle.tick_timer(cycles);

        // DMC ticks at the full CPU clock; one bit out per 'timer_period'.
        for _ in 0..cycles {
            self.dmc.tick_one();
        }

        // Expansion chips share the CPU clock.
        self.expansion.tick(cycles);

        // Frame counter: advance through the region- and mode-specific
        // schedule of quarter-/half-frame events. The event positions
        // come straight from docs/audio/nsf/apu-frame-counter-wiki.html
        // §"Mode 0"/"Mode 1" (APU-cycle columns doubled to CPU cycles),
        // so the 4-step interrupt period is exactly the documented 29830
        // (NTSC) / 33254 (PAL) CPU cycles rather than a uniform 4×7457
        // approximation. `frame_acc` counts CPU cycles since the last
        // sequence reset; each scheduled offset fires its step once.
        self.frame_acc += cycles;
        loop {
            let schedule = self.frame_schedule();
            let n_steps = schedule.len() - 1;
            let period = schedule[n_steps];
            if (self.frame_step as usize) < n_steps {
                // Fire the next scheduled step once we reach its offset.
                let next_offset = schedule[self.frame_step as usize];
                if self.frame_acc >= next_offset {
                    self.advance_frame_counter();
                } else {
                    break;
                }
            } else {
                // All steps fired; wait for the period boundary, then
                // reset the sequence (drop one period, rewind to step 0).
                if self.frame_acc >= period {
                    self.frame_acc -= period;
                    self.frame_step = 0;
                } else {
                    break;
                }
            }
        }
    }

    /// CPU-cycle offsets, within one frame-sequence period, at which
    /// each step fires — the last entry is the period length itself.
    /// Derived by doubling the documented APU-cycle positions in
    /// `docs/audio/nsf/apu-frame-counter-wiki.html`.
    fn frame_schedule(&self) -> &'static [u32] {
        match (self.five_step, self.pal) {
            // 4-step: steps 0..=3 at 3728/7456/11185/14914 APU; reset at
            // 14915 APU → period 29830 CPU.
            (false, false) => &[7456, 14912, 22370, 29828, 29830],
            (false, true) => &[8312, 16626, 24938, 33252, 33254],
            // 5-step: steps 0..=4 at 3728/7456/11185/14914/18640 APU;
            // reset at 18641 APU → period 37282 CPU.
            (true, false) => &[7456, 14912, 22370, 29828, 37280, 37282],
            (true, true) => &[8312, 16626, 24938, 33252, 41564, 41566],
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
            self.frame_step += 1;
        } else {
            match self.frame_step {
                0 | 2 => {
                    self.tick_quarter_frame();
                }
                1 | 3 => {
                    self.tick_quarter_frame();
                    self.tick_half_frame();
                    if self.frame_step == 3 && !self.frame_irq_inhibit {
                        // 4-step mode latches the frame interrupt
                        // flag at the end of step 3 (the same
                        // event that issues the second half-frame
                        // tick). Spec: only 4-step mode raises the
                        // flag; 5-step never does.
                        self.frame_irq_flag = true;
                    }
                }
                _ => {}
            }
            self.frame_step += 1;
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

    /// Closed-form non-linear mix per nesdev.org/wiki/APU_Mixer, plus
    /// the linearly-mixed expansion-chip outputs, scaled by the NSFe
    /// `mixe` per-device gain table.
    ///
    /// Output is in the range 0.0 .. ~1.5 once expansion chips fire
    /// (gain overrides can push that higher).
    pub fn output_sample(&self) -> f32 {
        let p1 = self.pulse1.output() as f32;
        let p2 = self.pulse2.output() as f32;
        let pulse_out = if (p1 + p2) <= 0.0 {
            0.0
        } else {
            95.88 / (8128.0 / (p1 + p2) + 100.0)
        };
        // Triangle in half-steps (0..=30) over a doubled divisor keeps
        // the ultrasonic "7.5" averaged level precise in the mixer.
        let t = self.triangle.output_halfsteps() as f32 / (8227.0 * 2.0);
        let n = self.noise.output() as f32 / 12241.0;
        let d = self.dmc.dac as f32 / 22638.0;
        let tnd_sum = t + n + d;
        let tnd_out = if tnd_sum <= 0.0 {
            0.0
        } else {
            159.79 / (1.0 / tnd_sum + 100.0)
        };
        let core = pulse_out * self.device_gain[mixe_device::APU_SQUARES as usize]
            + tnd_out * self.device_gain[mixe_device::APU_TND as usize];
        core + self.expansion.output_with_device_gain(&self.device_gain)
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
    fn pulse_low_note_plays_without_sweep_configured() {
        // A pulse with a large period (low bass note) and no sweep set
        // up (shift 0) must NOT be silenced by the sweep adder. Period
        // 0x500 is well inside the audible range per the §"Sequencer
        // behavior" frequency formula.
        let mut p = PulseChannel::new(false);
        p.enabled = true;
        p.write_main(0xBF); // duty 50%, halt, constant vol 15
        p.write_period_lo(0x00);
        p.write_period_hi(0x05); // timer_period = 0x500, length loaded
        p.length.counter = 10;
        // sweep never configured → shift 0.
        assert!(!p.sweep_mutes(), "low note wrongly muted by sweep adder");
        // Drive the duty into a high slot and confirm non-zero output.
        let mut saw = false;
        for _ in 0..0x4000 {
            p.tick_timer(1);
            if p.output() > 0 {
                saw = true;
                break;
            }
        }
        assert!(saw, "low-period pulse produced no output");
    }

    #[test]
    fn pulse_sweep_overflow_still_mutes() {
        // With a non-zero shift, a target that overflows 0x7FF must
        // still silence the channel (the genuine sweep-mute case).
        let mut p = PulseChannel::new(false);
        p.enabled = true;
        p.length.counter = 10;
        p.write_period_lo(0x00);
        p.write_period_hi(0x07); // timer_period = 0x700
        p.write_sweep(0x01); // shift 1, add mode → target = 0x700 + 0x380 > 0x7FF
        assert!(p.sweep_mutes(), "sweep overflow should mute");
        assert_eq!(p.output(), 0);
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
    fn noise_shift_rate_matches_spec_sample_rate() {
        // docs/audio/nsf/apu-noise-wiki.html §"Pitches of 93-step noise":
        // register $80 (period index 0 → 4 CPU cycles) clocks the shift
        // register at 1789773 / 4 ≈ 447443 Hz NTSC. Count shifts over a
        // known span of CPU cycles and check the implied rate.
        let mut noise = NoiseChannel::new();
        noise.timer_period = NoiseChannel::period_for(0, false); // = 4
        let cpu_cycles = 4_000u32;
        let mut last = noise.shift;
        let mut shifts = 0u32;
        for _ in 0..cpu_cycles {
            noise.tick_timer(1);
            if noise.shift != last {
                shifts += 1;
                last = noise.shift;
            }
        }
        // 4000 CPU cycles / 4 = 1000 shifts (the LFSR very rarely repeats
        // an identical 15-bit state, so a missed count is negligible).
        assert!(
            (995..=1000).contains(&shifts),
            "expected ~1000 shifts in 4000 CPU cycles at period 4, got {shifts}"
        );
    }

    #[test]
    fn noise_period_table_follows_region() {
        // Default region is NTSC: the fastest index ($F) is 4068 cycles.
        let mut apu = Apu2A03::new();
        apu.write_register(0x400E, 0x0F);
        assert_eq!(apu.noise.timer_period, 4068);
        // Index $0 → 4 on both tables; index $5 differs (96 NTSC, 88 PAL).
        apu.write_register(0x400E, 0x05);
        assert_eq!(apu.noise.timer_period, 96);

        // Switch to a PAL clock: the period table flips and the stored
        // index is re-derived against the PAL table.
        apu.set_cpu_hz(1_662_607);
        assert_eq!(apu.noise.timer_period, 88, "index $5 PAL = 88");
        apu.write_register(0x400E, 0x0F);
        assert_eq!(apu.noise.timer_period, 3778, "index $F PAL = 3778");

        // Back to NTSC: the same index re-derives to the NTSC value.
        apu.set_cpu_hz(1_789_773);
        assert_eq!(apu.noise.timer_period, 4068);
    }

    #[test]
    fn pulse_prescaler_carry_is_chunk_invariant() {
        // The pulse /2 prescaler must produce the same duty-step phase
        // whether the CPU feeds cycles in one big block or many odd
        // chunks. Run two APUs with identical pulse setup over the same
        // total CPU cycles, one in a single 1000-cycle call and one in
        // 1000 single-cycle calls, and require the duty step to agree.
        let setup = |apu: &mut Apu2A03| {
            apu.write_status(0x01);
            apu.write_register(0x4000, 0xBF); // duty 50%, halt, const vol 15
            apu.write_register(0x4002, 0x05);
            apu.write_register(0x4003, 0x00); // period = 5
        };
        let mut bulk = Apu2A03::new();
        setup(&mut bulk);
        bulk.tick_cpu_cycles(1001); // odd total exercises the carry
        let mut drip = Apu2A03::new();
        setup(&mut drip);
        for _ in 0..1001 {
            drip.tick_cpu_cycles(1);
        }
        assert_eq!(
            bulk.pulse1.duty_step, drip.pulse1.duty_step,
            "pulse duty step must be invariant to CPU cycle chunking"
        );
        assert_eq!(bulk.pulse1.timer, drip.pulse1.timer);
    }

    #[test]
    fn triangle_holds_position_when_counters_expire() {
        // docs/audio/nsf/apu-triangle-wiki.html: when length/linear
        // counters reach zero the sequencer stops advancing and the
        // channel holds its current output position rather than snapping
        // to silence. Clock the triangle to a known step, then let the
        // counters expire and confirm the held step persists.
        let mut tri = TriangleChannel {
            enabled: true,
            ..TriangleChannel::default()
        };
        tri.write_linear(0x05); // short linear, control flag clear
        tri.write_period_lo(0x10);
        tri.write_period_hi(0x00); // period 16, sets length via enabled? no
        tri.linear_counter = 4;
        tri.length.counter = 4;
        tri.linear_reload = false;
        // Advance the sequencer a few steps.
        tri.tick_timer(16 * 5);
        let held = tri.output_halfsteps();
        // Drain the linear + length counters to zero.
        tri.linear_counter = 0;
        tri.length.counter = 0;
        // Further ticks must not advance the sequencer.
        tri.tick_timer(16 * 10);
        assert_eq!(
            tri.output_halfsteps(),
            held,
            "triangle must hold its last step"
        );
    }

    #[test]
    fn triangle_ultrasonic_reports_midpoint_not_silence() {
        // A period < 2 sweeps faster than the output resolves; the spec
        // gives the averaged level as 7.5. output_halfsteps must report
        // 15 (= 7.5) rather than 0 (the old hard-silence behaviour).
        let mut tri = TriangleChannel {
            enabled: true,
            ..TriangleChannel::default()
        };
        tri.write_period_lo(0x01);
        tri.write_period_hi(0x00); // period = 1 → ultrasonic
        assert_eq!(tri.output_halfsteps(), 15);
        // Disabled still silences.
        tri.enabled = false;
        assert_eq!(tri.output_halfsteps(), 0);
    }

    #[test]
    fn mixer_rests_at_zero() {
        let apu = Apu2A03::new();
        assert!(apu.output_sample().abs() < 1e-9);
    }

    #[test]
    fn dmc_address_seeded_by_4012() {
        // $4012 stores ((value << 6) | 0xC000). Value 0x10 → $C400.
        let mut apu = Apu2A03::new();
        apu.write_register(0x4012, 0x10);
        apu.write_register(0x4013, 0x01); // length = 17 bytes
        apu.write_status(0x10); // enable DMC
        assert_eq!(apu.dmc.sample_addr_seed, 0xC400);
        assert_eq!(apu.dmc.sample_len_seed, (1 << 4) | 1);
        assert_eq!(apu.dmc.current_addr, 0xC400);
        assert_eq!(apu.dmc.bytes_remaining, (1 << 4) | 1);
    }

    #[test]
    fn dmc_pending_fetch_drains_after_byte_supplied() {
        let mut apu = Apu2A03::new();
        apu.write_register(0x4012, 0x00); // address = $C000
        apu.write_register(0x4013, 0x01); // length = 17 bytes
        apu.write_status(0x10); // enable DMC
                                // Tick a few cycles; the channel should request a fetch.
        for _ in 0..4 {
            apu.tick_cpu_cycles(1);
        }
        let pending = apu.dmc_pending_fetch();
        assert_eq!(pending, Some(0xC000));
        apu.dmc_supply_byte(0xAB);
        // Address advances; bytes remaining decrements.
        assert_eq!(apu.dmc.current_addr, 0xC001);
        assert_eq!(apu.dmc.bytes_remaining, 16);
        assert!(apu.dmc.sample_buffer_filled);
        assert!(apu.dmc_pending_fetch().is_none());
    }

    #[test]
    fn dmc_status_bit_reflects_bytes_remaining() {
        let mut apu = Apu2A03::new();
        apu.write_register(0x4010, 0x0F); // pick fastest rate index → shortest fetch interval
        apu.write_register(0x4012, 0);
        apu.write_register(0x4013, 0x02); // 33 bytes total
        apu.write_status(0x10);
        let s0 = apu.read_status();
        assert_eq!(s0 & 0x10, 0x10);
        // Drain by handing over a byte every time one is requested.
        // 33 bytes × 8 bits × 54 cycles/bit (NTSC fastest) ≈ 14256 cycles.
        for _ in 0..30_000 {
            apu.tick_cpu_cycles(1);
            if let Some(_addr) = apu.dmc_pending_fetch() {
                apu.dmc_supply_byte(0);
            }
            if apu.dmc.bytes_remaining == 0 {
                break;
            }
        }
        assert_eq!(apu.dmc.bytes_remaining, 0, "DMC should be drained");
        let s1 = apu.read_status();
        assert_eq!(s1 & 0x10, 0);
    }

    #[test]
    fn dmc_irq_flag_sets_on_end_of_sample_when_armed() {
        let mut apu = Apu2A03::new();
        // IRQ enable, no loop, fastest rate.
        apu.write_register(0x4010, 0x80 | 0x0F);
        apu.write_register(0x4012, 0);
        apu.write_register(0x4013, 0); // 1 byte (length = 0 means 1 byte)
        apu.write_status(0x10);
        for _ in 0..2_000 {
            apu.tick_cpu_cycles(1);
            if let Some(_a) = apu.dmc_pending_fetch() {
                apu.dmc_supply_byte(0);
            }
            if apu.dmc.bytes_remaining == 0 {
                break;
            }
        }
        assert!(apu.dmc.irq_flag, "DMC IRQ flag should be set");
        let s = apu.read_status();
        assert_eq!(s & 0x80, 0x80);
        // §"$4015 read": "Reading this register clears the frame
        // interrupt flag (but not the DMC interrupt flag)." A second
        // read must still report the DMC flag.
        let s2 = apu.read_status();
        assert_eq!(s2 & 0x80, 0x80, "$4015 read must NOT clear DMC IRQ");
        // §"$4015 write": "Writing to this register clears the DMC
        // interrupt flag."
        apu.write_status(0x10);
        assert!(!apu.dmc.irq_flag, "$4015 write must clear DMC IRQ");
        let s3 = apu.read_status();
        assert_eq!(s3 & 0x80, 0);
    }

    #[test]
    fn dmc_irq_flag_cleared_by_4010_irq_disable() {
        // §"$4010": "IRQ enabled flag. If clear, the interrupt flag is
        // cleared."
        let mut apu = Apu2A03::new();
        apu.dmc.irq_flag = true;
        apu.write_register(0x4010, 0x80); // IRQ enable stays set
        assert!(apu.dmc.irq_flag, "IRQ-enable set must preserve the flag");
        apu.write_register(0x4010, 0x00); // IRQ enable cleared
        assert!(!apu.dmc.irq_flag, "clearing IRQ enable must clear the flag");
    }

    #[test]
    fn status_write_does_not_touch_frame_irq_flag() {
        // The $4015-write clear applies to the DMC interrupt flag only;
        // the frame interrupt flag is cleared by a $4015 *read* or the
        // $4017 inhibit bit, never by a $4015 write.
        let mut apu = Apu2A03::new();
        apu.frame_irq_flag = true;
        apu.write_status(0x1F);
        assert!(apu.frame_irq_flag, "$4015 write must not clear frame IRQ");
        apu.read_status();
        assert!(!apu.frame_irq_flag, "$4015 read clears frame IRQ");
    }

    #[test]
    fn apu_irq_line_or_of_frame_and_dmc_sources() {
        let mut apu = Apu2A03::new();
        assert!(!apu.irq_line());

        // Light up the frame counter IRQ path.
        apu.write_frame_counter(0x00); // 4-step mode, inhibit clear
        apu.tick_cpu_cycles(35_000);
        assert!(apu.irq_line(), "frame IRQ should assert the line");

        // $4015 read acks the frame IRQ (the DMC flag is not involved
        // here — a read never clears it).
        apu.read_status();
        assert!(!apu.irq_line());

        // Inhibit + cleared flag should keep the line down.
        apu.write_frame_counter(0x40); // 4-step, inhibit set
        apu.tick_cpu_cycles(35_000);
        assert!(!apu.irq_line(), "inhibit must suppress frame IRQ");
    }

    #[test]
    fn frame_counter_irq_fires_on_documented_schedule() {
        // docs/audio/nsf/apu-frame-counter-wiki.html §"Mode 0": in
        // 4-step NTSC mode the frame interrupt flag is set at the final
        // step (APU 14914 → CPU 29828) and the whole sequence period is
        // 29830 CPU cycles. Feed cycles one at a time so the first
        // assertion lands at the documented offset.
        let mut apu = Apu2A03::new();
        apu.write_frame_counter(0x00); // 4-step, inhibit clear
        let mut fired_at = None;
        for c in 1..=30_000u32 {
            apu.tick_cpu_cycles(1);
            if apu.frame_irq_flag {
                fired_at = Some(c);
                break;
            }
        }
        assert_eq!(fired_at, Some(29_828), "4-step IRQ flag at CPU 29828");

        // Acknowledge, then confirm the next assertion is exactly one
        // documented period (29830 CPU cycles) later.
        apu.read_status();
        let mut next_at = None;
        for c in 1..=40_000u32 {
            apu.tick_cpu_cycles(1);
            if apu.frame_irq_flag {
                next_at = Some(c);
                break;
            }
        }
        assert_eq!(next_at, Some(29_830), "4-step IRQ period = 29830 CPU");
    }

    #[test]
    fn frame_counter_pal_period_is_documented() {
        // PAL 4-step period is 33254 CPU cycles (APU 16627 × 2).
        let mut apu = Apu2A03::new();
        apu.set_cpu_hz(1_662_607); // PAL clock
        apu.write_frame_counter(0x00);
        let mut fired_at = None;
        for c in 1..=40_000u32 {
            apu.tick_cpu_cycles(1);
            if apu.frame_irq_flag {
                fired_at = Some(c);
                break;
            }
        }
        assert_eq!(fired_at, Some(33_252), "PAL 4-step IRQ flag at CPU 33252");
    }

    #[test]
    fn frame_counter_irq_flag_in_status_byte_acknowledges_on_read() {
        let mut apu = Apu2A03::new();
        apu.write_frame_counter(0x00);
        apu.tick_cpu_cycles(35_000);
        let s = apu.read_status();
        assert!(s & 0x40 != 0, "$4015 bit 6 = frame IRQ flag");
        let s2 = apu.read_status();
        assert_eq!(s2 & 0x40, 0);
    }
}
