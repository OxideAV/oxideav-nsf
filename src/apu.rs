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

    /// The sweep unit's continuously-computed target period, per
    /// `docs/audio/nsf/apu-sweep-wiki.html` §"Calculating the target
    /// period": a barrel shifter shifts the 11-bit raw timer period
    /// right by the shift count to produce the change amount; the
    /// negate flag makes the change amount negative; and "The target
    /// period is the sum of the current period and the change amount,
    /// clamped to zero if this sum is negative."
    ///
    /// "The two pulse channels have their adders' carry inputs wired
    /// differently": pulse 1 adds the ones' complement (−c − 1, so
    /// making 20 negative produces −21), pulse 2 the two's complement
    /// (−c).
    fn target_period(&self) -> u16 {
        let change = (self.timer_period >> self.sweep_shift) as i32;
        if self.sweep_negate {
            let bias = if self.is_pulse_two { 0 } else { 1 };
            (self.timer_period as i32 - change - bias).max(0) as u16
        } else {
            // Period ≤ 0x7FF and change ≤ period, so no u16 overflow.
            self.timer_period + change as u16
        }
    }

    /// Sweep-unit muting, per `docs/audio/nsf/apu-sweep-wiki.html`
    /// §"Muting": "If the current period is less than 8" or "If at any
    /// time the target period is greater than $7FF, the sweep unit
    /// mutes the channel."
    ///
    /// "Muting happens regardless of whether the sweep unit is
    /// disabled (because either the Enabled flag or the Shift count
    /// are zero) and regardless of whether the sweep divider is
    /// outputting a clock signal." In particular, with the negate flag
    /// false and the shift count zero the change amount equals the
    /// current period, so any period ≥ $400 targets > $7FF and mutes —
    /// the doc's "why several publishers' NES games never seem to use
    /// the bottom octave of the pulse waves". To fully disable the
    /// sweep unit a program must turn on the negate flag (e.g. write
    /// $08), which keeps the target at or below the current period.
    ///
    /// (This deliberately reverses an earlier shift-count-zero
    /// carve-out that predated the dedicated sweep page being staged —
    /// the page pins the mute as unconditional.)
    fn sweep_mutes(&self) -> bool {
        self.timer_period < 8 || self.target_period() > 0x07FF
    }

    /// Half-frame sweep clock, per `docs/audio/nsf/apu-sweep-wiki.html`
    /// §"Updating the period": when the divider's counter is zero, the
    /// sweep is enabled, the shift count is nonzero ("If SSS is 0, then
    /// behaves like E=0") and the unit is not muting, the period is set
    /// to the target; while muting "the pulse's period remains
    /// unchanged, but the sweep unit's divider continues to count down
    /// and reload the divider's period as normal". Then "If the
    /// divider's counter is zero or the reload flag is true: The
    /// divider counter is set to P and the reload flag is cleared.
    /// Otherwise, the divider counter is decremented."
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

impl Default for TriangleChannel {
    /// Power-up state. The staged docs do not pin the sequencer's
    /// power-up position; we seed it at step 15 (the sequence's zero
    /// output value) so a never-played triangle holds silence rather
    /// than an arbitrary DC step. Once the channel has run, every
    /// silencing method holds whatever step it stopped on, per the
    /// documented behaviour (see `output_halfsteps`).
    fn default() -> Self {
        Self {
            enabled: false,
            timer_period: 0,
            timer: 0,
            seq_step: 15,
            length: LengthCounter::default(),
            linear_counter: 0,
            linear_reload_value: 0,
            linear_reload: false,
            control_flag: false,
        }
    }
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
    /// True while the sequencer is actually advancing (mirrors the
    /// gate in `tick_timer`): both counters non-zero and the period is
    /// not ultrasonic-halted.
    fn sequencer_running(&self) -> bool {
        self.length.active() && self.linear_counter > 0
    }

