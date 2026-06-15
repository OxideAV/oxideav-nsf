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

    /// Reads to `$4015` should clear the IRQ flag — round 2 records the
    /// flag for `$4015` reporting only.
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

/// Default per-device mix levels, in signed millibels (1/100 dB)
/// relative to the built-in APU square channel at its default volume,
/// per `docs/audio/nsf/nsfe-nesdev-wiki.html` §"mixe". The chunk's own
/// preamble states "Any omitted device should instead use a default
/// mix", and the device-byte list tabulates that default for every id:
///
/// ```text
/// 0 APU Squares ................. 0
/// 1 APU Triangle/Noise/DPCM .... -20  (compared via triangle)
/// 2 VRC6 ........................ 0
/// 3 VRC7 ..................... 1100  (compared via the §pseudo-square)
/// 4 FDS ...................... 700
/// 5 MMC5 ........................ 0
/// 6 N163 ..................... 1100  (compared in 1-channel mode)
/// 7 Sunsoft 5B ............... -130  (compared at volume 12 / $C)
/// ```
///
/// These apply to **every** NSF — a plain NSF v1 / NSF2 rip with no
/// `mixe` chunk, and an NSFe rip whose `mixe` chunk omits a device, all
/// inherit the tabulated default rather than a flat 0 dB. A present
/// `mixe` entry overrides only the device it names
/// ([`Apu2A03::apply_mixe_overrides`]).
///
/// Spec ambiguity (documented, not resolved here): the N163 row reads
/// "Default: 1100 or 1900" — the chunk preamble notes N163 mixing
/// "varies on a per-game basis", and the two listed values bracket that
/// range without the spec choosing one. We seed the lower bound 1100,
/// which coincides with the VRC7 default and is the conservative
/// reference point; an NSFe `mixe` chunk (the spec's intended N163
/// per-game mechanism) overrides it when the rip needs the louder mix.
pub const DEFAULT_MIX_MILLIBELS: [i16; MIXE_DEVICE_COUNT] = [
    0,    // APU squares (the reference)
    -20,  // APU triangle / noise / DPCM
    0,    // VRC6
    1100, // VRC7
    700,  // FDS
    0,    // MMC5
    1100, // N163 (spec: "1100 or 1900"; lower bound seeded)
    -130, // Sunsoft 5B
];

/// Convert a signed-millibel `mixe` level into a linear amplitude gain.
///
/// The `mixe` chunk specifies "millibels (1/100 dB) comparison with APU
/// square volume"; with the `dB = 20·log10(linear)` amplitude
/// convention this is `linear = 10^(mB / 2000)` (mB → dB is `/100`,
/// dB → linear amplitude is `10^(dB/20)`).
pub fn mix_millibels_to_gain(millibels: i16) -> f32 {
    10.0f32.powf(millibels as f32 / 2000.0)
}

/// The default per-device linear gain table derived from
/// [`DEFAULT_MIX_MILLIBELS`] via [`mix_millibels_to_gain`]. This is the
/// table a fresh [`Apu2A03`] starts with, before any NSFe `mixe`
/// override is applied.
pub fn default_device_gains() -> [f32; MIXE_DEVICE_COUNT] {
    let mut gains = [1.0f32; MIXE_DEVICE_COUNT];
    for (slot, &mb) in gains.iter_mut().zip(DEFAULT_MIX_MILLIBELS.iter()) {
        *slot = mix_millibels_to_gain(mb);
    }
    gains
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

    /// Linear gain per NSFe `mixe` device id. `1.0` = 0 dB / unchanged.
    /// Index 0 = APU squares, 1 = APU triangle/noise/DPCM, 2..=7 =
    /// VRC6 / VRC7 / FDS / MMC5 / N163 / 5B. Seeded from the §"mixe"
    /// tabulated [`DEFAULT_MIX_MILLIBELS`] (so a rip with no `mixe`
    /// chunk still mixes expansion audio at the documented relative
    /// loudness), then overridden per-device from a `Vec<NsfeMixerEntry>`
    /// via [`Apu2A03::apply_mixe_overrides`]. An override of `+X`
    /// millibels produces `10^(X/2000)` via [`mix_millibels_to_gain`]
    /// (the `dB = 20 * log10(linear)` convention from the §mixe spec).
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
            device_gain: default_device_gains(),
            expansion: crate::expansion::Expansion::new(),
        }
    }

    /// Apply NSFe `mixe` per-device millibel overrides. The spec says
    /// each entry is a signed 16-bit millibel comparison with the
    /// reference square at maximum volume — the player converts it to
    /// a linear gain via [`mix_millibels_to_gain`] and multiplies the
    /// channel's post-mixer contribution by that gain. Devices not
    /// mentioned by the entries keep their existing gain, which — per
    /// the §"mixe" "Any omitted device should instead use a default
    /// mix" rule — is the tabulated [`DEFAULT_MIX_MILLIBELS`] level a
    /// fresh [`Apu2A03`] is seeded with, not a flat 0 dB.
    pub fn apply_mixe_overrides(&mut self, entries: &[crate::nsfe::NsfeMixerEntry]) {
        for entry in entries {
            if (entry.device as usize) < MIXE_DEVICE_COUNT {
                self.device_gain[entry.device as usize] = mix_millibels_to_gain(entry.millibel);
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
    pub fn write_status(&mut self, value: u8) {
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
        // Reads to $4015 acknowledge / clear the frame-counter and
        // DMC IRQ flags per nesdev wiki §APU Status.
        self.frame_irq_flag = false;
        self.dmc.irq_flag = false;
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
        // Pulse / noise channels use a /2 prescaler off the CPU clock.
        // Triangle ticks every CPU clock.
        let pulse_cycles = cycles / 2;
        self.pulse1.tick_timer(pulse_cycles);
        self.pulse2.tick_timer(pulse_cycles);
        self.noise.tick_timer(pulse_cycles);
        self.triangle.tick_timer(cycles);

        // DMC ticks at the full CPU clock; one bit out per 'timer_period'.
        for _ in 0..cycles {
            self.dmc.tick_one();
        }

        // Expansion chips share the CPU clock.
        self.expansion.tick(cycles);

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
        let t = self.triangle.output() as f32 / 8227.0;
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
    fn default_mix_table_matches_spec_millibels() {
        // §"mixe" device-byte list defaults, in millibels.
        assert_eq!(DEFAULT_MIX_MILLIBELS, [0, -20, 0, 1100, 700, 0, 1100, -130]);
        // The conversion helper is the amplitude convention 10^(mB/2000).
        assert!((mix_millibels_to_gain(0) - 1.0).abs() < 1e-9);
        assert!((mix_millibels_to_gain(2000) - 10.0).abs() < 1e-4); // +20 dB
        assert!((mix_millibels_to_gain(-2000) - 0.1).abs() < 1e-5); // -20 dB
                                                                    // +6 dB ≈ 1.995x; -6 dB ≈ 0.501x.
        assert!((mix_millibels_to_gain(600) - 1.99526).abs() < 1e-3);
        assert!((mix_millibels_to_gain(-600) - 0.50119).abs() < 1e-3);
    }

    #[test]
    fn default_device_gains_seed_from_default_table() {
        let g = default_device_gains();
        for (i, &mb) in DEFAULT_MIX_MILLIBELS.iter().enumerate() {
            assert!((g[i] - mix_millibels_to_gain(mb)).abs() < 1e-9);
        }
        // A fresh APU starts from exactly this table.
        assert_eq!(Apu2A03::new().device_gains(), g);
        // VRC7 / FDS / N163 are louder than the square reference; the 5B
        // and TND defaults are quieter.
        assert!(g[mixe_device::VRC7 as usize] > 1.0);
        assert!(g[mixe_device::FDS as usize] > 1.0);
        assert!(g[mixe_device::N163 as usize] > 1.0);
        assert!(g[mixe_device::S5B as usize] < 1.0);
        assert!(g[mixe_device::APU_TND as usize] < 1.0);
        // VRC6, MMC5 and the square reference are at unity.
        assert!((g[mixe_device::VRC6 as usize] - 1.0).abs() < 1e-9);
        assert!((g[mixe_device::MMC5 as usize] - 1.0).abs() < 1e-9);
        assert!((g[mixe_device::APU_SQUARES as usize] - 1.0).abs() < 1e-9);
    }

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
        // Second read clears.
        let s2 = apu.read_status();
        assert_eq!(s2 & 0x80, 0);
    }

    #[test]
    fn apu_irq_line_or_of_frame_and_dmc_sources() {
        let mut apu = Apu2A03::new();
        assert!(!apu.irq_line());

        // Light up the frame counter IRQ path.
        apu.write_frame_counter(0x00); // 4-step mode, inhibit clear
        apu.tick_cpu_cycles(35_000);
        assert!(apu.irq_line(), "frame IRQ should assert the line");

        // $4015 read acks frame + DMC IRQs.
        apu.read_status();
        assert!(!apu.irq_line());

        // Inhibit + cleared flag should keep the line down.
        apu.write_frame_counter(0x40); // 4-step, inhibit set
        apu.tick_cpu_cycles(35_000);
        assert!(!apu.irq_line(), "inhibit must suppress frame IRQ");
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