    fn output_halfsteps(&self) -> u8 {
        // The triangle has no volume gate: its level is purely the
        // sequencer's current step. EVERY silencing method — length or
        // linear counter reaching zero, and the `$4015` disable (which
        // merely clears the length counter) — stops the sequencer and
        // *holds* its current step rather than snapping to silence:
        // "Silencing the triangle channel merely halts it. It will
        // continue to output its last value rather than 0"
        // (docs/audio/nsf/apu-nesdev-wiki.html §Triangle), and the
        // `$4015` method is listed among the ways that "halt it in
        // whatever its current output position is"
        // (docs/audio/nsf/apu-triangle-wiki.html §"silenced by several
        // methods"). When the period is ultrasonic (< 2) *and the
        // sequencer is cycling*, it sweeps faster than the output rate
        // can resolve and the lowpass average is "halfway between 7
        // and 8" per the same section, so we report the documented 7.5
        // (= 15 half-steps); a halted channel holds its step instead,
        // whatever the period. The residual DC a held step leaves in
        // the mix is what the documented post-DAC high-pass chain
        // removes downstream.
        if self.timer_period < 2 && self.sequencer_running() {
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

/// CPU cycles a **"load" DMC DMA** steals from the CPU — the first
/// fetch after a `$4015` D4 write with the sample buffer empty.
///
/// `docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA": load DMAs "are
/// scheduled to halt the CPU on a get cycle during the 2nd APU cycle
/// after the write", and DMC DMA "performs a halt cycle, a dummy
/// cycle, an optional alignment cycle, and a get". With the halt
/// landing on a get cycle, the dummy occupies the put half and the
/// sample read lands on the very next get — no alignment cycle is
/// needed, so "load DMAs take 3 cycles".
///
/// This is the *undelayed* count. The bus DMA engine places every
/// halt on its exact CPU cycle: §Behavior write-cycle halt delays
/// ("Delays of up to 3 cycles are possible, with read-modify-write
/// instructions having 2 consecutive writes and interrupts having 3")
/// flip the halt parity when odd, and the live stall then becomes 4
/// (see `crate::bus::NesBus::run_instruction`).
pub const DMC_DMA_LOAD_STALL_CYCLES: u32 = 3;

/// CPU cycles a **"reload" DMC DMA** steals from the CPU — every fetch
/// triggered by the sample buffer emptying at the end of an output
/// cycle.
///
/// `docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA": reload DMAs "are
/// scheduled to halt the CPU on a put cycle", so the halt occupies a
/// put, the dummy the following get, an alignment cycle the next put,
/// and the sample read the get after that — "reload DMAs take 4"
/// cycles. This is the *undelayed* count; an odd write-cycle halt
/// delay flips it to 3 (see [`DMC_DMA_LOAD_STALL_CYCLES`]).
pub const DMC_DMA_RELOAD_STALL_CYCLES: u32 = 4;

/// Historical flat per-fetch stall estimate from
/// `docs/audio/nsf/apu-dmc-wiki.html` §"Memory reader" ("The CPU is
/// stalled for 1-4 CPU cycles to read a sample byte"), used while the
/// DMA article was not yet staged. The article now is, and the live
/// path accounts [`DMC_DMA_LOAD_STALL_CYCLES`] /
/// [`DMC_DMA_RELOAD_STALL_CYCLES`] per fetch type instead.
#[deprecated(
    note = "the DMA page is staged; use DMC_DMA_LOAD_STALL_CYCLES / DMC_DMA_RELOAD_STALL_CYCLES"
)]
pub const DMC_DMA_STALL_CYCLES: u32 = 4;

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
/// CPU bus, stalling the CPU per fetch (see
/// [`DMC_DMA_LOAD_STALL_CYCLES`] / [`DMC_DMA_RELOAD_STALL_CYCLES`]).
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
    /// True while the bus-side DMA engine has taken the pending fetch
    /// (via [`Apu2A03::dmc_take_fetch`]) but the DMA read has not yet
    /// completed ([`DmcChannel::supply_byte`]). While a fetch is in
    /// flight the sample buffer stays empty — exactly as on hardware,
    /// where the byte only arrives on the DMA's get cycle — and
    /// `tick_one` must not arm a duplicate fetch.
    fetch_in_flight: bool,
    /// Address of the most recent fetch handed to the DMA engine.
    /// Needed by the apu-dma-wiki §Bugs "unexpected DMA" modelling:
    /// the extra reload DMA "occurs from the same address" as the
    /// sample's final fetch.
    last_fetch_addr: u16,
    /// Latched when the sample ends implicitly: the output cycle whose
    /// end moves the final buffered byte into the shift register (the
    /// moment a reload DMA "would schedule" per apu-dma-wiki §Bugs,
    /// except bytes remaining is zero). Drained by the bus DMA engine,
    /// which decides between the aborted-DMA and unexpected-DMA bug
    /// outcomes from the cycle's get/put parity.
    implicit_stop_event: bool,
    /// True when the *next* fetch to arm is a "load" DMA — i.e. it was
    /// initiated by a `$4015` D4 write with the sample buffer empty,
    /// rather than by the buffer emptying at the end of an output
    /// cycle. Load and reload DMAs halt the CPU on opposite get/put
    /// parities and so steal different cycle counts
    /// (`docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA").
    next_fetch_is_load: bool,
    /// Captured [`Self::next_fetch_is_load`] for the armed fetch.
    pending_fetch_is_load: bool,

    /// DMC interrupt flag. Set when the bytes-remaining counter hits
    /// zero with the `$4010` IRQ-enable bit set; cleared by a `$4015`
    /// *write* or by clearing the `$4010` IRQ-enable bit — NOT by a
    /// `$4015` read (`docs/audio/nsf/apu-nesdev-wiki.html` §"Status
    /// ($4015)").
    irq_flag: bool,
}

impl Default for DmcChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl DmcChannel {
    /// Power-up state. The output unit starts *silent*: the sample
    /// buffer is empty at power-up and
    /// `docs/audio/nsf/apu-dmc-wiki.html` §"Output unit" says "If the
    /// sample buffer is empty, then the silence flag is set" at the
    /// start of every output cycle, and "The DPCM unit can only
    /// transition from silent to playing at the end of an output
    /// cycle." A clear silence flag at power-up would let the very
    /// first timer clock apply a delta step from the (empty) shift
    /// register to the output level — audibly corrupting a `$4011`
    /// direct-load PCM level before any sample ever plays. The
    /// bits-remaining counter starts at 8 (a fresh output cycle);
    /// "the output level is loaded with 0 on power-up" per §Overview.
    fn new() -> Self {
        Self {
            enabled: false,
            irq_enable: false,
            loop_flag: false,
            rate_index: 0,
            dac: 0,
            sample_addr_seed: 0,
            sample_len_seed: 0,
            current_addr: 0,
            bytes_remaining: 0,
            sample_buffer: 0,
            sample_buffer_filled: false,
            output_shift: 0,
            output_bits: 8,
            output_silence: true,
            timer: 0,
            timer_period: 0,
            pending_fetch: false,
            pending_fetch_addr: 0,
            fetch_in_flight: false,
            last_fetch_addr: 0,
            implicit_stop_event: false,
            next_fetch_is_load: false,
            pending_fetch_is_load: false,
            irq_flag: false,
        }
    }

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
            self.next_fetch_is_load = false;
            // Cancel an armed-but-unserviced fetch: clearing $4015 D4
            // zeroes bytes remaining, and the memory reader only
            // fetches while "the sample buffer is in an empty state
            // and bytes remaining is not zero"
            // (docs/audio/nsf/apu-dmc-wiki.html §"Memory reader").
            // apu-dma-wiki §Bugs concurs on the hardware outcome for a
            // DMA already scheduled at the stop: it "is aborted after
            // a single cycle" without performing its read, so the
            // sample buffer must NOT be filled from a stale fetch —
            // previously a caller draining the queue after the disable
            // would fetch anyway and a later re-enable played one
            // never-fetched byte. (The abort's 1-cycle stall itself
            // needs sub-instruction stop timing and is not modelled.)
            self.pending_fetch = false;
        } else if self.bytes_remaining == 0 {
            self.restart_sample();
            // apu-dma-wiki §"DMC DMA": "Load DMAs occur after $4015 D4
            // is set, but only if the sample buffer is empty." The
            // fetch this write starts is the 3-cycle load DMA; every
            // buffer-emptied refetch after it is a 4-cycle reload.
            if !self.sample_buffer_filled && !self.pending_fetch {
                self.next_fetch_is_load = true;
            }
        }
    }

    /// Drain one CPU cycle's worth of DMC progress.
    ///
    /// The output unit steps *before* the fetch-need check so that an
    /// output cycle whose end empties the sample buffer arms its
    /// reload fetch on this same CPU cycle — the bus DMA engine
    /// derives the reload halt attempt ("scheduled to halt the CPU on
    /// a put cycle", apu-dma-wiki §"DMC DMA") from the cycle the
    /// buffer emptied, so the arm must not lag it.
    fn tick_one(&mut self) {
        // Output unit: counts down `timer_period` then shifts a bit out.
        if self.timer == 0 {
            self.timer = self.timer_period.saturating_sub(1);
            self.shift_one_bit();
        } else {
            self.timer -= 1;
        }
        // Fetcher: re-fill the sample buffer if it's empty + bytes remain.
        if !self.sample_buffer_filled
            && self.bytes_remaining > 0
            && !self.pending_fetch
            && !self.fetch_in_flight
        {
            self.pending_fetch = true;
            self.pending_fetch_addr = self.current_addr;
            self.pending_fetch_is_load = self.next_fetch_is_load;
            self.next_fetch_is_load = false;
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
                if self.bytes_remaining == 0 && self.enabled {
                    // The final buffered byte just moved into the
                    // shift register: sample playback stops implicitly
                    // at this output-cycle end — the exact moment
                    // apu-dma-wiki §Bugs says a reload DMA "would
                    // schedule". Latch the event for the bus DMA
                    // engine's aborted-/unexpected-DMA modelling.
                    self.implicit_stop_event = true;
                }
            }
        }
    }

    /// Bus calls this to surface a pending fetch to the CPU bus:
    /// the sample address plus the CPU cycles the DMA halt steals
    /// (3 for a load DMA, 4 for a reload DMA — see
    /// [`DMC_DMA_LOAD_STALL_CYCLES`] / [`DMC_DMA_RELOAD_STALL_CYCLES`]).
    fn pending_fetch(&self) -> Option<(u16, u32)> {
        if self.pending_fetch {
            let stall = if self.pending_fetch_is_load {
                DMC_DMA_LOAD_STALL_CYCLES
            } else {
                DMC_DMA_RELOAD_STALL_CYCLES
            };
            Some((self.pending_fetch_addr, stall))
        } else {
            None
        }
    }

    /// Bus calls this to deliver the byte that was at `pending_fetch_addr`.
    fn supply_byte(&mut self, byte: u8) {
        self.pending_fetch = false;
        self.fetch_in_flight = false;
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

/// One scheduled frame-sequence event: at `offset` CPU cycles from the
/// sequence start, optionally clock the quarter-frame units (envelopes
/// plus triangle linear counter), the half-frame units (length counters
/// plus sweeps), and/or set the frame interrupt flag.
#[derive(Clone, Copy)]
struct FrameEvent {
    offset: u32,
    quarter: bool,
    half: bool,
    irq: bool,
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

    /// Total CPU cycles ever ticked — the low bit is the CPU/APU phase
    /// (an APU cycle spans two CPU cycles; even = first half, odd =
    /// second half), which decides the `$4017` write-effect delay.
    total_cycles: u64,

    /// Pending `$4017` side-effect countdown, in CPU cycles. Per
    /// `docs/audio/nsf/apu-frame-counter-wiki.html` §"Side effects" the
    /// timer reset (and, in 5-step mode, the immediate quarter+half
    /// clock) does not happen on the write cycle itself: "After 3 or 4
    /// CPU clock cycles*, the timer is reset. […] * If the write
    /// occurs during an APU cycle, the effects occur 3 CPU cycles
    /// after the $4017 write cycle, and if the write occurs between
    /// APU cycles, the effects occurs 4 CPU cycles after the write
    /// cycle." Either way the effects land on the same CPU/APU phase.
    frame_reset_delay: Option<u32>,

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
            total_cycles: 0,
            frame_reset_delay: None,
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

    /// Whether a DMC sample byte is needed. Returns the fetch address
    /// and the *undelayed* CPU-cycle cost of the DMA:
    /// [`DMC_DMA_LOAD_STALL_CYCLES`] for the post-`$4015` "load" DMA,
    /// [`DMC_DMA_RELOAD_STALL_CYCLES`] for a buffer-emptied "reload"
    /// DMA (`docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA"). The bus
    /// DMA engine consumes fetches through [`Apu2A03::dmc_take_fetch`]
    /// instead and computes the live stall from the actual halt
    /// cycle's parity; this accessor remains for direct APU users.
    pub fn dmc_pending_fetch(&self) -> Option<(u16, u32)> {
        self.dmc.pending_fetch()
    }

    /// Bus calls this with the byte that was at the pending address.
    pub fn dmc_supply_byte(&mut self, byte: u8) {
        self.dmc.supply_byte(byte);
    }

    /// Bus DMA engine: take ownership of the armed DMC fetch, marking
    /// it in flight (the sample buffer stays empty until the engine
    /// completes the DMA read and calls [`Apu2A03::dmc_supply_byte`],
    /// exactly as on hardware). Returns `(address, is_load_dma)`.
    #[doc(hidden)]
    pub fn dmc_take_fetch(&mut self) -> Option<(u16, bool)> {
        if self.dmc.pending_fetch {
            self.dmc.pending_fetch = false;
            self.dmc.fetch_in_flight = true;
            self.dmc.last_fetch_addr = self.dmc.pending_fetch_addr;
            Some((self.dmc.pending_fetch_addr, self.dmc.pending_fetch_is_load))
        } else {
            None
        }
    }

    /// Bus DMA engine: drain the implicit-stop latch (the output-cycle
    /// end that moved the sample's final byte into the shift register
    /// — apu-dma-wiki §Bugs' "reload DMA would schedule" moment).
    #[doc(hidden)]
    pub fn dmc_take_implicit_stop(&mut self) -> bool {
        std::mem::take(&mut self.dmc.implicit_stop_event)
    }

    /// Address of the most recent DMA fetch — the apu-dma-wiki §Bugs
    /// "unexpected DMA" reloads "from the same address".
    #[doc(hidden)]
    pub fn dmc_last_fetch_addr(&self) -> u16 {
        self.dmc.last_fetch_addr
    }

    /// Bus DMA engine: cancel an in-flight/armed fetch whose DMA was
    /// aborted or cancelled before its read cycle (apu-dma-wiki §Bugs
    /// — the aborted DMA never performs its read, so the sample
    /// buffer must stay empty).
    #[doc(hidden)]
    pub fn dmc_cancel_in_flight(&mut self) {
        self.dmc.fetch_in_flight = false;
        self.dmc.pending_fetch = false;
    }

    /// Bus DMA engine: deliver the apu-dma-wiki §Bugs "unexpected DMA"
    /// byte. It only fills the sample buffer ("This extra byte goes
    /// into the sample buffer and is played after the current byte
    /// finishes") — the address/bytes-remaining bookkeeping already
    /// finished with the sample's real final fetch.
    #[doc(hidden)]
    pub fn dmc_supply_unexpected_byte(&mut self, byte: u8) {
        self.dmc.sample_buffer = byte;
        self.dmc.sample_buffer_filled = true;
    }

    /// True while DMC DMA activity is possible — the bus uses this to
    /// drop from its cycle-exact walk into the cheap chunked tick when
    /// no fetch can arm.
    #[doc(hidden)]
    pub fn dmc_activity_possible(&self) -> bool {
        self.dmc.pending_fetch
            || self.dmc.fetch_in_flight
            || (self.dmc.enabled && (self.dmc.bytes_remaining > 0 || self.dmc.sample_buffer_filled))
    }

    /// True when the machine uses the PAL (2A07-class) CPU/APU. The
    /// apu-dma-wiki §Bugs stop-timing quirks are gated off for PAL
    /// ("It is not known whether 2A07 CPUs are affected by these
    /// bugs").
    #[doc(hidden)]
    pub fn is_pal(&self) -> bool {
        self.pal
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
        self.write_status_except_dmc_enable(value);
        self.dmc_set_enabled(value & 0x10 != 0);
    }

    /// The `$4015` write minus the DMC-enable side effect. The bus
    /// applies the DMC enable/disable at the write's exact CPU cycle
    /// (via [`Apu2A03::dmc_set_enabled`]) so the DMA engine's load
    /// scheduling and §Bugs stop-timing see the true write cycle
    /// rather than the executing instruction's first cycle.
    #[doc(hidden)]
    pub fn write_status_except_dmc_enable(&mut self, value: u8) {
        self.dmc.irq_flag = false;
        self.pulse1.enabled = value & 0x01 != 0;
        self.pulse2.enabled = value & 0x02 != 0;
        self.triangle.enabled = value & 0x04 != 0;
        self.noise.enabled = value & 0x08 != 0;
        self.pulse1.length.silence_if_disabled(self.pulse1.enabled);
        self.pulse2.length.silence_if_disabled(self.pulse2.enabled);
        self.triangle
            .length
            .silence_if_disabled(self.triangle.enabled);
        self.noise.length.silence_if_disabled(self.noise.enabled);
    }

    /// Apply the `$4015` D4 DMC enable/disable (see
    /// [`Apu2A03::write_status_except_dmc_enable`]).
    #[doc(hidden)]
    pub fn dmc_set_enabled(&mut self, on: bool) {
        self.dmc.enable(on);
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
    ///
    /// The mode + inhibit register bits apply immediately, but the
    /// sequence-timer reset — and, when the mode flag is set, the
    /// accompanying quarter- + half-frame clock — is deferred by 3 or
    /// 4 CPU cycles depending on the write's CPU/APU phase, per the
    /// §"Side effects" block of
    /// `docs/audio/nsf/apu-frame-counter-wiki.html` (see
    /// [`Apu2A03::frame_reset_delay`]). A write on the second (odd)
    /// half of an APU cycle takes effect 3 CPU cycles later; a write
    /// on the first (even) half takes 4 — both land on the same APU
    /// phase. Until the reset lands, the old sequence keeps running.
    pub fn write_frame_counter(&mut self, value: u8) {
        self.five_step = value & 0x80 != 0;
        self.frame_irq_inhibit = value & 0x40 != 0;
        if self.frame_irq_inhibit {
            // Spec §$4017: "If set, the frame interrupt flag is
            // cleared, otherwise it is unaffected."
            self.frame_irq_flag = false;
        }
        self.frame_reset_delay = Some(if self.total_cycles & 1 == 1 { 3 } else { 4 });
    }

    /// Advance every channel timer + the frame counter by `cycles`
    /// CPU clocks.
    ///
    /// The batch is split at every scheduled frame-counter event so a
    /// quarter-/half-frame clock lands exactly *between* the channel
    /// timer cycles that surround its documented CPU offset, no matter
    /// how coarsely the CPU batches its cycles. Without the split, a
    /// whole instruction's cycles would tick the channel timers first
    /// and only then fire the frame event — observably wrong wherever
    /// the two interact (a sweep's period rewrite lands relative to the
    /// pulse timer's reload; a linear-counter expiry freezes the
    /// triangle sequencer mid-batch).
    pub fn tick_cpu_cycles(&mut self, cycles: u32) {
        let mut remaining = cycles;
        while remaining > 0 {
            let mut n = self.cycles_until_next_frame_event().min(remaining);
            if let Some(delay) = self.frame_reset_delay {
                n = n.min(delay.max(1));
            }
            self.advance_channel_timers(n);
            self.total_cycles += n as u64;
            self.frame_acc += n;
            if let Some(delay) = self.frame_reset_delay {
                let left = delay.saturating_sub(n);
                if left == 0 {
                    // Deferred $4017 side effects land now: the
                    // sequence timer resets, and in 5-step mode both
                    // the quarter- and half-frame units are clocked
                    // ("If the mode flag is set, then both 'quarter
                    // frame' and 'half frame' signals are also
                    // generated").
                    self.frame_reset_delay = None;
                    self.frame_acc = 0;
                    self.frame_step = 0;
                    if self.five_step {
                        self.tick_quarter_frame();
                        self.tick_half_frame();
                    }
                } else {
                    self.frame_reset_delay = Some(left);
                }
            }
            self.process_due_frame_events();
            remaining -= n;
        }
    }

    /// Tick the per-channel timers (no frame-counter interaction).
    fn advance_channel_timers(&mut self, cycles: u32) {
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
    }

    /// CPU cycles until the next scheduled frame-counter event (at
    /// least 1, so the tick loop always makes progress).
    fn cycles_until_next_frame_event(&self) -> u32 {
        let schedule = self.frame_events();
        let idx = (self.frame_step as usize).min(schedule.len() - 1);
        schedule[idx].offset.saturating_sub(self.frame_acc).max(1)
    }

    /// Fire every frame-sequence event whose offset has been reached.
    ///
    /// The event positions come straight from
    /// docs/audio/nsf/apu-frame-counter-wiki.html §"Mode 0"/"Mode 1":
    /// the quarter/half signals land on the PUT half of the listed
    /// APU cycle (CPU offset = 2×APU + 1, the doc's "additional delay
    /// of one CPU cycle for the quarter and half frame signals"), the
    /// 4-step frame-interrupt flag is set on THREE consecutive CPU
    /// cycles (the step-4 GET, its PUT, and the step-0 GET of the
    /// wrap), and the sequence period is exactly the documented 29830
    /// (NTSC) / 33254 (PAL) CPU cycles. `frame_acc` counts CPU cycles
    /// since the last sequence reset; each scheduled offset fires
    /// once (`frame_step` indexes the next unfired event).
    fn process_due_frame_events(&mut self) {
        loop {
            let schedule = self.frame_events();
            if (self.frame_step as usize) >= schedule.len() {
                // Defensive: a region flip mid-sequence can leave the
                // step index past the end of the new table.
                self.frame_step = 0;
                self.frame_acc = 0;
            }
            let ev = schedule[self.frame_step as usize];
            if self.frame_acc < ev.offset {
                break;
            }
            if ev.quarter {
                self.tick_quarter_frame();
            }
            if ev.half {
                self.tick_half_frame();
            }
            if ev.irq && !self.frame_irq_inhibit {
                // Only the 4-step sequence carries IRQ events; the
                // tables encode none for 5-step ("In this mode, the
                // frame interrupt flag is never set").
                self.frame_irq_flag = true;
            }
            if (self.frame_step as usize) == schedule.len() - 1 {
                // Last event doubles as the period boundary: "Once the
                // last step has executed, the count resets to 0 on the
                // next APU cycle."
                self.frame_acc -= ev.offset;
                self.frame_step = 0;
            } else {
                self.frame_step += 1;
            }
        }
    }

    /// Per-mode/region frame-sequence event tables, in CPU cycles from
    /// the sequence start, transcribed from the Mode 0 / Mode 1 tables
    /// of `docs/audio/nsf/apu-frame-counter-wiki.html`. Quarter/half
    /// signals fire on the PUT half of their APU cycle (CPU = 2×APU+1);
    /// the 4-step interrupt flag is set at the step-4 GET (2×APU), the
    /// step-4 PUT, and the wrap GET (= the period). The final entry's
    /// offset is the sequence period. Mode 1 (5-step) clocks nothing at
    /// its 4th step (the table row is blank) and never sets the flag.
    fn frame_events(&self) -> &'static [FrameEvent] {
        const fn ev(offset: u32, quarter: bool, half: bool, irq: bool) -> FrameEvent {
            FrameEvent {
                offset,
                quarter,
                half,
                irq,
            }
        }
        // 4-step NTSC: APU 3728/7456/11185/14914, wrap 14915.
        const NTSC_4: &[FrameEvent] = &[
            ev(7457, true, false, false),
            ev(14913, true, true, false),
            ev(22371, true, false, false),
            ev(29828, false, false, true),
            ev(29829, true, true, true),
            ev(29830, false, false, true),
        ];
        // 4-step PAL: APU 4156/8313/12469/16626, wrap 16627.
        const PAL_4: &[FrameEvent] = &[
            ev(8313, true, false, false),
            ev(16627, true, true, false),
            ev(24939, true, false, false),
            ev(33252, false, false, true),
            ev(33253, true, true, true),
            ev(33254, false, false, true),
        ];
        // 5-step NTSC: APU 3728/7456/11185/14914 (blank)/18640, wrap
        // 18641. Step 4 (APU 14914) clocks neither unit, so it is not
        // listed.
        const NTSC_5: &[FrameEvent] = &[
            ev(7457, true, false, false),
            ev(14913, true, true, false),
            ev(22371, true, false, false),
            ev(37281, true, true, false),
            ev(37282, false, false, false),
        ];
        // 5-step PAL: APU 4156/8313/12469/16626 (blank)/20782, wrap
        // 20783.
        const PAL_5: &[FrameEvent] = &[
            ev(8313, true, false, false),
            ev(16627, true, true, false),
            ev(24939, true, false, false),
            ev(41565, true, true, false),
            ev(41566, false, false, false),
        ];
        match (self.five_step, self.pal) {
            (false, false) => NTSC_4,
            (false, true) => PAL_4,
            (true, false) => NTSC_5,
            (true, true) => PAL_5,
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
    fn pulse_bottom_octave_mutes_with_sweep_unconfigured() {
        // apu-sweep-wiki §Muting: "Muting happens regardless of whether
        // the sweep unit is disabled (because either the Enabled flag
        // or the Shift count are zero)". With negate false and shift 0
        // the change amount equals the current period, so any period
        // ≥ $400 targets > $7FF and mutes — "why several publishers'
        // NES games never seem to use the bottom octave of the pulse
        // waves".
        let mut p = PulseChannel::new(false);
        p.enabled = true;
        p.write_main(0xBF); // duty 50%, halt, constant vol 15
        p.write_period_lo(0x00);
        p.write_period_hi(0x05); // timer_period = 0x500, length loaded
        p.length.counter = 10;
        // Sweep never configured → E=0, shift 0, negate false.
        assert!(p.sweep_mutes(), "period ≥ $400 must mute with negate false");
        for _ in 0..0x4000 {
            p.tick_timer(1);
            assert_eq!(p.output(), 0, "muted channel must send 0 to the mixer");
        }
        // Just below the boundary the target (2 × period) stays ≤ $7FF
        // and the note plays.
        p.write_period_lo(0xFF);
        p.write_period_hi(0x03); // timer_period = 0x3FF → target 0x7FE
        p.length.counter = 10;
        assert!(!p.sweep_mutes(), "period $3FF must not mute");
        let mut saw = false;
        for _ in 0..0x4000 {
            p.tick_timer(1);
            if p.output() > 0 {
                saw = true;
                break;
            }
        }
        assert!(saw, "period-$3FF pulse produced no output");
    }

    #[test]
    fn pulse_write_08_fully_disables_sweep_muting() {
        // apu-sweep-wiki §Muting: "to fully disable the sweep unit, a
        // program must additionally turn on the Negate flag, such as by
        // writing $08. This ensures that the target period is not
        // greater than the current period and therefore not greater
        // than $7FF." The bottom octave becomes playable again.
        let mut p = PulseChannel::new(false);
        p.enabled = true;
        p.write_main(0xBF);
        p.write_period_lo(0x00);
        p.write_period_hi(0x05); // timer_period = 0x500
        p.length.counter = 10;
        assert!(p.sweep_mutes(), "sanity: mutes before the $08 write");
        p.write_sweep(0x08); // negate on, E=0, shift 0
        assert!(!p.sweep_mutes(), "$08 write must lift the adder mute");
        let mut saw = false;
        for _ in 0..0x8000 {
            p.tick_timer(1);
            if p.output() > 0 {
                saw = true;
                break;
            }
        }
        assert!(saw, "negate-disabled low pulse produced no output");
    }

    #[test]
    fn pulse_negative_target_clamps_to_zero() {
        // apu-sweep-wiki §"Calculating the target period": the target
        // is "clamped to zero if this sum is negative". Pulse 1 with
        // period 20, negate, shift 0 computes 20 + (−21) = −1 → 0, not
        // a wrapped huge value (which would falsely trip the > $7FF
        // mute).
        let mut p = PulseChannel::new(false);
        p.write_period_lo(20);
        p.write_sweep(0x08); // negate, shift 0
        assert_eq!(p.target_period(), 0);
        assert!(!p.sweep_mutes(), "clamped target must not mute");
    }

    #[test]
    fn pulse_negate_carry_differs_between_channels() {
        // apu-sweep-wiki §"Calculating the target period": "Pulse 1
        // adds the ones' complement (−c − 1). Making 20 negative
        // produces a change amount of −21. Pulse 2 adds the two's
        // complement (−c). Making 20 negative produces a change amount
        // of −20." Period 40, shift 1 → change 20.
        let mut p1 = PulseChannel::new(false);
        p1.write_period_lo(40);
        p1.write_sweep(0x09); // negate, shift 1
        assert_eq!(p1.target_period(), 40 - 21);
        let mut p2 = PulseChannel::new(true);
        p2.write_period_lo(40);
        p2.write_sweep(0x09);
        assert_eq!(p2.target_period(), 40 - 20);
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
    fn sweep_divider_keeps_counting_while_muting() {
        // apu-sweep-wiki §"Updating the period": while the sweep unit
        // is muting "the pulse's period remains unchanged, but the
        // sweep unit's divider continues to count down and reload the
        // divider's period as normal".
        let mut p = PulseChannel::new(false);
        p.enabled = true;
        p.length.counter = 10;
        p.write_period_lo(0x00);
        p.write_period_hi(0x06); // timer_period = 0x600
        p.write_sweep(0xA1); // E=1, P=2, add mode, shift 1 → target > $7FF
        assert!(p.sweep_mutes());
        // First clock consumes the reload; divider = P.
        p.clock_sweep();
        assert_eq!(p.sweep_divider, 2);
        for expected in [1u8, 0] {
            p.clock_sweep();
            assert_eq!(p.sweep_divider, expected, "divider must keep counting");
        }
        // Divider hit zero while muting: period unchanged, divider
        // reloads on the next clock.
        p.clock_sweep();
        assert_eq!(p.timer_period, 0x600, "muting must block the period update");
        assert_eq!(p.sweep_divider, 2, "divider must reload as normal");
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
    fn frame_clocked_units_are_chunk_invariant() {
        // The whole APU must evolve identically whether the CPU feeds
        // its cycles one at a time or in giant blocks: the tick loop
        // splits each batch at every scheduled frame-counter event, so
        // a sweep's period rewrite / a linear-counter expiry lands
        // exactly between the channel-timer cycles that surround its
        // documented offset. Program a sweeping pulse and a triangle
        // with a short linear counter, run two APUs over the same
        // 100 000 CPU cycles (one bulk call vs single-cycle calls), and
        // require identical channel state.
        let setup = |apu: &mut Apu2A03| {
            apu.write_frame_counter(0x00);
            apu.write_status(0x05); // pulse1 + triangle
            apu.write_register(0x4000, 0x3F); // halt, const vol 15
            apu.write_register(0x4001, 0x82); // sweep on, period 0, shift 2
            apu.write_register(0x4002, 0x00);
            apu.write_register(0x4003, 0x02); // pulse period 0x200
            apu.write_register(0x4008, 0x05); // linear reload 5, control clear
            apu.write_register(0x400A, 0x21);
            apu.write_register(0x400B, 0x08); // tri period 0x21, arm reload
        };
        let mut bulk = Apu2A03::new();
        setup(&mut bulk);
        bulk.tick_cpu_cycles(100_000);
        let mut drip = Apu2A03::new();
        setup(&mut drip);
        for _ in 0..100_000 {
            drip.tick_cpu_cycles(1);
        }
        assert_eq!(bulk.pulse1.timer_period, drip.pulse1.timer_period);
        assert_eq!(bulk.pulse1.timer, drip.pulse1.timer);
        assert_eq!(bulk.pulse1.duty_step, drip.pulse1.duty_step);
        assert_eq!(bulk.triangle.seq_step, drip.triangle.seq_step);
        assert_eq!(bulk.triangle.linear_counter, drip.triangle.linear_counter);
        assert_eq!(bulk.frame_acc, drip.frame_acc);
        assert_eq!(bulk.frame_step, drip.frame_step);
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
        // A period < 2 sweeps faster than the output resolves while
        // the sequencer is cycling; the spec gives the averaged level
        // as 7.5. output_halfsteps must report 15 (= 7.5) rather than
        // 0 — but only while the sequencer actually runs (counters
        // non-zero); a halted channel holds its step instead.
        let mut tri = TriangleChannel {
            enabled: true,
            ..TriangleChannel::default()
        };
        // Period = 1 → ultrasonic. Counters at zero → halted: holds
        // the power-up step (15, output 0), NOT the midpoint.
        tri.write_period_lo(0x01);
        assert_eq!(tri.output_halfsteps(), 0, "halted channel holds");
        // Cycling + ultrasonic → the documented 7.5 average.
        tri.length.counter = 4;
        tri.linear_counter = 4;
        assert_eq!(tri.output_halfsteps(), 15, "cycling ultrasonic = 7.5");
    }

    #[test]
    fn triangle_4015_disable_holds_position_not_zero() {
        // "Use $4015 to turn off the channel, which will clear its
        // length counter" is one of the documented silencing methods
        // that "halt it in whatever its current output position is" —
        // and "Silencing the triangle channel merely halts it. It will
        // continue to output its last value rather than 0." Advance
        // the sequencer to a mid-wave step, disable via the $4015
        // path, and require the held (non-zero) level to persist.
        let mut tri = TriangleChannel {
            enabled: true,
            ..TriangleChannel::default()
        };
        tri.write_period_lo(0x10);
        tri.length.counter = 100;
        tri.linear_counter = 100;
        tri.tick_timer(17 * 5); // a few sequencer steps past power-up
        let held = tri.output_halfsteps();
        assert_ne!(held, 0, "mid-wave step should be non-zero");
        // $4015 disable: enable flag clears + length counter zeroed.
        tri.enabled = false;
        tri.length.silence_if_disabled(false);
        tri.tick_timer(17 * 50);
        assert_eq!(
            tri.output_halfsteps(),
            held,
            "disabled triangle must hold its last output, not snap to 0"
        );
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
        assert_eq!(pending, Some((0xC000, DMC_DMA_LOAD_STALL_CYCLES)));
        apu.dmc_supply_byte(0xAB);
        // Address advances; bytes remaining decrements.
        assert_eq!(apu.dmc.current_addr, 0xC001);
        assert_eq!(apu.dmc.bytes_remaining, 16);
        assert!(apu.dmc.sample_buffer_filled);
        assert!(apu.dmc_pending_fetch().is_none());
    }

    #[test]
    fn dmc_dma_load_then_reload_stall_cycles() {
        // apu-dma-wiki §"DMC DMA": "load DMAs take 3 cycles and reload
        // DMAs take 4" — the post-$4015 fetch halts on a get cycle
        // (halt + dummy + read), while a buffer-emptied refetch halts
        // on a put cycle and needs the extra alignment cycle.
        let mut apu = Apu2A03::new();
        apu.write_register(0x4010, 0x4F); // loop, fastest rate (54 cy/bit)
        apu.write_register(0x4012, 0x00); // address = $C000
        apu.write_register(0x4013, 0x00); // 1-byte sample
        apu.write_status(0x10); // enable DMC → the load DMA
        apu.tick_cpu_cycles(1);
        let (addr, stall) = apu.dmc_pending_fetch().expect("load fetch armed");
        assert_eq!(addr, 0xC000);
        assert_eq!(
            stall, DMC_DMA_LOAD_STALL_CYCLES,
            "post-$4015 fetch is a load"
        );
        apu.dmc_supply_byte(0xAA);
        // Run a full 8-bit output cycle so the buffer drains into the
        // shift register; the looping sample then arms a refetch.
        apu.tick_cpu_cycles(54 * 8 + 16);
        let (_, stall2) = apu.dmc_pending_fetch().expect("reload fetch armed");
        assert_eq!(
            stall2, DMC_DMA_RELOAD_STALL_CYCLES,
            "buffer-emptied refetch is a reload"
        );
    }

    #[test]
    fn dmc_disable_cancels_pending_fetch() {
        // §"Memory reader": fetches require "bytes remaining is not
        // zero", and a $4015 D4 clear zeroes it — the hardware DMA
        // scheduled at that point aborts without reading (apu-dma-wiki
        // §Bugs). A stale armed fetch must not survive the disable and
        // fill the sample buffer.
        let mut apu = Apu2A03::new();
        apu.write_register(0x4010, 0x0F);
        apu.write_register(0x4012, 0x00);
        apu.write_register(0x4013, 0x00);
        apu.write_status(0x10);
        apu.tick_cpu_cycles(1);
        assert!(apu.dmc_pending_fetch().is_some(), "fetch armed");
        apu.write_status(0x00); // explicit stop
        assert!(
            apu.dmc_pending_fetch().is_none(),
            "disable must cancel the armed fetch"
        );
        assert!(
            !apu.dmc.sample_buffer_filled,
            "no byte may reach the buffer from the aborted DMA"
        );
        // Re-enabling restarts the sample from its seed with a fresh
        // load DMA.
        apu.write_status(0x10);
        apu.tick_cpu_cycles(1);
        let (addr, stall) = apu.dmc_pending_fetch().expect("fresh load armed");
        assert_eq!(addr, 0xC000);
        assert_eq!(stall, DMC_DMA_LOAD_STALL_CYCLES);
    }

    #[test]
    fn dmc_reenable_after_sample_end_is_a_load_dma() {
        // A non-looping sample ends; re-setting $4015 D4 restarts it,
        // and that restart fetch is again a 3-cycle load DMA per
        // apu-dma-wiki §"DMC DMA" ("Load DMAs occur after $4015 D4 is
        // set, but only if the sample buffer is empty").
        let mut apu = Apu2A03::new();
        apu.write_register(0x4010, 0x0F); // no loop, fastest rate
        apu.write_register(0x4012, 0x00);
        apu.write_register(0x4013, 0x00); // 1-byte sample
        apu.write_status(0x10);
        apu.tick_cpu_cycles(1);
        let (_, stall) = apu.dmc_pending_fetch().expect("first load armed");
        assert_eq!(stall, DMC_DMA_LOAD_STALL_CYCLES);
        apu.dmc_supply_byte(0x55);
        // Drain the byte fully (8 bits + the following empty cycle).
        apu.tick_cpu_cycles(54 * 16 + 32);
        assert!(
            apu.dmc_pending_fetch().is_none(),
            "sample ended, no refetch"
        );
        apu.write_status(0x10); // restart
        apu.tick_cpu_cycles(1);
        let (_, stall2) = apu.dmc_pending_fetch().expect("restart load armed");
        assert_eq!(stall2, DMC_DMA_LOAD_STALL_CYCLES, "restart fetch is a load");
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
    fn dmc_output_unit_powers_up_silent() {
        // §"Output unit": the sample buffer is empty at power-up, so
        // the silence flag starts set and a $4011 direct-load level
        // must hold rock-steady while no sample is playing — no delta
        // step may be applied from the never-loaded shift register.
        let mut apu = Apu2A03::new();
        apu.write_register(0x4010, 0x0F); // fastest rate
        apu.write_register(0x4011, 0x40); // direct-load level 64
        for _ in 0..10_000 {
            apu.tick_cpu_cycles(1);
        }
        assert_eq!(
            apu.dmc.dac, 0x40,
            "$4011 level must not drift while the DMC is silent"
        );
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
        // 4-step NTSC mode the frame interrupt flag is first set at the
        // final step (APU 14914 GET → CPU 29828 from the sequence
        // start) and the whole sequence period is 29830 CPU cycles.
        // The $4017 write itself takes effect 4 CPU cycles later (an
        // even-phase write, §"Side effects"), so from the write the
        // first assertion lands at 4 + 29828. Feed cycles one at a
        // time so the assertion offset is exact.
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
        assert_eq!(
            fired_at,
            Some(4 + 29_828),
            "4-step IRQ flag at CPU 29828 after the delayed $4017 reset"
        );

        // §"Mode 0": the flag is set at THREE consecutive points — the
        // step-4 GET (29828), its PUT (29829), and the wrap GET
        // (29830). Acknowledging right after the first set point must
        // therefore see the flag re-assert one cycle later, and again
        // one cycle after that.
        apu.read_status();
        apu.tick_cpu_cycles(1); // sequence CPU 29829
        assert!(apu.frame_irq_flag, "flag re-set at step-4 PUT (29829)");
        apu.read_status();
        apu.tick_cpu_cycles(1); // sequence CPU 29830 (= wrap)
        assert!(apu.frame_irq_flag, "flag re-set at wrap GET (29830)");

        // After the final set point, the next assertion is a full
        // sequence later: 29828 cycles from the wrap.
        apu.read_status();
        let mut next_at = None;
        for c in 1..=40_000u32 {
            apu.tick_cpu_cycles(1);
            if apu.frame_irq_flag {
                next_at = Some(c);
                break;
            }
        }
        assert_eq!(next_at, Some(29_828), "next set point 29828 after wrap");
    }

    #[test]
    fn frame_counter_quarter_signal_lands_on_put_cycle() {
        // §"Mode 0" + the intro note "with an additional delay of one
        // CPU cycle for the quarter and half frame signals": the first
        // quarter-frame clock of the 4-step NTSC sequence lands at APU
        // 3728, PUT half → CPU cycle 2×3728 + 1 = 7457 from the
        // sequence start (which is 4 cycles after the even-phase $4017
        // write). Observe it via the envelope decay level: after a
        // $4003 write arms the start flag, the first quarter-frame
        // clock reloads decay to 15.
        let mut apu = Apu2A03::new();
        apu.write_frame_counter(0x00);
        apu.write_status(0x01);
        apu.write_register(0x4000, 0x00); // envelope mode, period 0
        apu.write_register(0x4003, 0x08); // arms envelope start
        apu.tick_cpu_cycles(4 + 7_456);
        assert_eq!(apu.pulse1.envelope.decay, 0, "no quarter clock yet");
        apu.tick_cpu_cycles(1); // sequence CPU 7457 = PUT of APU 3728
        assert_eq!(apu.pulse1.envelope.decay, 15, "quarter clock at 7457");
    }

    #[test]
    fn frame_counter_4017_reset_delay_depends_on_write_phase() {
        // §"Side effects": "After 3 or 4 CPU clock cycles*, the timer
        // is reset. If the mode flag is set, then both 'quarter frame'
        // and 'half frame' signals are also generated." — 3 cycles for
        // a write on the second (odd) half of an APU cycle, 4 for a
        // write on the first (even) half. Observe the deferred 5-step
        // quarter clock through the envelope start flag.
        //
        // Even-phase write: effects after 4 CPU cycles.
        let mut apu = Apu2A03::new();
        apu.write_status(0x01);
        apu.write_register(0x4000, 0x00);
        apu.write_register(0x4003, 0x08); // arm envelope start
        apu.write_frame_counter(0x80); // total_cycles = 0 (even)
        apu.tick_cpu_cycles(3);
        assert_eq!(apu.pulse1.envelope.decay, 0, "no clock 3 cycles in");
        apu.tick_cpu_cycles(1);
        assert_eq!(apu.pulse1.envelope.decay, 15, "clock on the 4th cycle");

        // Odd-phase write: effects after 3 CPU cycles.
        let mut apu = Apu2A03::new();
        apu.write_status(0x01);
        apu.write_register(0x4000, 0x00);
        apu.write_register(0x4003, 0x08);
        apu.tick_cpu_cycles(1); // total_cycles = 1 (odd)
        apu.write_frame_counter(0x80);
        apu.tick_cpu_cycles(2);
        assert_eq!(apu.pulse1.envelope.decay, 0, "no clock 2 cycles in");
        apu.tick_cpu_cycles(1);
        assert_eq!(apu.pulse1.envelope.decay, 15, "clock on the 3rd cycle");
    }

    #[test]
    fn frame_counter_4017_bit7_clear_does_not_clock_units() {
        // §"$4017" (registers page): "with bit 7 clear, only the
        // sequence is reset without clocking any of its units."
        let mut apu = Apu2A03::new();
        apu.write_status(0x01);
        apu.write_register(0x4000, 0x00);
        apu.write_register(0x4003, 0x08); // arm envelope start, length 254
        let len = apu.pulse1.length.counter;
        apu.write_frame_counter(0x00);
        apu.tick_cpu_cycles(8); // reset landed, no clocks
        assert_eq!(apu.pulse1.envelope.decay, 0, "no quarter clock");
        assert_eq!(apu.pulse1.length.counter, len, "no half clock");
    }

    #[test]
    fn five_step_clocks_nothing_at_fourth_step_and_both_at_fifth() {
        // §"Mode 1" table: step 4 (APU 14914 → CPU 29829) clocks
        // neither unit; step 5 (APU 18640 → CPU 37281) clocks BOTH the
        // quarter- and half-frame units. Track the envelope decay level
        // and the length counter. All offsets below are relative to the
        // sequence start, which lands 4 CPU cycles after the
        // (even-phase) $4017 write together with the write's own
        // quarter+half clock.
        let mut apu = Apu2A03::new();
        apu.write_status(0x01);
        apu.write_register(0x4000, 0x00); // envelope mode, halt clear
        apu.write_register(0x4003, 0x08); // length idx 1 → 254, env start
        apu.write_frame_counter(0x80); // 5-step
        apu.tick_cpu_cycles(4); // deferred reset + Q+H clock land
        let len0 = apu.pulse1.length.counter;
        let decay0 = apu.pulse1.envelope.decay;
        assert_eq!(decay0, 15, "write's own quarter clock consumed start");
        // Run to just past step 4 (sequence CPU 29829): quarter clocks
        // fired at 7457/14913/22371 only — step 4 contributes nothing.
        apu.tick_cpu_cycles(29_900);
        assert_eq!(
            apu.pulse1.envelope.decay,
            decay0 - 3,
            "exactly 3 quarter clocks through CPU 29900 (step 4 is blank)"
        );
        assert_eq!(
            apu.pulse1.length.counter,
            len0 - 1,
            "one half clock (step 2) through CPU 29900"
        );
        // Step 5 (sequence CPU 37281) clocks both.
        apu.tick_cpu_cycles(37_281 - 29_900);
        assert_eq!(
            apu.pulse1.envelope.decay,
            decay0 - 4,
            "step 5 issues the 4th quarter clock"
        );
        assert_eq!(
            apu.pulse1.length.counter,
            len0 - 2,
            "step 5 issues the 2nd half clock"
        );
    }

    #[test]
    fn frame_counter_pal_period_is_documented() {
        // PAL 4-step period is 33254 CPU cycles (APU 16627 × 2); the
        // first IRQ set point is the step-4 GET at CPU 33252 from the
        // sequence start, which begins 4 cycles after the (even-phase)
        // $4017 write.
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
        assert_eq!(
            fired_at,
            Some(4 + 33_252),
            "PAL 4-step IRQ flag at CPU 33252 after the delayed reset"
        );
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
