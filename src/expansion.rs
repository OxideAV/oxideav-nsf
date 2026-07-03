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
    /// `$9003` frequency-scaling field (bits 2-1, `AB`). Per
    /// `docs/audio/nsf/vrc6-audio-wiki.html` §"Frequency Control
    /// ($9003)": B = "16x frequency, all oscillators (4 octave
    /// increase)", A = "256x frequency, all oscillators (8 octave
    /// increase)", and "The 256x flag overrides the 16x flag." The
    /// flags "effectively control a 4-bit and 8-bit right shift of
    /// the 12-bit period registers" — see [`Vrc6::period_shift`].
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
    /// 16-step duty generator, counting **down** from 15 to 0 per
    /// `docs/audio/nsf/vrc6-audio-wiki.html` §"Pulse Channels": "The
    /// duty cycle generator takes 16 steps, counting down from 15 to
    /// 0. When the current step is less than or equal to the given
    /// duty cycle D, the channel volume V is output, otherwise 0 is
    /// output." A freshly reset / re-enabled generator starts at the
    /// top of the countdown (15).
    pub step: u8,
}

impl Vrc6Pulse {
    /// Apply a `$x002` E-bit (enable) write, honouring the §"Pulse
    /// Channels" reset semantics: "When the channel is disabled by
    /// clearing the E bit, output is forced to 0, and the duty cycle
    /// is immediately reset and halted; it will resume from the
    /// beginning when E is once again set." The duty generator's
    /// "beginning" is the top of the 15→0 countdown, so a falling
    /// edge on E pins `step` at 15 and the timer is reloaded so the
    /// generator resumes a full step from a clean phase when E is set
    /// again.
    fn set_enabled(&mut self, now_enabled: bool) {
        if !now_enabled && self.enabled {
            self.step = 15;
            self.timer = self.timer_period;
        }
        self.enabled = now_enabled;
    }
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
        let mut chip = Self::default();
        // The duty generator counts down from 15 to 0
        // (§"Pulse Channels"); seed both pulses at the top of the
        // countdown so the very first enabled cycle has a clean phase.
        chip.pulse[0].step = 15;
        chip.pulse[1].step = 15;
        chip
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
                self.pulse[0].set_enabled(value & 0x80 != 0);
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
                self.pulse[1].set_enabled(value & 0x80 != 0);
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
                let now_enabled = value & 0x80 != 0;
                // §Sawtooth Channel: "If E is clear, the accumulator is
                // forced to zero until E is again set. The phase of the
                // saw generator can be mostly reset by clearing and
                // immediately setting E. Clearing E does not reset the
                // frequency divider, however, so the first step of the
                // reset saw may appear shortened."
                //
                // A falling edge on E forces accum=0 + resets the
                // 14-step phase counter; the timer divider is preserved.
                if !now_enabled && self.saw.enabled {
                    self.saw.accum = 0;
                    self.saw.step = 0;
                }
                self.saw.enabled = now_enabled;
            }
            _ => {}
        }
    }

    /// How far the 12-bit period registers are right-shifted by the
    /// active `$9003` frequency-scaling flags. Per §"Frequency Control
    /// ($9003)": "The 16x/256x flags effectively control a 4-bit and
    /// 8-bit right shift of the 12-bit period registers", with "The
    /// 256x flag overrides the 16x flag."
    #[inline]
    fn period_shift(&self) -> u8 {
        // freq_shift holds `$9003` bits 2-1 (`AB`): bit 0 of the field
        // = B (16x → 4-bit shift), bit 1 = A (256x → 8-bit shift,
        // overriding B).
        if self.freq_shift & 0b10 != 0 {
            8
        } else if self.freq_shift & 0b01 != 0 {
            4
        } else {
            0
        }
    }

    /// Advance the chip by `cycles` CPU cycles, one cycle at a time.
    ///
    /// §"Pulse Channels": "The CPU clock rate (1.79 MHz) drives the
    /// 12-bit divider F. Every cycle the divider counts down until it
    /// reaches zero, at which point the divider resets and the duty
    /// cycle generator is clocked." The sawtooth divider works the
    /// same way (§"Sawtooth Channel"). Each divider is walked
    /// cycle-by-cycle so the generators stay phase-exact no matter how
    /// the CPU batches its cycles, and the `$9003` 16x/256x flags are
    /// applied as the documented right shift of the *reloaded* period
    /// — the divider itself always counts at the full CPU rate.
    pub fn tick(&mut self, cycles: u32) {
        if self.halt {
            // §"Frequency Control ($9003)": "H - halts all
            // oscillators, stopping them in their current state" and
            // "The halt flag overrides the other flags."
            return;
        }
        let shift = self.period_shift();
        for _ in 0..cycles {
            for p in &mut self.pulse {
                if !p.enabled {
                    continue;
                }
                if p.timer == 0 {
                    p.timer = p.timer_period >> shift;
                    // §"Pulse Channels": the duty generator counts
                    // down from 15 to 0, wrapping back to 15.
                    p.step = if p.step == 0 { 15 } else { p.step - 1 };
                } else {
                    p.timer -= 1;
                }
            }
            if self.saw.enabled {
                // §Sawtooth Channel: 14-step internal cycle. Each
                // timer expiry advances `step` by 1 modulo 14:
                //   * even-step positions 2, 4, 6, 8, 10, 12 each add
                //     the 6-bit rate value A to the 8-bit accumulator
                //     ("when clocked, the rate value A is added to an
                //     internal 8-bit accumulator"),
                //   * odd-step positions 1, 3, 5, 7, 9, 11, 13 are
                //     no-ops ("the accumulator only reacts on every 2
                //     clocks"),
                //   * step 0 (reached on the 14th clock from the
                //     previous step 0) resets the accumulator to zero
                //     ("after A has been added 6 times, on the 7th
                //     clock, instead of A being added, the internal
                //     accumulator is reset to zero").
                //
                // The walked example in the wiki (A=$08) reads
                //   step 0 →$00, 2→$08, 4→$10, 6→$18, 8→$20, 10→$28,
                //   12→$30, then back to step 0 → $00.
                if self.saw.timer == 0 {
                    self.saw.timer = self.saw.timer_period >> shift;
                    self.saw.step = (self.saw.step + 1) % 14;
                    if self.saw.step == 0 {
                        self.saw.accum = 0;
                    } else if self.saw.step & 0x01 == 0 {
                        self.saw.accum = self.saw.accum.wrapping_add(self.saw.rate);
                    }
                } else {
                    self.saw.timer -= 1;
                }
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
            // §"Pulse Channels": "When the current step is less than
            // or equal to the given duty cycle D, the channel volume V
            // is output, otherwise 0 is output. When the mode bit M is
            // true, the channel ignores the duty cycle generator and
            // outputs the current volume regardless of the current
            // duty."
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

/// CPU-cycle period of the MMC5's fixed 240 Hz envelope / length clock.
///
/// Per `docs/audio/nsf/mmc5-audio-wiki.html` §"Pulse 1 ($5000-$5003)":
/// "MMC5 does not have an equivalent frame sequencer (APU $4017);
/// envelope and length counter are fixed to a 240hz update rate." The
/// chip free-runs this clock — there is no 4-step / 5-step mode and no
/// `$4017` analogue. We reuse the same `7457`-cycle quarter-frame
/// period the 2A03 frame counter uses for its own ≈240 Hz quarter-frame
/// events, so the two share one cadence.
const MMC5_FRAME_CPU: u32 = 7457;

/// MMC5 audio has 2 pulses (almost identical to 2A03 pulses but no
/// sweep) at `$5000..=$5007` plus a raw 8-bit PCM channel at `$5011`
/// and a status register at `$5015`.
///
/// Round 18 wires the `$5010` PCM Mode / IRQ register per
/// `docs/audio/nsf/mmc5-audio-wiki.html` §"PCM Mode/IRQ ($5010)" +
/// §"PCM description" + §"IRQ operation":
///
/// * `$5010` write: bit 7 = PCM IRQ enable, bit 0 = mode select
///   (0 = write mode, 1 = read mode).
/// * `$5010` read: bit 7 = (irq_trip AND irq_enable); reading
///   acknowledges and clears irq_trip. Bit 0 mirrors the configured
///   mode bit (the wiki notes only the MMC5A revision implements a
///   `$5010.0` read bit and its function is undocumented; the
///   `MMC5A default power-on read value = $01` note is captured by
///   resetting `pcm_read_mode = false` and OR-ing bit 0 from the
///   current mode write — read mode therefore reads back `1`).
/// * `$5011` write in write mode: if value=0 → DAC unchanged,
///   irq_trip=1; else DAC=value, irq_trip=0. Write in read mode is
///   ignored from the CPU side; the same DAC update path runs from
///   `Mmc5::observe_prg_read` whenever the bus reads `$8000..=$BFFF`
///   while read mode is active (the "Write-by-read writes to this
///   register in PCM read-mode" semantic from §"Raw PCM ($5011)").
/// * `Mmc5::irq_line()` exposes `(irq_trip AND irq_enable)` so the
///   bus can OR it into the CPU's IRQ line alongside the APU
///   frame-counter / DMC / NSF2 timer sources.
#[derive(Default)]
pub struct Mmc5 {
    pub enabled: bool,
    pub pulse: [Mmc5Pulse; 2],
    pub pcm: u8,
    pub pcm_read_mode: bool,
    pub status: u8,
    /// `$5010` bit 7 — PCM IRQ enable.
    pub irq_enable: bool,
    /// Internal "DAC saw a zero-byte write" flag. Set whenever the
    /// active DAC update path (CPU `$5011` write in write mode, or
    /// `$8000..=$BFFF` read in read mode) sees a `$00` byte; cleared
    /// on any non-zero DAC update or a `$5010` read.
    pub irq_trip: bool,
    /// CPU-cycle accumulator for the fixed 240 Hz envelope / length
    /// clock (§"Pulse 1": "envelope and length counter are fixed to a
    /// 240hz update rate"). No frame sequencer exists, so this just
    /// free-runs and fires both the envelope and length steps each
    /// time it crosses `MMC5_FRAME_CPU`.
    pub frame_acc: u32,
    /// Dropped half-cycle of the pulse /2 prescaler, carried across
    /// batches. §"Pulse 1": the channels "behave almost identically
    /// to the native pulse channels in the NES APU", whose 11-bit
    /// timer counts APU (CPU/2) cycles — so an odd CPU batch must
    /// leave its odd cycle for the next batch instead of rounding.
    pub pulse_prescaler_carry: u32,
}

#[derive(Default, Clone, Copy)]
pub struct Mmc5Pulse {
    pub enabled: bool,
    pub duty: u8,
    pub volume: u8,
    /// `$5000`/`$5004` bit 4 — constant-volume vs envelope select. When
    /// set, `output()` uses `volume` directly; when clear, the envelope
    /// decay level is used (§"Pulse 1": "the envelope … [is] the same
    /// as their APU counterparts").
    pub constant: bool,
    pub timer_period: u16,
    pub timer: u16,
    pub step: u8,
    /// Length counter, loaded from the 2A03 `LENGTH_TABLE` on a
    /// `$5003`/`$5007` write and counted down at the fixed 240 Hz clock
    /// — "twice as fast as the APU length counter" per §"Pulse 1".
    pub length: u8,
    /// `$5000`/`$5004` bit 5 — length-counter halt + envelope loop. The
    /// 2A03 shares one bit for both functions; the MMC5 pulse does too
    /// ("the same as their APU counterparts").
    pub halt: bool,
    // ---- Envelope (APU-identical) ----
    /// Envelope start flag, set on every `$5003`/`$5007` write; the
    /// next envelope clock reloads the decay level to 15 and the
    /// divider to `volume`.
    pub env_start: bool,
    /// Current 4-bit envelope decay level (0..=15).
    pub env_decay: u8,
    /// Envelope divider; counts down from `volume`, reloads on 0.
    pub env_divider: u8,
}

const MMC5_DUTY: [[u8; 8]; 4] = [
    [0, 1, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0],
    [1, 0, 0, 1, 1, 1, 1, 1],
];

impl Mmc5Pulse {
    /// Clock the envelope unit once (240 Hz). Identical to the 2A03
    /// envelope (§"Pulse 1": "the envelope … the same as their APU
    /// counterparts"): `env_start` reloads decay=15 + divider=volume;
    /// otherwise the divider counts down and, on reaching 0, reloads to
    /// `volume` and decrements the decay (looping back to 15 when the
    /// halt/loop bit is set).
    fn clock_envelope(&mut self) {
        if self.env_start {
            self.env_start = false;
            self.env_decay = 15;
            self.env_divider = self.volume;
        } else if self.env_divider == 0 {
            self.env_divider = self.volume;
            if self.env_decay > 0 {
                self.env_decay -= 1;
            } else if self.halt {
                self.env_decay = 15;
            }
        } else {
            self.env_divider -= 1;
        }
    }

    /// Clock the length counter once (240 Hz, "twice as fast as the APU
    /// length counter"). The halt bit freezes it; the count never wraps
    /// below 0.
    fn clock_length(&mut self) {
        if !self.halt && self.length > 0 {
            self.length -= 1;
        }
    }

    /// 4-bit volume the channel currently emits: the constant level
    /// (`$500x` bit 4 set) or the envelope decay level (clear).
    fn current_volume(&self) -> u8 {
        if self.constant {
            self.volume
        } else {
            self.env_decay
        }
    }

    /// One channel's contribution before mixing. Silenced by a disabled
    /// channel, an expired length counter, or a low duty step. Note
    /// there is NO `timer_period >= 8` mute (§"Pulse 1": sub-8 periods
    /// emit ultrasonic tones rather than silence).
    fn pulse_output(&self) -> u32 {
        if !self.enabled
            || self.length == 0
            || MMC5_DUTY[self.duty as usize][self.step as usize] == 0
        {
            return 0;
        }
        self.current_volume() as u32
    }
}

/// MMC5 raw-PCM full-scale swing as a fraction of AVcc, taken from the
/// analog Pin 2 DAC transfer curve in
/// `docs/audio/nsf/mmc5-audio-wiki.html` §"Pin 2 DAC Characteristic":
/// the `(DAC value / 255) * (0.4 * AVcc)` term — i.e. the DAC pin
/// covers a 0.4·AVcc range as the byte sweeps `$00..=$FF`.
const MMC5_PIN2_DAC_SWING: f32 = 0.4;

/// MMC5 raw-PCM DAC value at the centre of the §"Pin 2 DAC
/// Characteristic" swing. The curve runs from DAC=0 (`0.1·AVcc`) to
/// DAC=255 (`0.5·AVcc`); the DC-coupled signal is recentred about the
/// DAC=127.5 midpoint so the channel sits at 0 when idle.
const MMC5_PIN2_DAC_MIDPOINT: f32 = 127.5;

/// Map an 8-bit MMC5 raw-PCM DAC byte through the analog Pin 2 DAC
/// transfer curve to an AC-coupled audio sample in units of AVcc.
///
/// Per `docs/audio/nsf/mmc5-audio-wiki.html` §"Pin 2 DAC Characteristic"
/// the no-load Pin 2 voltage is
/// `Voltage = [(DAC/255) * (0.4·AVcc)] + (0.1·AVcc)`, spanning
/// `0.1·AVcc` (DAC=`$00`) to `0.5·AVcc` (DAC=`$FF`). The cartridge AC
/// couples the output, removing the `0.3·AVcc` DC midpoint, so the
/// audible value is the §-quoted swing recentred about DAC=127.5:
/// `(DAC/255 − 127.5/255) · 0.4 = ((DAC − 127.5)/255) · 0.4`.
fn pin2_dac_ac(dac: u8) -> f32 {
    (dac as f32 - MMC5_PIN2_DAC_MIDPOINT) / 255.0 * MMC5_PIN2_DAC_SWING
}

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
                // Length counter loads from the 2A03 LENGTH_TABLE
                // (§"Pulse 1": length counter "the same as their APU
                // counterparts"); only when the channel is enabled in
                // `$5015`. Phase reset + envelope restart on the
                // length register write match the 2A03 ($4003) write.
                if self.pulse[0].enabled {
                    self.pulse[0].length = crate::apu::LENGTH_TABLE[((value >> 3) & 0x1F) as usize];
                }
                self.pulse[0].step = 0;
                self.pulse[0].env_start = true;
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
                if self.pulse[1].enabled {
                    self.pulse[1].length = crate::apu::LENGTH_TABLE[((value >> 3) & 0x1F) as usize];
                }
                self.pulse[1].step = 0;
                self.pulse[1].env_start = true;
            }
            0x5010 => {
                // §"PCM Mode/IRQ ($5010)" — write:
                //   Ixxx xxxM, I = IRQ enable, M = mode (0 write, 1 read).
                self.irq_enable = value & 0x80 != 0;
                self.pcm_read_mode = value & 0x01 != 0;
            }
            0x5011 if !self.pcm_read_mode => {
                // §"Raw PCM ($5011)" + §"IRQ operation":
                //   value == 0 → irqTrip = 1, DAC unchanged;
                //   value != 0 → irqTrip = 0, DAC = value.
                self.dac_update_from_pcm_byte(value);
            }
            0x5015 => {
                self.status = value;
                self.pulse[0].enabled = value & 0x01 != 0;
                self.pulse[1].enabled = value & 0x02 != 0;
                // §"Status ($5015)" is "analogous to the APU Status
                // register": clearing a channel's enable bit forces its
                // length counter to zero (the 2A03 `$4015` behaviour).
                for p in &mut self.pulse {
                    if !p.enabled {
                        p.length = 0;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x5010 => {
                // §"PCM Mode/IRQ ($5010)" — read:
                //   bit 7 = (irqTrip AND irqEnable); the read also
                //   clears irqTrip per §"IRQ operation" pseudocode.
                //   bit 0 is the documented MMC5A-only "unknown"
                //   readback; we mirror the configured mode bit so
                //   software polling `$5010.0` after a mode write
                //   observes its own write back (the wiki's
                //   "MMC5A default power-on read value = $01" note
                //   matches read-mode=true → bit 0 = 1 here).
                let mut s = 0u8;
                if self.irq_trip && self.irq_enable {
                    s |= 0x80;
                }
                if self.pcm_read_mode {
                    s |= 0x01;
                }
                self.irq_trip = false;
                s
            }
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

    /// Apply a DAC-update byte coming from either a write-mode CPU
    /// write to `$5011` or a read-mode write-by-read on
    /// `$8000..=$BFFF` per `docs/audio/nsf/mmc5-audio-wiki.html`
    /// §"IRQ operation" pseudocode.
    fn dac_update_from_pcm_byte(&mut self, value: u8) {
        if value == 0 {
            // §"Raw PCM ($5011)": "Writing $00 to this register will
            // have no effect on the output sound, and does not change
            // the PCM counter." + §"PCM description": "If you try to
            // assign a value of $00, the DAC is not changed; an IRQ
            // is generated instead."
            self.irq_trip = true;
        } else {
            self.pcm = value;
            self.irq_trip = false;
        }
    }

    /// Bus hook for `$8000..=$BFFF` reads while PCM read-mode is
    /// active — the wiki's §"Raw PCM ($5011)" "Write-by-read writes
    /// to this register in PCM read-mode" semantic. Called by
    /// `NesBus::read` after the byte has been fetched from PRG ROM
    /// / bank-pool, so the DAC sees the actual byte the CPU
    /// observed. No-op outside read mode.
    pub fn observe_prg_read(&mut self, byte: u8) {
        if !self.enabled || !self.pcm_read_mode {
            return;
        }
        self.dac_update_from_pcm_byte(byte);
    }

    /// CPU IRQ line contribution per §"IRQ operation" final line —
    /// `Cart IRQ line = (irqTrip AND irqEnable)`.
    pub fn irq_line(&self) -> bool {
        self.irq_trip && self.irq_enable
    }

    pub fn tick(&mut self, cycles: u32) {
        // Split each batch at the next 240 Hz boundary so the
        // envelope / length clock fires exactly *between* the timer
        // cycles that surround its CPU offset — the same interleaving
        // discipline the 2A03 frame counter uses. Without the split,
        // a whole batch of timer cycles would run before an
        // envelope / length step that belongs in its middle.
        let mut remaining = cycles;
        while remaining > 0 {
            let until_frame = MMC5_FRAME_CPU - self.frame_acc;
            let n = remaining.min(until_frame);
            self.advance_pulse_timers(n);
            self.frame_acc += n;
            if self.frame_acc >= MMC5_FRAME_CPU {
                self.frame_acc -= MMC5_FRAME_CPU;
                // §"Pulse 1": "envelope and length counter are fixed
                // to a 240hz update rate" with no frame sequencer.
                // The free-running clock steps both units at once
                // (the length counter runs "twice as fast as the APU
                // length counter", i.e. the APU's 120 Hz half-frame
                // clock doubled to 240 Hz).
                for p in &mut self.pulse {
                    p.clock_envelope();
                    p.clock_length();
                }
            }
            remaining -= n;
        }
    }

    /// Advance the two pulse timers by `cycles` CPU cycles. §"Pulse 1":
    /// the channels "behave almost identically to the native pulse
    /// channels in the NES APU" — the 11-bit timer counts down once
    /// per APU (CPU/2) cycle, and on reaching zero reloads and clocks
    /// the 8-step duty sequencer, i.e. one duty step per
    /// `2 * (t + 1)` CPU cycles (f = CPU / (16 * (t + 1))). The /2
    /// prescaler keeps its dropped half-cycle across batches, and the
    /// timers run whether or not the channel is enabled (as on the
    /// 2A03, `$5015` gates the length counter / output, not the
    /// timer).
    fn advance_pulse_timers(&mut self, cycles: u32) {
        let total = cycles + self.pulse_prescaler_carry;
        let apu_cycles = total / 2;
        self.pulse_prescaler_carry = total & 1;
        for p in &mut self.pulse {
            for _ in 0..apu_cycles {
                if p.timer == 0 {
                    p.timer = p.timer_period;
                    p.step = (p.step + 1) & 0x07;
                } else {
                    p.timer -= 1;
                }
            }
        }
    }

    pub fn output(&self) -> f32 {
        // §"Pulse 1": "Frequency values less than 8 do not silence the
        // MMC5 pulse channels; they can output ultrasonic frequencies."
        // — so, unlike the 2A03, there is NO `timer_period >= 8` mute.
        // A channel is silenced only by its length counter reaching 0
        // (the APU-identical length-counter gate) or by the duty being
        // low; the volume comes from the envelope (or constant level).
        let p0 = self.pulse[0].pulse_output();
        let p1 = self.pulse[1].pulse_output();
        // Pulses share the 2A03 mixer curve approximation.
        let pulse_sum = (p0 + p1) as f32;
        let pulse_out = if pulse_sum <= 0.0 {
            0.0
        } else {
            95.88 / (8128.0 / pulse_sum + 100.0)
        };
        // Raw PCM channel mapped through the analog Pin 2 DAC transfer
        // curve. Per `docs/audio/nsf/mmc5-audio-wiki.html`
        // §"Pin 2 DAC Characteristic": "Pin 2 no-load voltage very
        // closely follows the equation:
        //   Voltage = [(DAC value / 255) * (0.4 * AVcc)] + (0.1 * AVcc)".
        // So the DAC pin spans 0.1·AVcc (DAC=0) .. 0.5·AVcc (DAC=255),
        // an affine map with a fixed 0.1·AVcc floor and a 0.4·AVcc
        // full-scale swing. AC-coupling on the cartridge strips the
        // 0.3·AVcc midpoint DC offset, so the audible signal (in units
        // of AVcc) is the swing recentred about that midpoint:
        //   ((DAC/255)·0.4 + 0.1) − 0.3 = (DAC/255 − 0.5) · 0.4.
        let pcm_out = pin2_dac_ac(self.pcm);
        // Polarity: per the doc's opening section, "the polarity of
        // all MMC5 channels is reversed compared to the APU" (squares
        // "equivalent in volume … but the polarity … reversed"; the
        // PCM channel "similarly equivalent in volume to the APU with
        // equivalent input, and inverted"). The whole chip
        // contribution is therefore negated relative to the
        // positive-swinging 2A03 mixer — inaudible in isolation once
        // AC-coupled, but it flips the phase-interference sense
        // wherever MMC5 and 2A03 pulses play the same material.
        -(pulse_out + pcm_out)
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
///
/// §"Audio Register Select ($C000-$DFFF)": the select byte is
/// `DDDDRRRR`; a nonzero high nibble `DDDD` disables writes to the
/// `$E000` data port (the AY-3-8910 register-write lock-out). The
/// low nibble `RRRR` always updates the selected register index.
#[derive(Default)]
pub struct Sunsoft5b {
    pub enabled: bool,
    pub addr: u8,
    /// §"Audio Register Select ($C000-$DFFF)": the select byte is
    /// `DDDDRRRR` — the low nibble `RRRR` chooses the internal
    /// register and the high nibble `DDDD`, when nonzero, "Disable
    /// writes to $E000 if nonzero (like the original AY-3-8910)".
    /// A later select write with a zero high nibble re-enables the
    /// data port. The selected register index is retained either way.
    pub writes_disabled: bool,
    pub regs: [u8; 16],
    pub channels: [S5bChan; 3],
    /// Noise generator — 5-bit period at `$06`, 17-bit LFSR shared
    /// by every channel whose `$07` noise-disable bit is clear.
    pub noise: S5bNoise,
    /// Envelope generator — 16-bit period at `$0B`/`$0C`, 32-step
    /// ramp, shape parameters at `$0D` low nibble.
    pub envelope: S5bEnvelope,
    /// CPU-cycle remainder toward the next 16-clock minor tick.
    /// §Sound drives tone / noise / envelope off "every 16th clock
    /// cycle" of the CPU clock; batches are rarely multiples of 16,
    /// so the leftover cycles must carry into the next batch — the
    /// old `cycles / 16` truncation silently discarded them, running
    /// the whole chip slow by exactly the fraction of time the CPU
    /// spent executing short instructions.
    pub clock_rem: u32,
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
            0xC000 => {
                // §"Audio Register Select": low nibble selects the
                // internal register; a nonzero high nibble disables
                // subsequent `$E000` data-port writes until a select
                // write clears it.
                self.addr = value & 0x0F;
                self.writes_disabled = (value & 0xF0) != 0;
            }
            // §"Audio Register Write": ignored entirely while the
            // data port is disabled by a nonzero select high nibble.
            0xE000 if self.writes_disabled => {}
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
        // envelope all observe a 16-clock minor tick per §Sound
        // ("counts up on every 16th clock cycle"). Whole 16-clock
        // intervals are consumed here and the remainder carries into
        // the next batch, so the chip's clock is exact regardless of
        // how the CPU chunks its cycles.
        let total = self.clock_rem + cycles;
        let intervals = total / 16;
        self.clock_rem = total % 16;
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
    /// channel's update. Retained as the most-recently-ticked
    /// channel's held sample for the per-channel read tests / status.
    pub last_output: f32,
    /// Per-channel (1..=8 → index 0..=7) sample-and-hold of the last
    /// value each channel computed on its update slot. The chip
    /// time-multiplexes one DAC across the active channels at the
    /// channel-update rate; rather than reproduce the (often
    /// inaudible-but-aliasing) switching waveform, §"Mixing" of
    /// `docs/audio/nsf/namco-163-audio-wiki.html` recommends summing
    /// the active channels and dividing by their count. We hold each
    /// channel's last update here so [`N163::output`] can form that
    /// sum instead of presenting whichever single channel happened to
    /// tick most recently.
    pub chan_hold: [f32; 8],
    /// `$E000-$E7FF` bit 6 per §"Sound Enable ($E000-E7FF)":
    /// "Disables sound if set. Sound is enabled on the 163 by writing
    /// a clear bit 6 to this register (0 is sufficient)." While set,
    /// the update cycle is stopped (no phase advance / RAM
    /// write-back) and the chip contributes silence.
    pub sound_disabled: bool,
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
            chan_hold: [0.0; 8],
            sound_disabled: false,
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
            // §"Sound Enable ($E000-E7FF)": bit 6 "Disables sound if
            // set. Sound is enabled on the 163 by writing a clear
            // bit 6 to this register (0 is sufficient)." (Bits 5-0
            // are PRG banking, outside the audio path.)
            0xE000..=0xE7FF => {
                self.sound_disabled = value & 0x40 != 0;
            }
            // §"Address Port ($F800-$FFFF)" — the whole 2 KiB window
            // decodes to the one register.
            0xF800..=0xFFFF => {
                self.addr = value & 0x7F;
                self.addr_inc = value & 0x80 != 0;
            }
            // §"Data Port ($4800-$4FFF)" — likewise a full window.
            0x4800..=0x4FFF => {
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

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            // Data Port read: return the sound-RAM byte at the current
            // address, then auto-increment the pointer when the `I` bit
            // is set. Per `docs/audio/nsf/namco-163-audio-wiki.html`
            // §"Address Port ($F800-$FFFF)" the increment happens "on
            // writes and reads to the Data Port ($4800)", and §"Data
            // Port ($4800-$4FFF)" confirms "When read, the appropriate
            // byte is returned" — the whole window decodes. Like the
            // write path the pointer "does not wrap, instead stopping
            // at $7F".
            0x4800..=0x4FFF => {
                let value = self.ram[self.addr as usize];
                if self.addr_inc && self.addr < 0x7F {
                    self.addr += 1;
                }
                value
            }
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
        let sample = signed as f32 * dec.volume as f32 / 128.0;
        // Hold this channel's sample for the §"Mixing" sum, and keep
        // `last_output` pointed at whichever channel ticked most
        // recently (the single-DAC view used by status reads).
        self.chan_hold[(ch - 1) as usize] = sample;
        self.last_output = sample;

        // Round-robin pointer through the active set.
        self.next_chan_slot = (slot + 1) % self.channels_active;
    }

    /// CPU-side tick: the chip updates one channel every 15 CPU
    /// cycles (§"Channel Update": "It takes exactly 15 CPU cycles to
    /// update and output one channel"). The accumulator carries the
    /// sub-15-cycle remainder across batches, so the update cadence
    /// is exact for any CPU chunking. While the §"Sound Enable
    /// ($E000-E7FF)" disable bit is set the update cycle is stopped —
    /// no phase advance, no RAM write-back.
    pub fn tick(&mut self, cycles: u32) {
        if !self.enabled || self.sound_disabled {
            return;
        }
        self.cycle_accum += cycles;
        while self.cycle_accum >= 15 {
            self.cycle_accum -= 15;
            self.tick_one_channel();
        }
    }

    /// Mixed N163 output. Per §"Mixing" of
    /// `docs/audio/nsf/namco-163-audio-wiki.html`: "it is often
    /// preferred to simply sum the channel outputs, and divide the
    /// output volume by the number of active channels." Each active
    /// channel contributes its held sample (`chan_hold`), and the sum
    /// is scaled by `1 / channels_active`. This keeps a multi-voice
    /// track balanced — without it the chip presented only whichever
    /// single channel ticked most recently, so at the host sample rate
    /// a `c`-channel song dropped roughly `(c-1)/c` of its voices at
    /// any instant. The doc notes the approximation runs "slightly too
    /// loud" for `c >= 6` because it does not compensate for the energy
    /// the real multiplexer transfers; we accept that documented bound
    /// rather than reproduce the audible switching waveform.
    pub fn output(&self) -> f32 {
        if self.channels_active == 0 || self.sound_disabled {
            return 0.0;
        }
        let mut sum = 0.0f32;
        for slot in 0..self.channels_active {
            let ch = self.active_channel(slot);
            sum += self.chan_hold[(ch - 1) as usize];
        }
        sum / self.channels_active as f32
    }

    /// Channel-update rate in Hz for the current `channels_active`,
    /// given the host CPU clock `cpu_hz`. Per
    /// `docs/audio/nsf/namco-163-audio-wiki.html` §"Channel Update":
    /// "It takes exactly 15 CPU cycles to update and output one
    /// channel. When multiple channels are used it will cycle between
    /// them." So one full pass over the `c` active channels takes
    /// `15 * c` CPU cycles, and any individual channel is refreshed at
    /// `cpu_hz / (15 * c)`.
    ///
    /// The §"Channel Update" table tabulates this for the NTSC clock
    /// (≈1789773 Hz): 1 channel → 119.318 kHz, 2 → 59.659 kHz, …,
    /// 8 → 14.915 kHz — and the PAL column (≈1662607 Hz): 110.840 kHz
    /// down to 13.855 kHz. Returns 0 when no channel is active.
    pub fn update_rate_hz(&self, cpu_hz: u32) -> f64 {
        if self.channels_active == 0 {
            return 0.0;
        }
        cpu_hz as f64 / (15.0 * self.channels_active as f64)
    }

    /// Emitted output frequency in Hz of channel `ch` (1..=8), per the
    /// closed form in `docs/audio/nsf/namco-163-audio-wiki.html`
    /// §"Frequency":
    ///
    /// ```text
    /// f = (n * p) / (15 * 65536 * l * c)
    /// ```
    ///
    /// where `n` = CPU clock rate (`cpu_hz`), `p` = the channel's
    /// 18-bit frequency value, `l` = wave length (in 4-bit samples),
    /// and `c` = number of enabled channels. The derivation: the high
    /// 8 bits of the 24-bit phase accumulator drive the wave position,
    /// each channel is updated once per `15 * c` CPU cycles adding its
    /// 18-bit `p`, and one full wave of `l` samples spans
    /// `l << 16` accumulator counts. Returns 0 when the channel is
    /// inactive, its frequency value is 0, or no channels are enabled.
    pub fn emitted_frequency_hz(&self, ch: u8, cpu_hz: u32) -> f64 {
        if self.channels_active == 0 || !(1..=8).contains(&ch) {
            return 0.0;
        }
        let dec = self.decode_channel(ch);
        if dec.freq == 0 || dec.wave_len == 0 {
            return 0.0;
        }
        // f = (n * p) / (15 * 65536 * l * c)
        (cpu_hz as f64 * dec.freq as f64)
            / (15.0 * 65536.0 * dec.wave_len as f64 * self.channels_active as f64)
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

/// The three rhythm (drum) patches present in the VRC7's instrument
/// ROM, per the §"Internal patch set" footnote in
/// `docs/audio/nsf/vrc7-audio-wiki.html`: "The VRC7 instrument ROM
/// dump also shows 3 drum patches. It is believed that these
/// additional patches are an artifact from the YM2413 and are not
/// playable on the VRC7." They are inaudible on the VRC7 — the chip
/// has no rhythm DAC (§"Rhythm Register $0E") — but the bytes are
/// documented ROM contents, including the one divergence from the
/// YM2413's drum ROM the same footnote calls out: "byte $07 of the
/// snare drum ($68) differs from YM2413 ($48)".
///
/// Order: Bass Drum; Snare Drum / Hi-Hat; Tom / Top Cymbal — the
/// shared-patch pairing matches the Table III-9 slot allocation
/// (see [`crate::opll::RhythmInstrument::slots`]), where HH+SD and
/// TOM+T-CY each share one channel's modulator/carrier slot pair.
pub const VRC7_RHYTHM_ROM: [[u8; 8]; 3] = [
    [0x01, 0x01, 0x18, 0x0F, 0xDF, 0xF8, 0x6A, 0x6D], // Bass Drum
    [0x01, 0x01, 0x00, 0x00, 0xC8, 0xD8, 0xA7, 0x68], // Snare Drum / Hi-Hat
    [0x05, 0x01, 0x00, 0x00, 0xF8, 0xAA, 0x59, 0x55], // Tom / Top Cymbal
];

/// The 16 hardwired instrument patches of the original YM2413 (OPLL),
/// transcribed from
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §2a — silicon-RE
/// ROM contents (YM2413B debug-mode dump cross-checked against the
/// FHB013 die-shot bits). Slot 0 is the user "custom patch"
/// placeholder, as on the VRC7.
///
/// Selected as the default patch set when an NSFe `VRC7` chunk names
/// device variant `1` (YM2413) — per
/// `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7: "If a replacement
/// patch set is not contained in this chunk, an appropriate default
/// patch set should be used for the selected device."
pub const YM2413_INSTRUMENT_ROM: [[u8; 8]; 16] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // 0 (Custom Patch)
    [0x71, 0x61, 0x1E, 0x17, 0xD0, 0x78, 0x00, 0x17], // 1 Violin
    [0x13, 0x41, 0x1A, 0x0D, 0xD8, 0xF7, 0x23, 0x13], // 2 Guitar
    [0x13, 0x01, 0x99, 0x00, 0xF2, 0xC4, 0x11, 0x23], // 3 Piano
    [0x31, 0x61, 0x0E, 0x07, 0xA8, 0x64, 0x70, 0x27], // 4 Flute
    [0x32, 0x21, 0x1E, 0x06, 0xE0, 0x76, 0x00, 0x28], // 5 Clarinet
    [0x31, 0x22, 0x16, 0x05, 0xE0, 0x71, 0x00, 0x18], // 6 Oboe
    [0x21, 0x61, 0x1D, 0x07, 0x82, 0x81, 0x10, 0x07], // 7 Trumpet
    [0x23, 0x21, 0x2D, 0x14, 0xA2, 0x72, 0x00, 0x07], // 8 Organ
    [0x61, 0x61, 0x1B, 0x06, 0x64, 0x65, 0x10, 0x17], // 9 Horn
    [0x41, 0x61, 0x0B, 0x18, 0x85, 0xF7, 0x71, 0x07], // A Synthesizer
    [0x13, 0x01, 0x83, 0x11, 0xFA, 0xE4, 0x10, 0x04], // B Harpsichord
    [0x17, 0xC1, 0x24, 0x07, 0xF8, 0xF8, 0x22, 0x12], // C Vibraphone
    [0x61, 0x50, 0x0C, 0x05, 0xC2, 0xF5, 0x20, 0x42], // D Synthesizer Bass
    [0x01, 0x01, 0x55, 0x03, 0xC9, 0x95, 0x03, 0x02], // E Acoustic Bass
    [0x61, 0x41, 0x89, 0x03, 0xF1, 0xE4, 0x40, 0x13], // F Electric Guitar
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
/// fundamental frequencies were honoured but the FM operator math used
/// a 2-operator sinusoidal stand-in instead of OPLL's logarithmic
/// LUTs.
///
/// Round 13 added patch decoding — the hardwired §"Internal patch set"
/// ROM (15 named instruments + slot 0 user-programmable) is exposed
/// as [`VRC7_INSTRUMENT_ROM`], the user-programmable patch at
/// `regs[0x00..=0x07]` and each ROM slot decode to a [`Vrc7Patch`]
/// struct, and each channel's `$3X` high nibble selects the active
/// patch.
///
/// Round 14 (this round) wires the actual OPLL operator pipeline from
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` + the andete
/// log-sin/exp tables in
/// `docs/audio/nsf/opll-ym2413/ym2413-logsin-exp-tables-andete-2015-04-09.txt`:
/// the per-channel phase generator, modulator/carrier pair, modulator
/// self-feedback, and the envelope generator with key-on/off + sustain
/// transitions all run through [`crate::opll::OpllChannel`]. The
/// sinusoidal stand-in is gone — `output()` reads from the OPLL
/// channels directly. The KSL attenuation table and the per-rate
/// envelope-increment numeric arrays are left as documented
/// followups (see crate README §Round 14+ followups for the docs
/// gap).
pub struct Vrc7 {
    pub enabled: bool,
    pub addr: u8,
    pub regs: [u8; 0x40],
    /// Per-channel register-level state (the patch index, volume,
    /// key-on bit, block / fnum) decoded from the most recent
    /// register write.
    pub channels: [Vrc7Chan; 6],
    /// OPLL synthesis engines, one per channel. Driven from the
    /// register-level state in [`Vrc7::channels`] and the staged
    /// patch table.
    pub opll_channels: [crate::opll::OpllChannel; 6],
    /// Accumulated CPU cycles not yet converted into operator
    /// samples. The OPLL operator clock is the master 3.579545 MHz
    /// crystal / 72 = 49.7163 kHz; the NES CPU runs at 1.789773 MHz
    /// so the operator-tick interval is `1.789773e6 / 49716.3 ≈
    /// 36.0` CPU cycles per operator sample. We accumulate fractional
    /// cycles in Q8 fixed-point: `cycles_q8 += cpu_cycles << 8`, then
    /// emit one operator sample every `OP_CYCLES_Q8` units.
    pub op_cycles_q8: u32,
    /// Last operator sample, latched for the host mixer to read via
    /// [`Vrc7::output`] until the next operator tick.
    pub latched_output: i32,
    /// Decoded `$0F` test-register state. Per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F":
    /// the low 4 bits override envelope output (bit 0), hold the LFO
    /// at zero (bit 1), hold the waveform phase at zero (bit 2), and
    /// run the LFOs much faster (bit 3). Each operator's per-sample
    /// path consults this struct via
    /// [`crate::opll::OpllChannel::sample_with_test`].
    pub test_register: crate::opll::TestRegister,
    /// Audio Reset (`$E000` bit 6) state per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Audio Reset ($E000)":
    /// "Setting this bit will silence the expansion audio and clear
    /// its registers (including tremolo LFO state, but not including
    /// vibrato LFO state). Writes to $9010 and $9030 are disregarded
    /// while this bit is set." Default false (BIOS starts the chip
    /// in the unreset state).
    pub audio_reset_held: bool,
    /// NSFe `VRC7` chunk device selector — `true` when the chunk named
    /// device variant `1` (YM2413), which swaps the default instrument
    /// ROM to [`YM2413_INSTRUMENT_ROM`] per
    /// `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7 ("an appropriate
    /// default patch set should be used for the selected device").
    pub ym2413_variant: bool,
    /// NSFe `VRC7` chunk replacement patch set: 16 patches × 8 bytes
    /// in register format, overriding the built-in instrument ROM for
    /// slots `1..=15` (slot 0 stays the live user patch — the spec
    /// expects the table's first 8 bytes to be zero "since on the
    /// VRC7 patch 0 is custom-only"). The optional 24 extra bytes of
    /// the 152-byte form customise YM2413 rhythm instruments, which
    /// the VRC7 cannot voice (no rhythm DAC), so they are not stored
    /// here.
    pub patch_override: Option<[[u8; 8]; 16]>,
    /// Built-in AM (tremolo) + VIB (vibrato) low-frequency
    /// oscillators. Per `docs/audio/nsf/vrc7-audio-wiki.html`
    /// §"Test Register $0F" the LFO phases advance once every 64
    /// (tremolo) / 1024 (vibrato) per-operator samples in normal
    /// mode, every sample under `$0F` bit 3, and are held+reset under
    /// `$0F` bit 1. The §"Audio Reset ($E000)" clears the tremolo
    /// phase but preserves the vibrato phase. Advanced once per
    /// emitted operator sample in [`Vrc7::tick`] and read per operator
    /// in [`crate::opll::OpllChannel::sample_with_test`]: the phase is
    /// mapped through a triangle scaled to the §7 *physical* depths
    /// (1.0 dB AM / ±7-cent VIB), so an operator with its `$00`/`$01`
    /// AM / VIB bit set is audibly modulated.
    pub lfo: crate::opll::Lfo,
}

impl Default for Vrc7 {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: 0,
            regs: [0u8; 0x40],
            channels: [Vrc7Chan::default(); 6],
            opll_channels: [crate::opll::OpllChannel::default(); 6],
            op_cycles_q8: 0,
            latched_output: 0,
            test_register: crate::opll::TestRegister::default(),
            audio_reset_held: false,
            ym2413_variant: false,
            patch_override: None,
            lfo: crate::opll::Lfo::default(),
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
            // §"Audio Reset ($E000)": bit 6 (R) is the expansion-audio
            // reset; the rest of the byte is mapper mirroring/WRAM
            // control and is not our concern here.
            0xE000 => {
                let now = value & 0x40 != 0;
                let was = self.audio_reset_held;
                self.audio_reset_held = now;
                if now && !was {
                    // Entering reset: "silence the expansion audio
                    // and clear its registers (including tremolo LFO
                    // state, but not including vibrato LFO state)."
                    // The tremolo phase is cleared and the vibrato
                    // phase preserved by `Lfo::audio_reset` per the
                    // §"Audio Reset ($E000)" asymmetry; the register
                    // clear + silence are observable.
                    self.regs = [0u8; 0x40];
                    self.channels = [Vrc7Chan::default(); 6];
                    self.opll_channels = [crate::opll::OpllChannel::default(); 6];
                    self.latched_output = 0;
                    self.op_cycles_q8 = 0;
                    self.test_register = crate::opll::TestRegister::default();
                    self.lfo.audio_reset();
                    // §"Test Register $0F" bit 1 reset semantics
                    // (LFO held + reset) overlap with the audio
                    // reset's LFO clear; both leave the LFO at
                    // zero, which is its uninitialised state here.
                }
            }
            // §"Audio Reset ($E000)": "Writes to $9010 and $9030 are
            // disregarded while this bit is set."
            0x9010 if !self.audio_reset_held => self.addr = value & 0x3F,
            0x9030 if !self.audio_reset_held => {
                let a = self.addr as usize;
                self.regs[a] = value;
                if a == 0x0F {
                    // §"Test Register $0F" — record the decoded
                    // bitfield in addition to the raw byte. Done
                    // here (not in refresh_from_regs) because the
                    // test register is chip-wide, not per-channel.
                    self.test_register = crate::opll::TestRegister::from_byte(value);
                }
                // §"Internal patch set": registers $00-$07 are the
                // user-programmable instrument (patch slot 0). A write
                // to any of them must reload the live envelopes of every
                // channel currently selecting patch 0 — otherwise an
                // already-keyed user-patch channel keeps the operator
                // constants captured at its last $3X (patch-select)
                // write, even though the user just reprogrammed them.
                let user_patch_write = a <= 0x07;
                self.refresh_from_regs(user_patch_write);
            }
            _ => {}
        }
    }

    fn refresh_from_regs(&mut self, user_patch_write: bool) {
        for ch in 0..6 {
            let new_fnum =
                (self.regs[0x10 + ch] as u16) | (((self.regs[0x20 + ch] & 0x01) as u16) << 8);
            let new_block = (self.regs[0x20 + ch] >> 1) & 0x07;
            // $2X bitfield --STOOOH: bit 4 = trigger / key-on,
            // bit 5 = sustain override (§Channels).
            let new_key_on = self.regs[0x20 + ch] & 0x10 != 0;
            let new_sustain = self.regs[0x20 + ch] & 0x20 != 0;
            // $3X bitfield IIIIVVVV: high nibble = instrument index,
            // low nibble = inverted volume.
            let new_patch_index = (self.regs[0x30 + ch] >> 4) & 0x0F;
            let new_volume = self.regs[0x30 + ch] & 0x0F;

            let was_key_on = self.channels[ch].key_on;
            let was_sustain = self.channels[ch].sustain;
            let patch_changed = self.channels[ch].patch_index != new_patch_index;
            let volume_changed = self.channels[ch].volume != new_volume;
            let sustain_changed = was_sustain != new_sustain;

            self.channels[ch].fnum = new_fnum;
            self.channels[ch].block = new_block;
            self.channels[ch].key_on = new_key_on;
            self.channels[ch].sustain = new_sustain;
            self.channels[ch].patch_index = new_patch_index;
            self.channels[ch].volume = new_volume;

            // Mirror register-level state into the OPLL synthesis
            // engines. The patch + volume are reloaded whenever
            // either changes; the fnum + block are mirrored every
            // tick so a sweep mid-note is honoured.
            let pitch_changed = self.opll_channels[ch].fnum != new_fnum
                || self.opll_channels[ch].block != new_block;
            self.opll_channels[ch].fnum = new_fnum;
            self.opll_channels[ch].block = new_block;
            // A user-patch ($00-$07) write reloads any channel selecting
            // patch slot 0 with the freshly-reprogrammed constants.
            let user_patch_reload = user_patch_write && new_patch_index == 0;
            if patch_changed || volume_changed || user_patch_reload {
                let p = self.patch(new_patch_index);
                self.opll_channels[ch].load_patch(&p, new_volume);
                // Re-apply the channel-level sustain override; the
                // patch load reset the release rate to the patch's
                // own value.
                if new_sustain {
                    self.opll_channels[ch].set_channel_sustain_override(true, &p);
                }
            } else if sustain_changed {
                // Sustain bit flipped without a patch swap — just
                // update the release-rate override.
                let p = self.patch(new_patch_index);
                self.opll_channels[ch].set_channel_sustain_override(new_sustain, &p);
            }
            // §III-1-2 KSR — when the pitch changes the Rks offset
            // changes, so re-derive it for both operators. `load_patch`
            // above already does this; only call again on a pure
            // pitch-only write.
            if pitch_changed && !(patch_changed || volume_changed) {
                self.opll_channels[ch].refresh_rks();
            }

            // Key-on / key-off edge detection. The OPLL channel's
            // own `key_on` flag tracks whether we've already issued
            // the edge, so repeat writes of the same state don't
            // re-trigger.
            match (was_key_on, new_key_on) {
                (false, true) => self.opll_channels[ch].trigger_key_on(),
                (true, false) => self.opll_channels[ch].trigger_key_off(),
                _ => {}
            }
        }
    }

    /// Return the decoded patch parameters for instrument slot
    /// `index`. Slot `0` reads from the user-programmable
    /// `regs[0x00..=0x07]`; slots `1..=15` read from the NSFe `VRC7`
    /// chunk's replacement patch set when one was supplied, otherwise
    /// from the built-in ROM of the selected device variant
    /// ([`VRC7_INSTRUMENT_ROM`], or [`YM2413_INSTRUMENT_ROM`] when
    /// the chunk named device `1`). Indices `>= 16` wrap modulo 16
    /// (the `$3X` instrument field is only 4 bits wide so this is a
    /// defensive default, never a real write).
    pub fn patch(&self, index: u8) -> Vrc7Patch {
        let i = (index as usize) & 0x0F;
        if i == 0 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&self.regs[0x00..0x08]);
            Vrc7Patch::from_bytes(&b)
        } else if let Some(table) = &self.patch_override {
            Vrc7Patch::from_bytes(&table[i])
        } else if self.ym2413_variant {
            Vrc7Patch::from_bytes(&YM2413_INSTRUMENT_ROM[i])
        } else {
            Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[i])
        }
    }

    /// Apply an NSFe `VRC7` chunk to the chip per
    /// `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7: byte 0 selects
    /// the device variant ("0 = VRC7, 1 = YM2413" — the variant picks
    /// the default instrument ROM), and the optional 128- or 152-byte
    /// remainder is "a replacement patch set for the device" (16 × 8
    /// register-format bytes; "The first 8 bytes of this patch set
    /// are expected to be zero, since on the VRC7 patch 0 is
    /// custom-only"). The 24 extra bytes of the 152-byte form are
    /// YM2413 rhythm patches, "not accessible on VRC7" — the VRC7 has
    /// no rhythm DAC, so they are accepted and ignored.
    pub fn apply_nsfe_chunk(&mut self, device: u8, patches: Option<&[u8]>) {
        self.ym2413_variant = device == 1;
        if let Some(bytes) = patches {
            if bytes.len() >= 128 {
                let mut table = [[0u8; 8]; 16];
                for (i, row) in table.iter_mut().enumerate() {
                    row.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
                }
                // Slot 0 stays live user-patch territory regardless of
                // what the table carried; `patch()` never reads it.
                self.patch_override = Some(table);
            }
        }
    }

    /// Return the currently selected patch for channel `ch` (0..=5).
    pub fn active_patch(&self, ch: usize) -> Vrc7Patch {
        self.patch(self.channels[ch].patch_index)
    }

    /// Effective rhythm-control state — constant on the VRC7.
    ///
    /// Per `docs/audio/nsf/vrc7-audio-wiki.html` §"Rhythm Register
    /// $0E": "In normal operation, the 'rhythm mode' bit in register
    /// $0E is treated as though it were always enabled, resulting
    /// [in] only six audible FM channels. The VRC7 has no rhythm DAC,
    /// so the 5 rhythm channels are always inaudible." And per
    /// §"Internal Audio Registers", register values outside the
    /// documented set ($00-$07, $10-$15, $20-$25, $30-$35, $0F) "are
    /// ignored" — so a `$0E` write is recorded in [`Vrc7::regs`] as
    /// raw bookkeeping but never reaches the synthesis path. The
    /// returned state therefore always reads `rhythm_mode = true`
    /// with all five drum keys off, regardless of what was written.
    /// (Disabling the bit for 9-channel OPLL audio requires the
    /// hardware debug mode on pin 15, which an NSF rip cannot
    /// exercise.)
    pub fn rhythm_control(&self) -> crate::opll::RhythmRegister {
        crate::opll::RhythmRegister {
            rhythm_mode: true,
            ..crate::opll::RhythmRegister::default()
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        // §"Audio Reset ($E000)": "Setting this bit will silence the
        // expansion audio…" — while held, the operator pipeline is
        // pinned at zero and no samples are emitted.
        if self.audio_reset_held {
            self.latched_output = 0;
            return;
        }
        // NES master CPU clock: 1.789773 MHz. OPLL operator clock:
        // 3.579545 MHz / 72 ≈ 49.7163 kHz, so the operator-sample
        // interval is `1_789_773 / 49_716.3 ≈ 35.9956` CPU cycles
        // per operator sample. We track that in Q8 fixed-point.
        const OP_CYCLES_Q8: u32 =
            (1_789_773.0_f64 / crate::opll::OPLL_SAMPLE_RATE_HZ as f64 * 256.0) as u32;
        self.op_cycles_q8 = self.op_cycles_q8.saturating_add(cycles << 8);
        while self.op_cycles_q8 >= OP_CYCLES_Q8 {
            self.op_cycles_q8 -= OP_CYCLES_Q8;
            // Emit one operator sample: sum the 6 carrier outputs.
            // The chip-wide `$0F` test register is consulted per
            // channel so bits 0/2 (envelope-bypass / phase-hold)
            // override the synthesis path uniformly.
            let test = self.test_register;
            // Advance the built-in AM/VIB LFOs once per operator
            // sample. `$0F` bit 1 holds+resets both phases; bit 3
            // makes both advance every sample instead of once per
            // 64 / 1024 samples. The triangle-mapped AM / VIB depth
            // (§7 1.0 dB / ±7 cents) is read per operator in
            // `sample_with_test` below.
            self.lfo.tick(test.hold_lfo, test.fast_lfo);
            let lfo = self.lfo;
            let mut sum: i32 = 0;
            for ch in &mut self.opll_channels {
                if ch.is_active() || test.envs_zero {
                    // bit 0 forces full volume even when a channel's
                    // envelope sits at Idle (because the carrier
                    // would normally be silenced and we'd skip
                    // sampling it). Always sample when bit 0 is set.
                    sum = sum.saturating_add(ch.sample_with_test(&test, &lfo));
                }
            }
            self.latched_output = sum;
        }
    }

    pub fn output(&self) -> f32 {
        // Per `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §6
        // the operator output is a signed 9-bit linear amplitude
        // (peak ±255 at volume=0; row-256). Summed across 6 channels
        // the worst-case peak is ±1530. Normalise to the host
        // mixer's roughly ±1.0 range, with a modest headroom margin
        // so a many-voice patch doesn't clip the float bus before
        // the APU mixer's own headroom kicks in.
        const NORMALIZATION: f32 = 1.0 / 2048.0; // 6 channels × 255 + headroom
        self.latched_output as f32 * NORMALIZATION
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
                // bit 7 runs both envelopes 4x faster (and halts the
                // mod-table accumulator + the wave unit).
                self.env_halt = value & 0x40 != 0;
                self.env_fast = value & 0x80 != 0;
                if self.env_halt {
                    self.vol_env_timer = self.vol_env_period();
                    self.mod_env_timer = self.mod_env_period();
                }
                if self.env_fast {
                    // §Wavetables: "Disabling the wave unit via the
                    // high bit of $4083 immediately resets its
                    // accumulator, delaying the next tick after they
                    // are enabled again until the next overflow.
                    // Consequently, this also resets the wave position
                    // to 0 (i.e. the $4040 value)."
                    self.wave_acc = 0;
                    self.wave_pos = 0;
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
        // top 6 bits. The `$4083` bit-7 halt disables this unit too
        // (§Wavetables: "Disabling the wave unit via the high bit of
        // $4083…" — the accumulator was already reset by the write).
        if self.freq != 0 && !self.env_fast {
            self.wave_acc = (self.wave_acc.wrapping_add(self.wave_pitch())) & 0xFF_FFFF;
            self.wave_pos = ((self.wave_acc >> 18) & 0x3F) as u8;
        }
    }

    /// Step the volume + mod envelope ramp generators by ONE CPU
    /// clock. Each envelope counts its own `c = 8·(e+1)·(m+1)` timer;
    /// on expiry it ramps the gain ±1 (clamped 0..=32 on the active
    /// edge) per §"Unit tick → Envelopes". Disabled (master speed 0),
    /// halted (`$4083` bit 6), or mode-bit-set envelopes do not ramp.
    fn env_tick_one(&mut self) {
        if self.env_halt || self.master_env_speed == 0 {
            return;
        }
        // Volume envelope.
        if !self.vol_env_disabled {
            let period = self.vol_env_period();
            if period != 0 {
                if self.vol_env_timer == 0 {
                    self.vol_env_timer = period;
                }
                self.vol_env_timer -= 1;
                if self.vol_env_timer == 0 {
                    self.step_volume_env();
                    self.vol_env_timer = period;
                }
            }
        }
        // Mod envelope.
        if !self.mod_env_disabled {
            let period = self.mod_env_period();
            if period != 0 {
                if self.mod_env_timer == 0 {
                    self.mod_env_timer = period;
                }
                self.mod_env_timer -= 1;
                if self.mod_env_timer == 0 {
                    self.step_mod_env();
                    self.mod_env_timer = period;
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
        // Walk the batch one CPU cycle at a time so the envelope ramp
        // timers and the 16-cycle wave/mod unit tick interleave in
        // true cycle order — a mod-envelope step that lands mid-batch
        // changes the mod gain (and therefore the §"Modulation unit"
        // pitch formula) for the unit ticks that follow it in the
        // same batch, instead of the whole batch's envelope work
        // running up front.
        for _ in 0..cycles {
            self.env_tick_one();
            self.cycle_acc += 1;
            if self.cycle_acc >= 16 {
                self.cycle_acc -= 16;
                self.unit_tick();
                // The wave position advanced; commit any staged volume
                // gain now that we may be at wave position 0.
                self.commit_pending_volume();
            }
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

    pub fn read(&mut self, addr: u16) -> u8 {
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

    /// True iff any enabled expansion chip is currently asserting its
    /// own IRQ line. Round 18 wires the MMC5 PCM IRQ source per
    /// `docs/audio/nsf/mmc5-audio-wiki.html` §"IRQ operation"; the
    /// other expansion chips do not raise CPU IRQs.
    pub fn irq_line(&self) -> bool {
        self.mmc5.enabled && self.mmc5.irq_line()
    }

    /// Bus hook for CPU reads on `$8000..=$BFFF`. When the MMC5 is
    /// enabled and in PCM read-mode, the byte the CPU just observed
    /// is also routed into the MMC5 DAC update path per the
    /// "Write-by-read writes to this register in PCM read-mode"
    /// note in the MMC5-audio wiki §"Raw PCM ($5011)". No-op
    /// otherwise.
    pub fn observe_prg_read(&mut self, addr: u16, byte: u8) {
        if (0x8000..=0xBFFF).contains(&addr) {
            self.mmc5.observe_prg_read(byte);
        }
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

    // ---------------- Round 223: VRC6 sawtooth 14-step cycle ----------------
    //
    // Spec source: `docs/audio/nsf/vrc6-audio-wiki.html`
    //   §"Sawtooth Channel" — 14-step internal cycle, "after A has been
    //                         added 6 times, on the 7th clock, instead
    //                         of A being added, the internal
    //                         accumulator is reset to zero" + the
    //                         walked example for A=$08 producing
    //                         accumulator $00, $00, $08, $08, $10, $10,
    //                         $18, $18, $20, $20, $28, $28, $30, $30
    //                         then resetting to $00.
    //   §"Sawtooth Channel" — E-clear forces the accumulator to zero;
    //                         the frequency divider is preserved on
    //                         falling edge of E.
    //
    // The previous bit-mask `step & 0x0D` produced 1/2/3/8/9/12/13
    // rather than the §example walk; modulo 14 now matches.

    /// Drive the saw chip until `step` returns to the same position the
    /// next timer expiry would land it on, emitting one accumulator
    /// reading after every timer expiry — i.e. one reading per saw
    /// "clock" in the §"Sawtooth Channel" walkthrough.
    fn vrc6_saw_walk(chip: &mut Vrc6, n_clocks: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n_clocks);
        for _ in 0..n_clocks {
            // chip.tick consumes one cycle at a time so the timer
            // walks predictably; we tick exactly (timer + 1) cycles
            // (or `timer_period + 1` after a load) to step the saw
            // by one. The +1 absorbs the "0 → reload + step" tick.
            let period = chip.saw.timer_period;
            chip.tick(period as u32 + 1);
            out.push(chip.saw.accum);
        }
        out
    }

    #[test]
    fn vrc6_saw_walk_a_08_matches_example_table() {
        // §"Sawtooth Channel" example: A=$08, expected accumulator
        // sequence beginning at step 0:
        //   step 0..13 → $00, $00, $08, $08, $10, $10, $18, $18, $20,
        //   $20, $28, $28, $30, $30, then (step 0 again) $00.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08); // A = $08
        chip.write(0xB001, 0x10); // period low
        chip.write(0xB002, 0x80); // E=1, period high = 0
                                  // The chip starts on step 0 — the first tick we observe
                                  // advances to step 1 (no-op), so we read $00. Walk 14 clocks
                                  // to reach the full cycle plus the reset wrap.
        let walk = vrc6_saw_walk(&mut chip, 14);
        // step 1=odd→$00, 2=add→$08, 3=$08, 4=$10, 5=$10, 6=$18,
        // 7=$18, 8=$20, 9=$20, 10=$28, 11=$28, 12=$30, 13=$30,
        // 0=reset→$00.
        assert_eq!(
            walk,
            vec![
                0x00, 0x08, 0x08, 0x10, 0x10, 0x18, 0x18, 0x20, 0x20, 0x28, 0x28, 0x30, 0x30, 0x00,
            ]
        );
    }

    #[test]
    fn vrc6_saw_resets_after_seventh_clock_per_spec() {
        // §"Sawtooth Channel": "after A has been added 6 times, on
        // the 7th clock, instead of A being added, the internal
        // accumulator is reset to zero." A=$01 makes each add
        // visible without aliasing.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x01);
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80);
        let walk = vrc6_saw_walk(&mut chip, 28);
        // Two full cycles: first cycle peaks at $06, second cycle
        // also peaks at $06 after the reset.
        assert_eq!(walk[12], 0x06, "1st cycle final add reaches A * 6");
        assert_eq!(walk[13], 0x00, "step 0 of 2nd cycle resets to 0");
        assert_eq!(walk[26], 0x06, "2nd cycle final add reaches A * 6");
        assert_eq!(walk[27], 0x00, "step 0 of 3rd cycle resets to 0");
    }

    #[test]
    fn vrc6_saw_output_is_top_five_accumulator_bits() {
        // §"Output": "The final mix is a 6-bit DAC summing the two
        // 4-bit pulse outputs and the high 5 bits of the saw
        // accumulator."
        //
        // A=$08 produces an accumulator that maxes at $30; the high
        // 5 bits are accum >> 3, so the §example "Output" column
        // reads 0,0,1,1,2,2,3,3,4,4,5,5,6,6,0.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0x10);
        chip.write(0xB002, 0x80);
        // Walk through one full cycle and verify the output
        // contribution at each step matches the §example.
        let mut samples = Vec::with_capacity(14);
        for _ in 0..14 {
            let period = chip.saw.timer_period;
            chip.tick(period as u32 + 1);
            samples.push((chip.saw.accum >> 3) as u32);
        }
        assert_eq!(samples, vec![0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 0]);
    }

    #[test]
    fn vrc6_saw_e_clear_forces_accumulator_to_zero() {
        // §"Sawtooth Channel": "If E is clear, the accumulator is
        // forced to zero until E is again set."
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0x04);
        chip.write(0xB002, 0x80); // E=1
                                  // Drive a few clocks to put the accumulator above zero.
        let _ = vrc6_saw_walk(&mut chip, 4);
        assert!(
            chip.saw.accum > 0,
            "accumulator must have ramped above zero"
        );
        // Falling edge on E forces accumulator + step to 0.
        chip.write(0xB002, 0x00); // E=0
        assert_eq!(chip.saw.accum, 0);
        assert_eq!(chip.saw.step, 0);
        // Output path also reads zero while disabled (saw block in
        // `output()` is gated on `saw.enabled`).
        assert_eq!(chip.output(), 0.0);
    }

    #[test]
    fn vrc6_saw_e_clear_preserves_frequency_divider() {
        // §"Sawtooth Channel": "Clearing E does not reset the
        // frequency divider, however, so the first step of the
        // reset saw may appear shortened."
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0xFF);
        chip.write(0xB002, 0x80); // E=1, period = $0FF
                                  // Tick partway through the timer so the divider is mid-count.
        chip.tick(50);
        let timer_before = chip.saw.timer;
        assert!(timer_before > 0 && timer_before < chip.saw.timer_period);
        // Clearing E zeroes the accumulator + step but leaves the
        // running divider untouched.
        chip.write(0xB002, 0x00);
        assert_eq!(chip.saw.timer, timer_before, "frequency divider preserved");
    }

    #[test]
    fn vrc6_saw_disabled_chip_holds_accumulator_at_zero_under_tick() {
        // §"Sawtooth Channel": "If E is clear, the accumulator is
        // forced to zero **until E is again set**." A `tick` while
        // E is clear must not advance the accumulator.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x20); // a non-trivial rate
        chip.write(0xB001, 0x00); // tight period
        chip.write(0xB002, 0x00); // E=0
        chip.tick(10_000);
        assert_eq!(chip.saw.accum, 0);
        assert_eq!(chip.saw.step, 0);
    }

    #[test]
    fn vrc6_saw_distorts_when_rate_exceeds_42() {
        // §"Sawtooth Channel" footnote: "If A is more than 42
        // (floor(255 / 6)), the accumulator will wrap, resulting in
        // distorted sound."
        //
        // A=43 makes 43 * 6 = 258 → wraps past the 8-bit accumulator
        // ceiling (255) by 3 on the final add. Walk through one
        // cycle and confirm the wrap happens (the §footnote labels
        // it "distorted").
        let mut chip = Vrc6::new();
        chip.write(0xB000, 43); // A = 43, > 42 threshold
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80);
        // Step through the 6 adds + reset.
        let walk = vrc6_saw_walk(&mut chip, 14);
        // Final add lands at 43*6 = 258, wrapped to 258 - 256 = 2.
        assert_eq!(walk[12], 2, "6th add wraps past 255 → distortion");
        assert_eq!(walk[13], 0, "step-0 reset still fires on the next clock");
    }

    #[test]
    fn vrc6_saw_a_zero_rate_silences_channel() {
        // §"Saw Accum Rate ($B000)" sets a 6-bit rate field with no
        // special-case carve-out — A=0 simply never adds anything,
        // so the accumulator stays at zero (and is reset back to
        // zero every 14th clock anyway).
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x00); // A = 0
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80);
        let walk = vrc6_saw_walk(&mut chip, 28);
        assert!(walk.iter().all(|&v| v == 0));
    }

    #[test]
    fn vrc6_saw_rate_masked_to_six_bits() {
        // §"Saw Accum Rate ($B000)" layout `..AA AAAA`: the top two
        // bits of the byte are documented as inert.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0xC8); // top bits set, A = $08
        assert_eq!(chip.saw.rate, 0x08);
    }

    #[test]
    fn vrc6_saw_re_enable_restarts_phase_at_step_zero() {
        // §"Sawtooth Channel": clearing then immediately setting E
        // "mostly resets" the phase. Our model resets step + accum
        // on E falling edge so a re-enable starts a fresh 14-step
        // cycle from step 0.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80);
        // Walk into the middle of a cycle.
        let _ = vrc6_saw_walk(&mut chip, 5);
        assert!(chip.saw.step > 0);
        // Clear E, then set E: step + accum back to 0.
        chip.write(0xB002, 0x00);
        chip.write(0xB002, 0x80);
        assert_eq!(chip.saw.step, 0);
        assert_eq!(chip.saw.accum, 0);
        // Walk one full cycle; first non-zero accumulator hit must
        // be $08 (the first add at step 2).
        let walk = vrc6_saw_walk(&mut chip, 14);
        assert_eq!(walk[0], 0x00); // step 1
        assert_eq!(walk[1], 0x08); // step 2
    }

    #[test]
    fn vrc6_saw_period_zero_still_ticks_step_each_cycle() {
        // §"Sawtooth Channel": the divider counts down from `t`
        // until it reaches zero, at which point it reloads. With
        // period_low+high both 0, the timer reloads with 0 on every
        // expiry — so a single CPU cycle advances one step in the
        // 14-clock cycle.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80); // period = 0
        chip.tick(1);
        assert_eq!(chip.saw.step, 1);
        chip.tick(1);
        assert_eq!(chip.saw.step, 2);
        assert_eq!(chip.saw.accum, 0x08); // first add fires at step 2
    }

    #[test]
    fn vrc6_halt_overrides_all_other_freq_control_bits() {
        // §"Frequency Control ($9003)": "The halt flag overrides
        // the other flags." H=1 must freeze the saw + pulses
        // regardless of A/B.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0x00);
        chip.write(0xB002, 0x80);
        // H=1, B=1, A=1 — A/B would otherwise turn the saw into
        // 256x; halt overrides.
        chip.write(0x9003, 0b0000_0111);
        chip.tick(10_000);
        assert_eq!(chip.saw.step, 0);
        assert_eq!(chip.saw.accum, 0);
    }

    // ---------------- Round 386: VRC6 $9003 16x/256x scaling ----------------
    //
    // Spec source: `docs/audio/nsf/vrc6-audio-wiki.html`
    //   §"Frequency Control ($9003)" — "B - 16x frequency, all
    //   oscillators (4 octave increase)", "A - 256x frequency, all
    //   oscillators (8 octave increase)", "The 256x flag overrides the
    //   16x flag.", and "The 16x/256x flags effectively control a
    //   4-bit and 8-bit right shift of the 12-bit period registers."
    //
    // Previously the flags were consumed as a batching chunk size, so
    // the divider still counted one step per CPU cycle and the 16x /
    // 256x speed-up never happened at all.

    /// Count duty-generator steps of pulse 1 over `cycles` CPU cycles.
    fn vrc6_pulse_steps_over(chip: &mut Vrc6, cycles: u32) -> u32 {
        let mut steps = 0u32;
        let mut prev = chip.pulse[0].step;
        for _ in 0..cycles {
            chip.tick(1);
            if chip.pulse[0].step != prev {
                steps += 1;
                prev = chip.pulse[0].step;
            }
        }
        steps
    }

    #[test]
    fn vrc6_9003_16x_flag_shifts_period_right_four_bits() {
        // Period t = $0FF. Normal rate: one duty step per (t+1) = 256
        // cycles. With B set the divider reloads with (t >> 4) = $0F,
        // i.e. one step per 16 cycles — 16x frequency.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x0F); // vol=15, duty=0
        chip.write(0x9001, 0xFF); // period low = $FF
        chip.write(0x9002, 0x80); // E=1, period high = 0
        assert_eq!(vrc6_pulse_steps_over(&mut chip, 2560), 10, "1x baseline");

        let mut fast = Vrc6::new();
        fast.write(0x9000, 0x0F);
        fast.write(0x9001, 0xFF);
        fast.write(0x9002, 0x80);
        fast.write(0x9003, 0b0000_0010); // B = 16x
        assert_eq!(
            vrc6_pulse_steps_over(&mut fast, 2560),
            160,
            "16x flag → duty steps once per (t>>4)+1 = 16 cycles"
        );
    }

    #[test]
    fn vrc6_9003_256x_flag_overrides_16x() {
        // A=1 (with B=1 too): §"The 256x flag overrides the 16x flag."
        // Period t = $2FF → reload (t >> 8) = 2, one step per 3 cycles.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x0F);
        chip.write(0x9001, 0xFF);
        chip.write(0x9002, 0x82); // E=1, period high = 2 → t = $2FF
        chip.write(0x9003, 0b0000_0110); // A=1, B=1 → 256x wins
        assert_eq!(
            vrc6_pulse_steps_over(&mut chip, 300),
            100,
            "256x flag → duty steps once per (t>>8)+1 = 3 cycles"
        );
    }

    #[test]
    fn vrc6_9003_scaling_also_drives_the_sawtooth() {
        // §"Frequency Control ($9003)": the flags scale "all
        // oscillators". Saw period t = $0FF, A rate $08; with the 16x
        // flag a full 14-step saw cycle takes 14 * ((t>>4)+1) = 224
        // cycles instead of 14 * 256.
        let mut chip = Vrc6::new();
        chip.write(0xB000, 0x08);
        chip.write(0xB001, 0xFF); // period low = $FF
        chip.write(0xB002, 0x80); // E=1, period high = 0
        chip.write(0x9003, 0b0000_0010); // 16x
                                         // Walk exactly one full 14-step cycle: 14 * 16 = 224 cycles.
        chip.tick(224);
        assert_eq!(chip.saw.step, 0, "14 steps in 224 cycles at 16x");
        // Accumulator was reset on re-reaching step 0.
        assert_eq!(chip.saw.accum, 0);
        // Half a cycle later we sit at step 7 with 4 adds applied.
        chip.tick(112);
        assert_eq!(chip.saw.step, 7);
        assert_eq!(chip.saw.accum, 0x08 * 3);
    }

    // ---------------- Round 290: VRC6 pulse duty generator -----------------
    //
    // Spec source: `docs/audio/nsf/vrc6-audio-wiki.html`
    //   §"Pulse Channels" — "The duty cycle generator takes 16 steps,
    //                        counting down from 15 to 0. When the
    //                        current step is less than or equal to the
    //                        given duty cycle D, the channel volume V
    //                        is output, otherwise 0 is output. When the
    //                        mode bit M is true, the channel ignores
    //                        the duty cycle generator and outputs the
    //                        current volume regardless of the current
    //                        duty."
    //   §"Pulse Channels" — "When the channel is disabled by clearing
    //                        the E bit, output is forced to 0, and the
    //                        duty cycle is immediately reset and
    //                        halted; it will resume from the beginning
    //                        when E is once again set."

    /// Advance the pulse duty generator by exactly one step and return
    /// the new step value. The chip ticks `timer_period + 1` cycles per
    /// step (the +1 absorbs the "0 → reload + advance" tick), with
    /// `freq_shift = 0` so one CPU cycle == one divider tick.
    fn vrc6_pulse_step_once(chip: &mut Vrc6, idx: usize) -> u8 {
        let period = chip.pulse[idx].timer_period;
        chip.tick(period as u32 + 1);
        chip.pulse[idx].step
    }

    #[test]
    fn vrc6_pulse_duty_generator_counts_down_from_fifteen() {
        // §"Pulse Channels": the 16-step generator counts down 15→0
        // and wraps. A fresh chip seeds step at 15; each timer expiry
        // decrements it, wrapping 0 → 15.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x0F); // vol=15, duty=0, M=0
        chip.write(0x9001, 0x02); // period low = 2 (short, deterministic)
        chip.write(0x9002, 0x80); // E=1, period high = 0
        assert_eq!(chip.pulse[0].step, 15, "fresh generator starts at 15");
        let walk: Vec<u8> = (0..17)
            .map(|_| vrc6_pulse_step_once(&mut chip, 0))
            .collect();
        assert_eq!(
            walk,
            vec![14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 15, 14],
            "step counts down 15→0 then wraps back to 15"
        );
    }

    #[test]
    fn vrc6_pulse_duty_ratio_is_d_plus_one_steps_high() {
        // §"Pulse Channels": "When the current step is less than or
        // equal to the given duty cycle D, the channel volume V is
        // output." With D=3 (25 % per the §duty table 4/16), exactly
        // steps 0,1,2,3 of the 16-step cycle output volume — 4 of 16.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x3F); // M=0, duty=3, vol=15
        chip.write(0x9001, 0x02);
        chip.write(0x9002, 0x80);
        let mut high = 0usize;
        // Sample all 16 phases of the duty cycle.
        for _ in 0..16 {
            if chip.output() > 0.0 {
                high += 1;
            }
            let _ = vrc6_pulse_step_once(&mut chip, 0);
        }
        assert_eq!(high, 4, "duty D=3 → 4/16 steps high (25 %)");
    }

    #[test]
    fn vrc6_pulse_mode_bit_outputs_full_volume_ignoring_duty() {
        // §"Pulse Channels": "When the mode bit M is true, the channel
        // ignores the duty cycle generator and outputs the current
        // volume regardless of the current duty." (§duty table M row
        // = 16/16 = 100 %.) Even with duty=0 (the narrowest setting),
        // M=1 outputs volume at every one of the 16 phases.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x8F); // M=1, duty=0, vol=15
        chip.write(0x9001, 0x02);
        chip.write(0x9002, 0x80);
        for _ in 0..16 {
            assert!(chip.output() > 0.0, "M-mode outputs volume at every phase");
            let _ = vrc6_pulse_step_once(&mut chip, 0);
        }
    }

    #[test]
    fn vrc6_pulse_e_clear_resets_duty_generator_to_beginning() {
        // §"Pulse Channels": "When the channel is disabled by clearing
        // the E bit, output is forced to 0, and the duty cycle is
        // immediately reset and halted; it will resume from the
        // beginning when E is once again set." The duty generator's
        // "beginning" is the top of the 15→0 countdown (step 15).
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x7F); // M=0, duty=7 (50 %), vol=15
        chip.write(0x9001, 0x04);
        chip.write(0x9002, 0x80); // E=1
                                  // Walk a few steps so the generator is mid-countdown.
        for _ in 0..5 {
            let _ = vrc6_pulse_step_once(&mut chip, 0);
        }
        assert_ne!(chip.pulse[0].step, 15, "generator has walked off 15");
        // Falling edge on E forces output to 0 and resets the duty
        // generator to the beginning (step 15).
        chip.write(0x9002, 0x00); // E=0
        assert!(!chip.pulse[0].enabled);
        assert_eq!(chip.pulse[0].step, 15, "duty generator reset to beginning");
        assert_eq!(chip.output(), 0.0, "disabled pulse outputs 0");
        // Re-enabling resumes from the beginning (step 15) per the
        // "resume from the beginning when E is once again set" rule.
        chip.write(0x9002, 0x80); // E=1
        assert_eq!(chip.pulse[0].step, 15);
    }

    #[test]
    fn vrc6_pulse_e_clear_phase_reset_matches_2a03_style_technique() {
        // §"Pulse Channels": "Thus it is possible to reset phase by
        // clearing and immediately setting E." A clear-then-set pair
        // must land the generator at a deterministic phase (15)
        // independent of where it was when E was cleared.
        let mut chip = Vrc6::new();
        chip.write(0x9000, 0x5F); // duty=5, vol=15
        chip.write(0x9001, 0x08);
        chip.write(0x9002, 0x80); // E=1
        for _ in 0..9 {
            let _ = vrc6_pulse_step_once(&mut chip, 0);
        }
        // Clear + immediately set E → phase pinned at 15.
        chip.write(0x9002, 0x00);
        chip.write(0x9002, 0x80);
        assert_eq!(chip.pulse[0].step, 15);
    }

    #[test]
    fn mmc5_pulse_status_reports_active_lengths() {
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x03); // enable both
        chip.write(0x5003, 0x08); // pulse 0 length set
        let s = chip.read(0x5015);
        assert_eq!(s & 0x03, 0x01);
    }

    // ---------------- Round 18: MMC5 PCM IRQ + read-mode tests ----------------
    //
    // Spec source: `docs/audio/nsf/mmc5-audio-wiki.html`
    //   §"PCM Mode/IRQ ($5010)"  — write/read bit layout.
    //   §"Raw PCM ($5011)"       — write-only + write-by-read.
    //   §"PCM description"       — DAC update + $00 → IRQ side effect.
    //   §"IRQ operation"         — pseudocode for irqTrip / irqEnable.

    #[test]
    fn mmc5_5010_write_decodes_irq_enable_and_mode_bits() {
        // §"PCM Mode/IRQ ($5010)" write layout: Ixxx xxxM.
        let mut chip = Mmc5::new();
        chip.write(0x5010, 0x80); // I=1, M=0
        assert!(chip.irq_enable);
        assert!(!chip.pcm_read_mode);
        chip.write(0x5010, 0x01); // I=0, M=1
        assert!(!chip.irq_enable);
        assert!(chip.pcm_read_mode);
        chip.write(0x5010, 0x81); // both set
        assert!(chip.irq_enable);
        assert!(chip.pcm_read_mode);
    }

    #[test]
    fn mmc5_5011_zero_write_sets_irq_trip_without_changing_dac() {
        // §"Raw PCM ($5011)": "Writing $00 to this register will have
        // no effect on the output sound, and does not change the PCM
        // counter." §"PCM description": "If you try to assign a value
        // of $00, the DAC is not changed; an IRQ is generated instead."
        let mut chip = Mmc5::new();
        chip.write(0x5011, 0xC0); // seed DAC.
        assert_eq!(chip.pcm, 0xC0);
        assert!(!chip.irq_trip);
        chip.write(0x5011, 0x00); // zero write → trip.
        assert_eq!(chip.pcm, 0xC0, "DAC must NOT change on $00 write");
        assert!(chip.irq_trip);
    }

    #[test]
    fn mmc5_5011_nonzero_write_updates_dac_and_clears_irq_trip() {
        // §"IRQ operation" pseudocode: non-zero write → irqTrip = 0,
        // DAC = value.
        let mut chip = Mmc5::new();
        chip.write(0x5011, 0x00); // arm trip first.
        assert!(chip.irq_trip);
        chip.write(0x5011, 0x7F);
        assert_eq!(chip.pcm, 0x7F);
        assert!(!chip.irq_trip);
    }

    #[test]
    fn mmc5_pin2_dac_curve_matches_spec_equation() {
        // §"Pin 2 DAC Characteristic": Voltage =
        //   [(DAC/255)·(0.4·AVcc)] + (0.1·AVcc).
        // pin2_dac_ac returns the AC-coupled value = absolute pin
        // voltage (in AVcc units) minus the 0.3·AVcc DC midpoint. Drive
        // the full §-quoted equation independently and compare.
        let spec_ac = |dac: u8| ((dac as f32 / 255.0) * 0.4 + 0.1) - 0.3;
        for &dac in &[0u8, 1, 64, 127, 128, 200, 255] {
            let got = pin2_dac_ac(dac);
            let want = spec_ac(dac);
            assert!(
                (got - want).abs() < 1e-6,
                "DAC {dac:#04x}: got {got}, spec {want}"
            );
        }
        // Endpoints: DAC=0 sits at the bottom of the swing (−0.2·AVcc),
        // DAC=255 at the top (+0.2·AVcc); the curve is symmetric about
        // the 127.5 midpoint.
        assert!((pin2_dac_ac(0) - (-0.2)).abs() < 1e-6);
        assert!((pin2_dac_ac(255) - 0.2).abs() < 1e-6);
        assert!((pin2_dac_ac(0) + pin2_dac_ac(255)).abs() < 1e-6);
    }

    #[test]
    fn mmc5_5011_write_ignored_in_read_mode() {
        // §"Raw PCM ($5011)": "Write-by-read writes to this register
        // in PCM read-mode" — i.e. the CPU's direct $5011 write path
        // is inert; updates only arrive via observe_prg_read.
        let mut chip = Mmc5::new();
        chip.write(0x5010, 0x01); // read mode on.
        chip.write(0x5011, 0x55);
        assert_eq!(chip.pcm, 0x00, "$5011 write must be ignored in read mode");
        assert!(
            !chip.irq_trip,
            "and must not trip the IRQ either (the wiki's write path is gated on !read_mode)"
        );
    }

    #[test]
    fn mmc5_5010_read_returns_irq_status_and_clears_trip() {
        // §"IRQ operation" pseudocode (on $5010 read):
        //   value.bit7 = (irqTrip AND irqEnable);
        //   irqTrip = 0;
        let mut chip = Mmc5::new();
        chip.write(0x5010, 0x80); // enable IRQ.
        chip.write(0x5011, 0x00); // trip.
        assert!(chip.irq_trip);
        let v = chip.read(0x5010);
        assert_eq!(v & 0x80, 0x80, "bit 7 reads back the asserted IRQ");
        assert!(
            !chip.irq_trip,
            "read of $5010 acknowledges + clears the trip"
        );
        // A second read with no new trip reads back 0.
        let v2 = chip.read(0x5010);
        assert_eq!(v2 & 0x80, 0x00);
    }

    #[test]
    fn mmc5_5010_read_masks_with_irq_enable() {
        // §"IRQ operation": value.bit7 = (irqTrip AND irqEnable).
        // A trip without an enabled IRQ reads back zero (and the
        // cleared-on-read semantics still apply).
        let mut chip = Mmc5::new();
        chip.write(0x5010, 0x00); // I=0, M=0
        chip.write(0x5011, 0x00); // trip.
        let v = chip.read(0x5010);
        assert_eq!(
            v & 0x80,
            0x00,
            "disabled IRQ must not surface in the readback"
        );
        assert!(!chip.irq_trip, "read still acknowledges + clears the trip");
    }

    #[test]
    fn mmc5_5010_read_mirrors_mode_bit() {
        // §"PCM Mode/IRQ ($5010)" read note + MMC5A default power-on
        // read value = $01 — read mode reads back bit 0 = 1.
        let mut chip = Mmc5::new();
        chip.write(0x5010, 0x01); // read mode.
        assert_eq!(chip.read(0x5010) & 0x01, 0x01);
        chip.write(0x5010, 0x00); // write mode.
        assert_eq!(chip.read(0x5010) & 0x01, 0x00);
    }

    #[test]
    fn mmc5_irq_line_tracks_trip_and_enable() {
        // §"IRQ operation" final line: Cart IRQ line = (irqTrip AND
        // irqEnable). Validates `Mmc5::irq_line()` against the four
        // truth-table cells.
        let mut chip = Mmc5::new();
        assert!(!chip.irq_line());
        chip.write(0x5011, 0x00); // trip, but I=0
        assert!(!chip.irq_line());
        chip.write(0x5010, 0x80); // enable IRQ
        assert!(chip.irq_line());
        let _ = chip.read(0x5010); // ack
        assert!(!chip.irq_line());
        // Drop enable while a trip is pending — line must drop too.
        chip.write(0x5011, 0x00);
        assert!(chip.irq_line());
        chip.write(0x5010, 0x00); // disable IRQ (I=0)
        assert!(!chip.irq_line());
    }

    #[test]
    fn mmc5_observe_prg_read_updates_dac_in_read_mode_only() {
        // §"Raw PCM ($5011)": "Write-by-read writes to this register
        // in PCM read-mode" — the bus's `$8000..=$BFFF` read path is
        // routed through `observe_prg_read`. Outside read-mode it's
        // inert; inside read-mode it updates the DAC + clears trip on
        // non-zero, sets trip on zero.
        let mut chip = Mmc5::new();
        chip.enabled = true;
        // Write mode: observe_prg_read is a no-op.
        chip.observe_prg_read(0x42);
        assert_eq!(chip.pcm, 0x00);
        assert!(!chip.irq_trip);
        // Read mode + non-zero → DAC updates, trip stays clear.
        chip.write(0x5010, 0x01);
        chip.observe_prg_read(0x42);
        assert_eq!(chip.pcm, 0x42);
        assert!(!chip.irq_trip);
        // Read mode + zero → trip set, DAC unchanged.
        chip.observe_prg_read(0x00);
        assert_eq!(chip.pcm, 0x42, "DAC must NOT change on $00 byte");
        assert!(chip.irq_trip);
        // A subsequent non-zero byte clears trip and updates DAC.
        chip.observe_prg_read(0x80);
        assert_eq!(chip.pcm, 0x80);
        assert!(!chip.irq_trip);
    }

    #[test]
    fn mmc5_observe_prg_read_inert_when_chip_disabled() {
        // The bus only routes the write-by-read when MMC5 is enabled
        // in the expansion mask; `Mmc5::observe_prg_read` guards on
        // its own `enabled` flag as defence-in-depth.
        let mut chip = Mmc5::new();
        chip.enabled = false;
        chip.write(0x5010, 0x01); // read mode
        chip.observe_prg_read(0x42);
        assert_eq!(chip.pcm, 0x00);
        assert!(!chip.irq_trip);
    }

    #[test]
    fn expansion_irq_line_surfaces_mmc5_pcm_irq() {
        // `Expansion::irq_line()` must OR the MMC5 PCM IRQ line into
        // the bus's view per §"IRQ operation" — without it the APU's
        // own irq_line() would never see the MMC5 contribution.
        let mut ex = Expansion::new();
        // bit 3 (0x08) = MMC5 per Kevtris v1.61 spec §expansion bits.
        ex.set_flags(crate::header::ExpansionChips(0x08));
        assert!(ex.mmc5.enabled);
        assert!(!ex.irq_line());
        // Enable IRQ + trip via $5010 + $5011.
        ex.write(0x5010, 0x80);
        ex.write(0x5011, 0x00);
        assert!(ex.irq_line());
        // Ack via $5010 read.
        let _ = ex.read(0x5010);
        assert!(!ex.irq_line());
    }

    #[test]
    fn expansion_observe_prg_read_window_is_8000_to_bfff() {
        // §"PCM description": "MMC5's DAC is changed either by
        // writing a value to $5011 (in write mode) or reading a value
        // from $8000-BFFF (in read mode)." The bus's hook must
        // restrict the side effect to that window — reads of
        // $C000..=$FFFF are also PRG-ROM reads, but the wiki window
        // stops at $BFFF.
        let mut ex = Expansion::new();
        ex.set_flags(crate::header::ExpansionChips(0x08));
        ex.write(0x5010, 0x01); // read mode
        ex.observe_prg_read(0xC000, 0x55); // outside window → no DAC change
        assert_eq!(ex.mmc5.pcm, 0x00);
        ex.observe_prg_read(0x8000, 0x77); // start of window
        assert_eq!(ex.mmc5.pcm, 0x77);
        ex.observe_prg_read(0xBFFF, 0x33); // end of window inclusive
        assert_eq!(ex.mmc5.pcm, 0x33);
        ex.observe_prg_read(0xC000, 0x11); // just outside
        assert_eq!(ex.mmc5.pcm, 0x33);
    }

    // ---------------- Round 294: MMC5 240 Hz envelope + length ----------------
    //
    // Spec source: `docs/audio/nsf/mmc5-audio-wiki.html` §"Pulse 1
    // ($5000-$5003)":
    //   * "$5001 is not implemented" (no sweep).
    //   * "Frequency values less than 8 do not silence the MMC5 pulse
    //     channels; they can output ultrasonic frequencies."
    //   * "Length counter operates twice as fast as the APU length
    //     counter (might be clocked at the envelope rate)."
    //   * "MMC5 does not have an equivalent frame sequencer (APU
    //     $4017); envelope and length counter are fixed to a 240hz
    //     update rate."
    //   * "Other features such as the envelope and phase reset are the
    //     same as their APU counterparts."

    /// One MMC5 240 Hz envelope/length step is `MMC5_FRAME_CPU` CPU
    /// cycles; this drives exactly `n` of them.
    fn mmc5_frame_steps(chip: &mut Mmc5, n: u32) {
        for _ in 0..n {
            chip.tick(MMC5_FRAME_CPU);
        }
    }

    #[test]
    fn mmc5_length_loads_from_apu_table_not_raw_index() {
        // §"Pulse 1": length counter is "the same as their APU
        // counterparts" — a $5003 write loads LENGTH_TABLE[value>>3],
        // not the raw 5-bit index. value=$08 → index 1 → 254.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01); // enable pulse 0
        chip.write(0x5003, 0x08);
        assert_eq!(chip.pulse[0].length, 254);
        // index 0 → 10.
        chip.write(0x5003, 0x00);
        assert_eq!(chip.pulse[0].length, 10);
    }

    #[test]
    fn mmc5_length_load_ignored_when_channel_disabled() {
        // APU-identical: a length-register write while the channel is
        // disabled in $5015 does not load the counter.
        let mut chip = Mmc5::new();
        chip.write(0x5003, 0x08); // pulse 0 still disabled
        assert_eq!(chip.pulse[0].length, 0);
    }

    #[test]
    fn mmc5_length_counts_down_at_240hz_until_silent() {
        // The length counter decrements once per MMC5_FRAME_CPU cycles
        // and silences the channel when it reaches 0. index 3 → 2 is
        // the shortest non-trivial entry, so 2 ticks exhausts it.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x1F); // constant volume 15, halt CLEAR
        chip.write(0x5002, 0x40); // period > 0 so duty step can be high
        chip.write(0x5003, 0x18); // index 3 → length 2
        assert_eq!(chip.pulse[0].length, 2);
        mmc5_frame_steps(&mut chip, 1);
        assert_eq!(chip.pulse[0].length, 1);
        mmc5_frame_steps(&mut chip, 1);
        assert_eq!(chip.pulse[0].length, 0);
        // Expired length → channel silent regardless of duty step.
        chip.pulse[0].step = 1; // a high duty step
        assert_eq!(chip.pulse[0].pulse_output(), 0);
    }

    #[test]
    fn mmc5_length_halt_freezes_counter() {
        // §"Pulse 1": halt bit ($5000 bit 5) is "the same as their APU
        // counterparts" — it freezes the length counter.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x3F); // halt SET (bit 5), constant vol 15
        chip.write(0x5003, 0x18); // length 2
        mmc5_frame_steps(&mut chip, 10);
        assert_eq!(chip.pulse[0].length, 2, "halt must freeze the counter");
    }

    #[test]
    fn mmc5_disabling_channel_clears_length() {
        // §"Status ($5015)" "analogous to the APU Status register":
        // clearing the enable bit zeroes the length counter.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5003, 0x08); // length 254
        assert_eq!(chip.pulse[0].length, 254);
        chip.write(0x5015, 0x00); // disable
        assert_eq!(chip.pulse[0].length, 0);
    }

    #[test]
    fn mmc5_envelope_decays_at_240hz() {
        // §"Pulse 1": envelope is "the same as their APU counterparts".
        // With the constant bit clear and a fast period (volume=0 →
        // divider reloads to 0 → decay drops every tick), the decay
        // ladder walks 15,14,13,… one step per 240 Hz clock.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x00); // constant CLEAR, halt CLEAR, env period (volume)=0
        chip.write(0x5003, 0x08); // env_start + long length
                                  // First clock consumes env_start (decay→15).
        mmc5_frame_steps(&mut chip, 1);
        assert_eq!(chip.pulse[0].env_decay, 15);
        mmc5_frame_steps(&mut chip, 1);
        assert_eq!(chip.pulse[0].env_decay, 14);
        mmc5_frame_steps(&mut chip, 5);
        assert_eq!(chip.pulse[0].env_decay, 9);
    }

    #[test]
    fn mmc5_envelope_period_slows_decay() {
        // A non-zero envelope period (the volume nibble) divides the
        // 240 Hz clock: volume=3 → divider counts 3,2,1,0 before each
        // decay step, so the decay drops every 4th tick.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x03); // constant CLEAR, volume/period = 3
        chip.write(0x5003, 0x08); // env_start
        mmc5_frame_steps(&mut chip, 1); // env_start → decay 15, divider 3
        assert_eq!(chip.pulse[0].env_decay, 15);
        mmc5_frame_steps(&mut chip, 3); // divider 3→2→1→0, decay unchanged
        assert_eq!(chip.pulse[0].env_decay, 15);
        mmc5_frame_steps(&mut chip, 1); // divider 0 → reload, decay 14
        assert_eq!(chip.pulse[0].env_decay, 14);
    }

    #[test]
    fn mmc5_envelope_loops_when_halt_set() {
        // The shared halt/loop bit ($5000 bit 5) makes the envelope
        // wrap 0 → 15 instead of staying at 0 — APU-identical loop.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x20); // constant CLEAR, halt/loop SET, period 0
        chip.write(0x5003, 0x08); // env_start
                                  // Walk decay down to 0 (15 steps after the start-consuming one).
        mmc5_frame_steps(&mut chip, 1 + 15);
        assert_eq!(chip.pulse[0].env_decay, 0);
        mmc5_frame_steps(&mut chip, 1); // loop wraps back to 15
        assert_eq!(chip.pulse[0].env_decay, 15);
    }

    #[test]
    fn mmc5_constant_bit_selects_volume_over_envelope() {
        // §"Pulse 1" constant-volume path: bit 4 set → fixed `volume`;
        // clear → envelope decay level.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5002, 0x40); // non-zero period
                                  // Constant volume 7.
        chip.write(0x5000, 0x17); // constant SET, volume 7
        chip.write(0x5003, 0x08); // arm length (resets duty step to 0)
        chip.pulse[0].step = 1; // a high step for duty 0
        assert_eq!(chip.pulse[0].pulse_output(), 7);
        // Envelope mode: output tracks the decay level (15 after start).
        chip.write(0x5000, 0x07); // constant CLEAR, volume(period) 7
        chip.write(0x5003, 0x08); // resets duty step to 0, arms env_start
        mmc5_frame_steps(&mut chip, 1); // env_start → decay 15
        chip.pulse[0].step = 1; // high step (after the timer-advancing tick)
        assert_eq!(chip.pulse[0].pulse_output(), 15);
    }

    #[test]
    fn mmc5_sub_eight_period_is_not_silenced() {
        // §"Pulse 1": "Frequency values less than 8 do not silence the
        // MMC5 pulse channels" — unlike the 2A03, a period below 8 must
        // still emit on a high duty step.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x1F); // constant volume 15
        chip.write(0x5002, 0x02); // period low = 2 (< 8)
        chip.write(0x5003, 0x08); // period hi 0, arm length
        chip.pulse[0].step = 1; // high step for duty 0
        assert_eq!(
            chip.pulse[0].pulse_output(),
            15,
            "sub-8 period must NOT silence the MMC5 pulse"
        );
    }

    #[test]
    fn mmc5_output_polarity_is_reversed_relative_to_the_apu() {
        // Doc intro: "the polarity of all MMC5 channels is reversed
        // compared to the APU". A pulse going high (which raises the
        // 2A03 mixer output) must LOWER the MMC5 contribution, and a
        // rising PCM byte (which raises the Pin 2 voltage) must lower
        // it too — deltas measured against the same chip idle, since
        // the AC-coupled PCM midpoint offset shifts the absolute
        // level.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x1F); // constant volume 15
        chip.write(0x5002, 0x40);
        chip.write(0x5003, 0x08); // arm length
        chip.pulse[0].step = 0; // low duty step → idle baseline
        let idle = chip.output();
        chip.pulse[0].step = 1; // high duty step
        assert!(
            chip.output() < idle,
            "pulse going high must swing the MMC5 output DOWN"
        );
        // PCM: a rising DAC byte lowers the (inverted) output.
        let mut pcm = Mmc5::new();
        pcm.write(0x5011, 0x01);
        let low = pcm.output();
        pcm.write(0x5011, 0xFF);
        assert!(
            pcm.output() < low,
            "rising PCM byte must swing the MMC5 output DOWN"
        );
    }

    #[test]
    fn mmc5_length_write_restarts_envelope() {
        // "phase reset … the same as their APU counterparts": a
        // $5003 write sets env_start so the next clock reloads decay
        // to 15, even mid-decay.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x00); // envelope mode, period 0
        chip.write(0x5003, 0x08);
        mmc5_frame_steps(&mut chip, 5); // decay walks down below 15
        assert!(chip.pulse[0].env_decay < 15);
        chip.write(0x5003, 0x08); // re-arm → env_start
        mmc5_frame_steps(&mut chip, 1);
        assert_eq!(chip.pulse[0].env_decay, 15);
    }

    #[test]
    fn mmc5_240hz_clock_needs_full_period() {
        // The envelope/length clock only fires once MMC5_FRAME_CPU CPU
        // cycles have accumulated; a partial tick must not advance it.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5000, 0x00);
        chip.write(0x5003, 0x18); // length 2
        chip.tick(MMC5_FRAME_CPU - 1); // one cycle short
        assert_eq!(chip.pulse[0].length, 2, "sub-period tick must not step");
        chip.tick(1); // crosses the boundary
        assert_eq!(chip.pulse[0].length, 1);
    }

    // ---------------- Round 386: MMC5 pulse timer rate ----------------
    //
    // Spec source: `docs/audio/nsf/mmc5-audio-wiki.html` — the pulse
    // channels "behave almost identically to the native pulse channels
    // in the NES APU" and "function the same as to those found in the
    // NES APU except for the following differences" (none of which
    // touch the timer). The APU pulse timer counts APU (CPU/2) cycles
    // and clocks the 8-step duty sequencer on expiry, so one duty step
    // takes 2*(t+1) CPU cycles (f = CPU / (16*(t+1))). The old tick
    // decremented the timer once per CPU cycle — every MMC5 pulse
    // played roughly an octave sharp — and dropped odd cycles at batch
    // edges.

    /// Tick one CPU cycle at a time until pulse 0's duty step changes;
    /// returns how many CPU cycles that took.
    fn mmc5_cpu_cycles_to_next_duty_step(chip: &mut Mmc5) -> u32 {
        let start = chip.pulse[0].step;
        let mut cycles = 0u32;
        while chip.pulse[0].step == start {
            chip.tick(1);
            cycles += 1;
            assert!(cycles < 10_000, "duty sequencer never stepped");
        }
        cycles
    }

    #[test]
    fn mmc5_pulse_duty_steps_every_two_t_plus_one_cpu_cycles() {
        // t = 0x40 → one duty step per 2*(t+1) = 130 CPU cycles
        // (f = CPU / (16*(t+1)) across the 8-step sequence). The
        // power-up timer is empty, so the very first APU cycle
        // reloads + steps; every following step costs 130.
        let mut chip = Mmc5::new();
        chip.write(0x5015, 0x01);
        chip.write(0x5002, 0x40);
        chip.write(0x5003, 0x00); // timer_period high = 0 (+ length load)
        let _first = mmc5_cpu_cycles_to_next_duty_step(&mut chip);
        for _ in 0..4 {
            assert_eq!(
                mmc5_cpu_cycles_to_next_duty_step(&mut chip),
                2 * (0x40 + 1),
                "APU-identical rate: 2*(t+1) CPU cycles per duty step"
            );
        }
    }

    #[test]
    fn mmc5_pulse_prescaler_carries_odd_cycles_across_batches() {
        // The same total cycle count driven whole and in ragged
        // 1/3/5/7-cycle chunks must land the pulse timers, duty
        // steps, prescaler carry, and 240 Hz accumulator in exactly
        // the same state.
        let setup = |chip: &mut Mmc5| {
            chip.write(0x5015, 0x03);
            chip.write(0x5000, 0x10); // constant volume
            chip.write(0x5002, 0x40);
            chip.write(0x5003, 0x08); // length + phase reset
            chip.write(0x5006, 0x33);
            chip.write(0x5007, 0x10);
        };
        let mut whole = Mmc5::new();
        let mut ragged = Mmc5::new();
        setup(&mut whole);
        setup(&mut ragged);

        const TOTAL: u32 = 30_011; // odd + spans several 240 Hz events
        whole.tick(TOTAL);
        let chunks = [1u32, 3, 5, 7];
        let mut left = TOTAL;
        let mut i = 0usize;
        while left > 0 {
            let n = chunks[i % chunks.len()].min(left);
            ragged.tick(n);
            left -= n;
            i += 1;
        }
        for ch in 0..2 {
            assert_eq!(whole.pulse[ch].timer, ragged.pulse[ch].timer);
            assert_eq!(whole.pulse[ch].step, ragged.pulse[ch].step);
            assert_eq!(whole.pulse[ch].length, ragged.pulse[ch].length);
            assert_eq!(whole.pulse[ch].env_decay, ragged.pulse[ch].env_decay);
        }
        assert_eq!(whole.frame_acc, ragged.frame_acc);
        assert_eq!(whole.pulse_prescaler_carry, ragged.pulse_prescaler_carry);
    }

    #[test]
    fn mmc5_timer_runs_while_channel_disabled() {
        // As on the 2A03, `$5015` gates the length counter / output,
        // not the timer: a disabled channel's sequencer keeps moving.
        let mut chip = Mmc5::new();
        chip.write(0x5002, 0x10); // t = 0x10, channel stays disabled
        let before = chip.pulse[0].step;
        chip.tick(2 * (0x10 + 1) * 3);
        assert_ne!(chip.pulse[0].step, before);
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
    fn s5b_select_high_nibble_disables_data_port_writes() {
        // §"Audio Register Select ($C000-$DFFF)": `DDDDRRRR` — a
        // nonzero high nibble "Disable writes to $E000 if nonzero".
        // The low nibble still selects the register, but the
        // following data-port write must be ignored.
        let mut chip = Sunsoft5b::new();
        // Establish a known value with writes enabled.
        chip.write(0xC000, 0x00); // select R0, writes enabled
        chip.write(0xE000, 0x42);
        assert_eq!(chip.regs[0], 0x42);
        assert!(!chip.writes_disabled);
        // Now select R0 again but with a nonzero high nibble.
        chip.write(0xC000, 0x10); // select R0, writes DISABLED
        assert!(chip.writes_disabled);
        assert_eq!(chip.addr, 0x00, "low nibble still selects R0");
        chip.write(0xE000, 0xFF); // must be ignored
        assert_eq!(chip.regs[0], 0x42, "data-port write ignored while disabled");
        assert_eq!(
            chip.channels[0].timer_period, 0x0042,
            "register-derived state unchanged while disabled"
        );
    }

    #[test]
    fn s5b_select_zero_high_nibble_reenables_data_port_writes() {
        // A later select write with a zero high nibble clears the
        // disable and the data port works again.
        let mut chip = Sunsoft5b::new();
        chip.write(0xC000, 0xF3); // select R3, writes DISABLED
        assert!(chip.writes_disabled);
        chip.write(0xE000, 0xAA); // ignored
        assert_eq!(chip.regs[3], 0x00);
        chip.write(0xC000, 0x03); // select R3, writes ENABLED
        assert!(!chip.writes_disabled);
        chip.write(0xE000, 0x07); // honoured
        assert_eq!(chip.regs[3], 0x07);
        // R3 is channel B period high (`---- HHHH`); confirm it
        // propagated to the channel-B period derivation.
        assert_eq!(chip.channels[1].timer_period, 0x0700);
    }

    #[test]
    fn s5b_every_nonzero_high_nibble_disables_writes() {
        // The disable is "if nonzero" — exercise each high-nibble
        // value 1..=15 and confirm the data port stays inert, while
        // high nibble 0 is the only value that enables it.
        for high in 0u8..=0x0F {
            let mut chip = Sunsoft5b::new();
            chip.write(0xC000, high << 4); // select R0, high nibble = `high`
            chip.write(0xE000, 0x55);
            if high == 0 {
                assert_eq!(chip.regs[0], 0x55, "high=0 must allow the write");
            } else {
                assert_eq!(chip.regs[0], 0x00, "high={high} must block the write");
            }
        }
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

    // ---------------- Round 386: 16-clock prescaler carry ----------------
    //
    // Spec source: `docs/audio/nsf/sunsoft-5b-audio-wiki.html` §Sound —
    // "a counter that counts up on every 16th clock cycle". A CPU
    // instruction is 2-7 cycles, so per-instruction batches almost
    // never contain a whole 16-clock interval; the old `cycles / 16`
    // truncation dropped the remainder and the whole chip fell silent
    // (or ran arbitrarily slow) while the CPU was executing code.

    #[test]
    fn s5b_single_cycle_ticks_accumulate_across_batches() {
        // 64 one-cycle ticks must behave exactly like one tick(64):
        // with tone period 4 (flip every 64 clocks) the level flips
        // exactly once.
        let mut chip = Sunsoft5b::new();
        s5b_write_reg(&mut chip, 0, 0x04);
        s5b_write_reg(&mut chip, 1, 0x00);
        let start = chip.channels[0].level;
        for _ in 0..64 {
            chip.tick(1);
        }
        assert_eq!(
            chip.channels[0].level,
            start ^ 1,
            "sixty-four 1-cycle batches = one 64-cycle batch"
        );
    }

    #[test]
    fn s5b_state_is_invariant_to_batch_chunking() {
        // Drive two identical chips (tone + noise + envelope all
        // active) for the same total cycle count, one in a single
        // batch and one in ragged 1/3/5/7-cycle chunks; every piece
        // of audible state must match.
        let setup = |chip: &mut Sunsoft5b| {
            s5b_write_reg(chip, 0, 0x07); // tone A period lo
            s5b_write_reg(chip, 1, 0x00);
            s5b_write_reg(chip, 6, 0x03); // noise period
            s5b_write_reg(chip, 0x0B, 0x05); // env period lo
            s5b_write_reg(chip, 0x0C, 0x00);
            s5b_write_reg(chip, 0x0D, 0x0E); // continue+attack+alternate
            s5b_write_reg(chip, 8, 0x10); // ch A: envelope-routed
        };
        let mut whole = Sunsoft5b::new();
        let mut ragged = Sunsoft5b::new();
        setup(&mut whole);
        setup(&mut ragged);

        const TOTAL: u32 = 16_384;
        whole.tick(TOTAL);
        let mut left = TOTAL;
        let chunks = [1u32, 3, 5, 7];
        let mut i = 0usize;
        while left > 0 {
            let n = chunks[i % chunks.len()].min(left);
            ragged.tick(n);
            left -= n;
            i += 1;
        }

        assert_eq!(whole.channels[0].level, ragged.channels[0].level);
        assert_eq!(whole.channels[0].timer, ragged.channels[0].timer);
        assert_eq!(whole.noise.lfsr, ragged.noise.lfsr);
        assert_eq!(whole.noise.timer, ragged.noise.timer);
        assert_eq!(whole.envelope.step, ragged.envelope.step);
        assert_eq!(whole.envelope.timer, ragged.envelope.timer);
        assert_eq!(whole.clock_rem, ragged.clock_rem);
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

    // ---------------- Round 386: FDS wave halt + cycle interleave ----------------
    //
    // Spec source: `docs/audio/nsf/fds-audio-wiki.html`
    //   §Wavetables — "Disabling the wave unit via the high bit of
    //   $4083 immediately resets its accumulator, delaying the next
    //   tick after they are enabled again until the next overflow.
    //   Consequently, this also resets the wave position to 0 (i.e.
    //   the $4040 value)."

    #[test]
    fn fds_4083_bit7_halts_wave_unit_and_resets_accumulator() {
        let mut chip = Fds::new();
        chip.write(0x4082, 0xFF); // pitch low
        chip.write(0x4083, 0x0F); // pitch high = $F → freq = $FFF
        chip.tick(16 * 40); // let the wave accumulate
        assert_ne!(chip.wave_acc, 0);
        assert_ne!(chip.wave_pos, 0);
        // Set the halt bit: accumulator + position reset immediately.
        chip.write(0x4083, 0x80 | 0x0F);
        assert_eq!(chip.wave_acc, 0);
        assert_eq!(chip.wave_pos, 0, "halted wave outputs the $4040 value");
        // While held, the wave unit does not advance.
        chip.tick(16 * 100);
        assert_eq!(chip.wave_acc, 0);
        assert_eq!(chip.wave_pos, 0);
        // Re-enable at a low pitch: the next position change waits
        // for the next overflow out of the 18 fractional bits. With
        // freq $040 (neutral mod → wave_pitch = $040 * $40 = 4096 per
        // unit tick) the first carry into bit 18 needs 65 unit ticks.
        chip.write(0x4082, 0x40);
        chip.write(0x4083, 0x00); // freq = $040, halt released
        chip.tick(16 * 63);
        assert_eq!(chip.wave_pos, 0, "position stays 0 until the overflow");
        chip.tick(16 * 2);
        assert_ne!(chip.wave_pos, 0);
    }

    #[test]
    fn fds_state_is_invariant_to_batch_chunking() {
        // Envelope steps must interleave with the 16-cycle unit ticks
        // in true cycle order: the same total driven whole and in
        // ragged chunks lands every accumulator in the same state.
        // (The old batch shape ran a whole batch's envelope ramping
        // before any of its unit ticks, so a mod-gain step drifted
        // against the §"Modulation unit" pitch formula whenever the
        // batch spanned an envelope expiry.)
        let setup = |chip: &mut Fds| {
            for i in 0..32u16 {
                chip.write(0x4089, 0x80); // wave write enable
                chip.write(0x4040 + i * 2, (i & 0x3F) as u8);
                chip.write(0x4089, 0x00);
            }
            chip.write(0x4085, 0x08); // seed mod counter
            chip.write(0x4088, 0x01); // mod table (while disabled)
            chip.write(0x4088, 0x06);
            chip.write(0x4086, 0xFF); // mod freq low
            chip.write(0x4087, 0x03); // mod freq high, unit ENABLED
            chip.write(0x4084, 0x08); // mod envelope on, decrease, e=8
            chip.write(0x4080, 0x25); // volume envelope on, decrease, e=37
            chip.write(0x408A, 0x10); // master env speed
            chip.write(0x4082, 0x80); // pitch low
            chip.write(0x4083, 0x02); // pitch high, no halt bits
        };
        let mut whole = Fds::new();
        let mut ragged = Fds::new();
        setup(&mut whole);
        setup(&mut ragged);

        const TOTAL: u32 = 50_021;
        whole.tick(TOTAL);
        let chunks = [1u32, 3, 5, 7, 16, 29];
        let mut left = TOTAL;
        let mut i = 0usize;
        while left > 0 {
            let n = chunks[i % chunks.len()].min(left);
            ragged.tick(n);
            left -= n;
            i += 1;
        }
        assert_eq!(whole.wave_acc, ragged.wave_acc);
        assert_eq!(whole.wave_pos, ragged.wave_pos);
        assert_eq!(whole.mod_acc, ragged.mod_acc);
        assert_eq!(whole.mod_pos, ragged.mod_pos);
        assert_eq!(whole.mod_counter, ragged.mod_counter);
        assert_eq!(whole.mod_gain, ragged.mod_gain);
        assert_eq!(whole.volume, ragged.volume);
        assert_eq!(whole.vol_env_timer, ragged.vol_env_timer);
        assert_eq!(whole.mod_env_timer, ragged.mod_env_timer);
        assert_eq!(whole.cycle_acc, ragged.cycle_acc);
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

    // ---------------- Round 386: N163 register windows + sound enable ----------------
    //
    // Spec source: `docs/audio/nsf/namco-163-audio-wiki.html` — the
    // register headings are ranges, not single addresses: §"Sound
    // Enable ($E000-E7FF)" (bit 6 "Disables sound if set"; "Sound is
    // enabled on the 163 by writing a clear bit 6 to this register"),
    // §"Address Port ($F800-$FFFF)", §"Data Port ($4800-$4FFF)".
    // Previously only the exact base addresses $F800 / $4800 decoded
    // and the sound-enable register did not exist at all.

    #[test]
    fn n163_address_and_data_ports_decode_their_full_windows() {
        let mut chip = N163::new();
        chip.write(0xFFFF, 0x80 | 0x10); // top of the $F800-$FFFF window
        assert_eq!(chip.addr, 0x10);
        chip.write(0x4FFF, 0xAB); // top of the $4800-$4FFF window
        assert_eq!(chip.ram[0x10], 0xAB);
        assert_eq!(chip.addr, 0x11, "auto-increment through the mirror");
        chip.write(0xF923, 0x10); // mid-window address write, back to $10
        assert_eq!(chip.read(0x4A00), 0xAB, "mid-window data read");
    }

    #[test]
    fn n163_e000_bit6_disables_sound() {
        let mut chip = N163::new();
        chip.enabled = true;
        n163_setup_channel(&mut chip, 8, 0, 15);
        // Give channel 8 a non-zero frequency so ticking moves phase.
        let base = N163::chan_base(8);
        chip.ram[base] = 0x40;
        chip.tick(15);
        let phase_before = chip.ram[base + 1];
        assert_ne!(phase_before, 0, "sanity: enabled chip advances phase");

        chip.write(0xE000, 0x40); // bit 6 set → sound disabled
        assert!(chip.sound_disabled);
        assert_eq!(chip.output(), 0.0, "disabled chip is silent");
        chip.tick(150);
        assert_eq!(
            chip.ram[base + 1],
            phase_before,
            "update cycle stops while disabled"
        );

        chip.write(0xE7FF, 0x00); // clear via the window top re-enables
        assert!(!chip.sound_disabled);
        chip.tick(150);
        assert_ne!(chip.ram[base + 1], phase_before, "updates resume");
    }

    #[test]
    fn n163_data_port_read_returns_ram_at_current_address() {
        // §"Data Port ($4800-$4FFF)": "When read, the appropriate byte
        // is returned." With auto-increment clear, repeated reads stay
        // at the same address.
        let mut chip = N163::new();
        chip.ram[0x20] = 0x5A;
        chip.write(0xF800, 0x20); // addr=0x20, I clear → no auto-inc
        assert_eq!(chip.read(0x4800), 0x5A);
        assert_eq!(
            chip.addr, 0x20,
            "read must not move a non-incrementing pointer"
        );
        assert_eq!(chip.read(0x4800), 0x5A);
    }

    #[test]
    fn n163_data_port_read_auto_increments_when_i_bit_set() {
        // §"Address Port": "If the 'I' bit is set, the address will
        // increment on writes and reads to the Data Port ($4800)."
        let mut chip = N163::new();
        chip.ram[0x30] = 0x11;
        chip.ram[0x31] = 0x22;
        chip.ram[0x32] = 0x33;
        chip.write(0xF800, 0x80 | 0x30); // addr=0x30, auto-inc set
        assert_eq!(chip.read(0x4800), 0x11);
        assert_eq!(chip.addr, 0x31);
        assert_eq!(chip.read(0x4800), 0x22);
        assert_eq!(chip.addr, 0x32);
        assert_eq!(chip.read(0x4800), 0x33);
        assert_eq!(chip.addr, 0x33);
    }

    /// Configure one N163 channel directly in sound RAM so a test can
    /// drive a deterministic held sample. `ch` is 1-based; the
    /// frequency is kept at 0 so the phase (and therefore the sampled
    /// nibble) is stable across `tick_one_channel`. `wave_addr`
    /// selects the nibble index and `volume` is the 4-bit linear
    /// volume.
    fn n163_setup_channel(chip: &mut N163, ch: u8, wave_addr: u8, volume: u8) {
        let base = N163::chan_base(ch);
        chip.ram[base] = 0; // low freq
        chip.ram[base + 1] = 0; // low phase
        chip.ram[base + 2] = 0; // mid freq
        chip.ram[base + 3] = 0; // mid phase
        chip.ram[base + 4] = 0; // L=0 → wave_len 256, high freq 0
        chip.ram[base + 5] = 0; // high phase
        chip.ram[base + 6] = wave_addr; // wave address
        chip.ram[base + 7] = volume & 0x0F; // linear volume
    }

    #[test]
    fn n163_output_sums_active_channels_divided_by_count() {
        // §"Mixing": "it is often preferred to simply sum the channel
        // outputs, and divide the output volume by the number of active
        // channels." With two channels enabled the audible output must
        // be the *average* of both held samples, not just whichever one
        // ticked most recently.
        let mut chip = N163::new();
        chip.enabled = true;
        // Two active channels → ch7 (slot 0) + ch8 (slot 1).
        chip.channels_active = 2;
        // Nibble 0 of the wave RAM = low nibble of byte 0; nibble 1 =
        // high nibble of byte 0. Set byte 0 so nibble 0 = 0xF (→ +7)
        // and nibble 1 = 0x0 (→ -8).
        chip.ram[0] = 0x0F;
        // Channel 7 reads nibble 0 (+7), channel 8 reads nibble 1 (-8),
        // both at full volume 15.
        n163_setup_channel(&mut chip, 7, 0, 15);
        n163_setup_channel(&mut chip, 8, 1, 15);

        // Tick both channels once (round-robin: slot 0 then slot 1).
        chip.tick_one_channel();
        chip.tick_one_channel();

        let ch7 = 7.0f32 * 15.0 / 128.0; // (+7) * vol / 128
        let ch8 = -8.0f32 * 15.0 / 128.0; // (-8) * vol / 128
        let expected = (ch7 + ch8) / 2.0;
        assert!(
            (chip.output() - expected).abs() < 1e-6,
            "output {} should be the 2-channel average {}",
            chip.output(),
            expected
        );
        // `last_output` still reflects the single most-recent tick (ch8).
        assert!((chip.last_output - ch8).abs() < 1e-6);
        // The averaged mix must differ from the bare last-channel value
        // — the property the §"Mixing" sum exists to fix.
        assert!((chip.output() - chip.last_output).abs() > 1e-3);
    }

    #[test]
    fn n163_single_channel_output_equals_its_sample() {
        // With one active channel the sum/divide reduces to that
        // channel's held sample (divide by 1), so the mix matches the
        // single-DAC `last_output`.
        let mut chip = N163::new();
        chip.enabled = true;
        chip.channels_active = 1; // only ch8 active
        chip.ram[0] = 0x0F; // nibble 0 = +7
        n163_setup_channel(&mut chip, 8, 0, 15);
        chip.tick_one_channel();
        let expected = 7.0f32 * 15.0 / 128.0;
        assert!((chip.output() - expected).abs() < 1e-6);
        assert!((chip.output() - chip.last_output).abs() < 1e-6);
    }

    #[test]
    fn n163_inactive_channel_holds_are_excluded_from_mix() {
        // A stale hold on a now-inactive channel must not leak into the
        // sum — `output()` iterates only the active set (top-down per
        // `$7F` `1+C`).
        let mut chip = N163::new();
        chip.enabled = true;
        // Park a large stale value on channel 1 (inactive when only the
        // top channels are enabled).
        chip.chan_hold[0] = 99.0;
        chip.channels_active = 1; // only ch8 active
        chip.ram[0] = 0x0F;
        n163_setup_channel(&mut chip, 8, 0, 15);
        chip.tick_one_channel();
        let expected = 7.0f32 * 15.0 / 128.0;
        assert!(
            (chip.output() - expected).abs() < 1e-6,
            "stale channel-1 hold must not enter the 1-channel mix"
        );
    }

    #[test]
    fn n163_data_port_read_auto_increment_stops_at_7f() {
        // §"Address Port": "it does not wrap, instead stopping at $7F."
        // The same non-wrapping rule the write path honours applies to
        // the read-driven increment.
        let mut chip = N163::new();
        chip.ram[0x7F] = 0xCC;
        chip.write(0xF800, 0x80 | 0x7F); // addr=0x7F, auto-inc set
        assert_eq!(chip.read(0x4800), 0xCC);
        assert_eq!(chip.addr, 0x7F, "pointer must clamp at $7F, not wrap");
        // A second read still observes $7F (pointer pinned).
        assert_eq!(chip.read(0x4800), 0xCC);
        assert_eq!(chip.addr, 0x7F);
    }

    #[test]
    fn n163_data_port_read_through_expansion_router_increments() {
        // Drive the auto-increment via the public `Expansion::read`
        // path (the bus-facing entry the CPU uses), confirming the
        // mutation reaches the live chip when N163 is enabled.
        let mut ex = Expansion::new();
        ex.set_flags(ExpansionChips(0x10)); // N163 enable bit
        ex.n163.ram[0x40] = 0xDE;
        ex.n163.ram[0x41] = 0xAD;
        ex.n163.write(0xF800, 0x80 | 0x40); // addr=0x40, auto-inc
        assert_eq!(ex.read(0x4800), 0xDE);
        assert_eq!(ex.n163.addr, 0x41);
        assert_eq!(ex.read(0x4800), 0xAD);
        assert_eq!(ex.n163.addr, 0x42);
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

    // ----- N163 emitted-frequency / update-rate calibration (round 274) -----

    const N163_NTSC_HZ: u32 = 1_789_773;
    const N163_PAL_HZ: u32 = 1_662_607;

    #[test]
    fn n163_update_rate_matches_wiki_ntsc_table() {
        // Wiki §"Channel Update" tabulates the per-channel update rate
        // at the NTSC clock (≈1.789773 MHz): rate = n / (15 * c).
        // The documented kHz values (1..=8 channels):
        let expected_khz = [
            119.318_f64,
            59.659,
            39.773,
            29.830,
            23.864,
            19.886,
            17.045,
            14.915,
        ];
        let mut chip = N163::new();
        for (i, &khz) in expected_khz.iter().enumerate() {
            chip.channels_active = (i + 1) as u8;
            let got_khz = chip.update_rate_hz(N163_NTSC_HZ) / 1000.0;
            assert!(
                (got_khz - khz).abs() < 0.001,
                "ntsc {} channels: got {got_khz} kHz, table says {khz} kHz",
                i + 1
            );
        }
    }

    #[test]
    fn n163_update_rate_matches_wiki_pal_table() {
        // Wiki §"Channel Update" PAL column (≈1.662607 MHz clock).
        let expected_khz = [
            110.840_f64,
            55.420,
            36.947,
            27.710,
            22.168,
            18.473,
            15.834,
            13.855,
        ];
        let mut chip = N163::new();
        for (i, &khz) in expected_khz.iter().enumerate() {
            chip.channels_active = (i + 1) as u8;
            let got_khz = chip.update_rate_hz(N163_PAL_HZ) / 1000.0;
            assert!(
                (got_khz - khz).abs() < 0.001,
                "pal {} channels: got {got_khz} kHz, table says {khz} kHz",
                i + 1
            );
        }
    }

    #[test]
    fn n163_update_rate_zero_when_no_channels_active() {
        let mut chip = N163::new();
        chip.channels_active = 0;
        assert_eq!(chip.update_rate_hz(N163_NTSC_HZ), 0.0);
    }

    #[test]
    fn n163_update_rate_halves_when_channels_double() {
        // rate = n / (15 * c): doubling c halves the per-channel rate.
        let mut chip = N163::new();
        chip.channels_active = 2;
        let r2 = chip.update_rate_hz(N163_NTSC_HZ);
        chip.channels_active = 4;
        let r4 = chip.update_rate_hz(N163_NTSC_HZ);
        assert!((r2 - 2.0 * r4).abs() < 1e-6, "r2={r2} r4={r4}");
    }

    #[test]
    fn n163_emitted_frequency_matches_closed_form() {
        // Wiki §"Frequency": f = (n * p) / (15 * 65536 * l * c).
        // Single channel (c=1), wave_len l=4 (L field=63), p=0x100.
        let mut chip = N163::new();
        chip.enabled = true;
        n163_write_channel8(&mut chip, 0x100, 0, 63, 0, 0x0F, 0);
        assert_eq!(chip.channels_active, 1);
        let p = 256.0_f64; // 0x100
        let l = 4.0_f64;
        let c = 1.0_f64;
        let expected = (N163_NTSC_HZ as f64 * p) / (15.0 * 65536.0 * l * c);
        let got = chip.emitted_frequency_hz(8, N163_NTSC_HZ);
        assert!(
            (got - expected).abs() < 1e-6,
            "got {got}, expected {expected}"
        );
    }

    #[test]
    fn n163_emitted_frequency_scales_inversely_with_channel_count() {
        // The §"Frequency" note: "the output frequency is thus divided
        // by the number of channels enabled." Same p + l, but c=4
        // should yield exactly a quarter of the c=1 frequency.
        let mut chip1 = N163::new();
        chip1.enabled = true;
        n163_write_channel8(&mut chip1, 0x4000, 0, 63, 0, 0x0F, 0); // C=0 → 1 ch
        let f1 = chip1.emitted_frequency_hz(8, N163_NTSC_HZ);

        let mut chip4 = N163::new();
        chip4.enabled = true;
        n163_write_channel8(&mut chip4, 0x4000, 0, 63, 0, 0x0F, 3); // C=3 → 4 ch
        assert_eq!(chip4.channels_active, 4);
        let f4 = chip4.emitted_frequency_hz(8, N163_NTSC_HZ);
        assert!((f1 - 4.0 * f4).abs() < 1e-6, "f1={f1} f4={f4}");
    }

    #[test]
    fn n163_emitted_frequency_scales_inversely_with_wave_length() {
        // f ∝ 1/l: a wave twice as long emits half the frequency for
        // the same 18-bit frequency value.
        // wave_len = 256 - (L<<2). L=63 → len 4; L=62 → len 8.
        let mut chip_short = N163::new();
        chip_short.enabled = true;
        n163_write_channel8(&mut chip_short, 0x400, 0, 63, 0, 0x0F, 0); // len 4
        let f_short = chip_short.emitted_frequency_hz(8, N163_NTSC_HZ);

        let mut chip_long = N163::new();
        chip_long.enabled = true;
        n163_write_channel8(&mut chip_long, 0x400, 0, 62, 0, 0x0F, 0); // len 8
        let f_long = chip_long.emitted_frequency_hz(8, N163_NTSC_HZ);
        assert!(
            (f_short - 2.0 * f_long).abs() < 1e-6,
            "short={f_short} long={f_long}"
        );
    }

    #[test]
    fn n163_emitted_frequency_zero_for_silent_or_inactive() {
        let mut chip = N163::new();
        chip.enabled = true;
        // freq value 0 → no output frequency.
        n163_write_channel8(&mut chip, 0, 0, 63, 0, 0x0F, 0);
        assert_eq!(chip.emitted_frequency_hz(8, N163_NTSC_HZ), 0.0);
        // Out-of-range channel index → 0.
        n163_write_channel8(&mut chip, 0x100, 0, 63, 0, 0x0F, 0);
        assert_eq!(chip.emitted_frequency_hz(0, N163_NTSC_HZ), 0.0);
        assert_eq!(chip.emitted_frequency_hz(9, N163_NTSC_HZ), 0.0);
        // No channels active → 0.
        chip.channels_active = 0;
        assert_eq!(chip.emitted_frequency_hz(8, N163_NTSC_HZ), 0.0);
    }

    #[test]
    fn n163_emitted_frequency_pal_uses_pal_clock() {
        // Same registers, PAL clock → frequency scaled by the clock
        // ratio n_pal / n_ntsc.
        let mut chip = N163::new();
        chip.enabled = true;
        n163_write_channel8(&mut chip, 0x800, 0, 63, 0, 0x0F, 0);
        let f_ntsc = chip.emitted_frequency_hz(8, N163_NTSC_HZ);
        let f_pal = chip.emitted_frequency_hz(8, N163_PAL_HZ);
        let ratio = N163_PAL_HZ as f64 / N163_NTSC_HZ as f64;
        assert!(
            (f_pal - f_ntsc * ratio).abs() < 1e-6,
            "f_ntsc={f_ntsc} f_pal={f_pal}"
        );
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

    /// §"Internal patch set": a write to the user-patch registers
    /// ($00-$07) must reload the live operator constants of any channel
    /// already selecting patch slot 0 — even when the $3X patch-index /
    /// volume bytes are unchanged (so the patch-swap reload path doesn't
    /// fire). Regression: previously the user-patch AR/DR/etc. only
    /// reached the envelopes on a $3X patch/volume change, so a track
    /// that programs $00-$07 *after* selecting patch 0 ran with the
    /// default (zeroed) operator constants.
    #[test]
    fn vrc7_user_patch_write_reloads_patch_zero_channels() {
        let mut chip = Vrc7::new();
        // Channel 0 already selects patch 0, volume 0 (the power-on
        // default) — so the $30 write below is a no-op for the swap path.
        vrc7_write_reg(&mut chip, 0x30, 0x00);
        assert_eq!(chip.opll_channels[0].carrier.env.attack_rate, 0);
        // Now program the user patch with a distinctive carrier AR.
        vrc7_write_reg(&mut chip, 0x05, 0xF7); // carrier $05: AR=15, DR=7
        assert_eq!(
            chip.opll_channels[0].carrier.env.attack_rate, 15,
            "user-patch $05 write must reload patch-0 channel's carrier AR"
        );
        vrc7_write_reg(&mut chip, 0x04, 0xA3); // modulator $04: AR=10, DR=3
        assert_eq!(
            chip.opll_channels[0].modulator.env.attack_rate, 10,
            "user-patch $04 write must reload patch-0 channel's modulator AR"
        );
        // A channel on a ROM patch (index 1) must NOT be disturbed by a
        // user-patch write.
        vrc7_write_reg(&mut chip, 0x31, 0x10); // channel 1 → patch 1
        let rom_ar = chip.opll_channels[1].carrier.env.attack_rate;
        vrc7_write_reg(&mut chip, 0x05, 0x11); // reprogram user patch
        assert_eq!(
            chip.opll_channels[1].carrier.env.attack_rate, rom_ar,
            "user-patch write must not touch a ROM-patch channel"
        );
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

    // ----------------------------------------------- $0F test register

    /// Writing a byte to `$0F` via the indirect port should land in
    /// `regs[0x0F]` AND update the decoded `test_register` struct per
    /// `docs/audio/nsf/vrc7-audio-wiki.html` §"Test Register $0F".
    #[test]
    fn vrc7_register_0f_updates_test_register_state() {
        let mut chip = Vrc7::new();
        vrc7_write_reg(&mut chip, 0x0F, 0b1111);
        assert_eq!(chip.regs[0x0F], 0b1111);
        assert!(chip.test_register.envs_zero);
        assert!(chip.test_register.hold_lfo);
        assert!(chip.test_register.hold_phase);
        assert!(chip.test_register.fast_lfo);
        // Clear it again.
        vrc7_write_reg(&mut chip, 0x0F, 0);
        assert_eq!(chip.test_register, crate::opll::TestRegister::default());
    }

    /// §"Test Register $0F" bit 2: with the waveform phase held at 0,
    /// even a keyed-on channel should fall silent. The chip's
    /// `latched_output` should sit near 0 across a long tick window.
    #[test]
    fn vrc7_test_register_bit2_silences_chip() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Programme the slot-0 user patch with both KSR bits clear,
        // a fast-but-not-instant attack, a long sustain level so the
        // carrier holds an audible amplitude, and an EG-TYP=1
        // (sustained) tone on the carrier so it doesn't release
        // through sustain. Modulator $00 = 0x21 (T=0 V=0 S=1 K=0 M=1
        // → EG-TYP=sustained, KSR=0). Carrier $01 = 0x21 (same).
        // $04 = 0xF7 (AR=15, DR=7 — slow enough to dwell), $05 same.
        // $06 / $07 = 0x10 (SL=1 = loud, RR=0 — release halts).
        vrc7_write_reg(&mut chip, 0x00, 0x21);
        vrc7_write_reg(&mut chip, 0x01, 0x21);
        vrc7_write_reg(&mut chip, 0x02, 0x00);
        vrc7_write_reg(&mut chip, 0x03, 0x00);
        vrc7_write_reg(&mut chip, 0x04, 0xF7);
        vrc7_write_reg(&mut chip, 0x05, 0xF7);
        vrc7_write_reg(&mut chip, 0x06, 0x10);
        vrc7_write_reg(&mut chip, 0x07, 0x10);
        // Channel 0: patch=0 (user), volume=0 (loudest), block=4,
        // fnum=0x100 (so the phase generator advances).
        vrc7_write_reg(&mut chip, 0x30, 0x00);
        vrc7_write_reg(&mut chip, 0x10, 0x00); // fnum low = 0x00
        vrc7_write_reg(&mut chip, 0x20, 0x19); // key-on, block 4, fnum-high bit = 1 → fnum = 0x100
                                               // Accumulate a baseline peak across many ticks so we don't
                                               // catch a zero-crossing latched value.
        let mut baseline_peak: i32 = 0;
        for _ in 0..500 {
            chip.tick(50);
            baseline_peak = baseline_peak.max(chip.latched_output.abs());
        }
        assert!(
            baseline_peak > 5,
            "fixture didn't produce audible signal before the test ran, baseline_peak={}",
            baseline_peak
        );
        // Now hold the phase at 0 — the chip should go silent.
        vrc7_write_reg(&mut chip, 0x0F, 0b0100);
        // First flush any samples emitted before the phase-hold took
        // effect; then sample.
        chip.tick(2_000);
        let mut peak: i32 = 0;
        for _ in 0..500 {
            chip.tick(50);
            peak = peak.max(chip.latched_output.abs());
        }
        assert!(peak <= 5, "phase-hold must silence chip, got {}", peak);
    }

    // ----------------------------------------------- $2X.S channel sustain

    /// §Channels: the `$2X.S` override should change the operator's
    /// `release_rate` even when the patch was loaded with a different
    /// value.
    #[test]
    fn vrc7_channel_sustain_bit_overrides_release_rate_to_five() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Trumpet (patch 7): car_release = 0x07 per the ROM.
        vrc7_write_reg(&mut chip, 0x30, 0x70);
        vrc7_write_reg(&mut chip, 0x20, 0x10); // key-on, no sustain
        let car_rr = chip.opll_channels[0].carrier.env.release_rate;
        assert_eq!(car_rr, 0x07);
        // Set sustain bit ($2X.S = 1).
        vrc7_write_reg(&mut chip, 0x20, 0x30);
        assert_eq!(chip.opll_channels[0].carrier.env.release_rate, 0x05);
        assert_eq!(chip.opll_channels[0].modulator.env.release_rate, 0x05);
        // Clear it again — should revert to the patch value.
        vrc7_write_reg(&mut chip, 0x20, 0x10);
        assert_eq!(chip.opll_channels[0].carrier.env.release_rate, 0x07);
    }

    // ----------------------------------------------- $00.S release disable

    /// §"Custom Patch": the modulator's `$00.S` should set
    /// `release_disabled` on the OPLL modulator envelope; the carrier
    /// never has it.
    #[test]
    fn vrc7_modulator_sustain_bit_disables_modulator_release() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Slot 0 user patch: $00 = 0x20 (S=1), $01 = 0x00 (S=0).
        vrc7_write_reg(&mut chip, 0x00, 0x20);
        vrc7_write_reg(&mut chip, 0x01, 0x00);
        // Activate the patch on channel 0 (any non-zero volume triggers
        // the patch reload through volume_changed). Patch index = 0.
        vrc7_write_reg(&mut chip, 0x30, 0x01);
        assert!(chip.opll_channels[0].modulator.env.release_disabled);
        assert!(!chip.opll_channels[0].carrier.env.release_disabled);
    }

    // ----------------------------------------------- $E000 audio reset

    /// §"Audio Reset ($E000)": bit 6 silences the chip and clears its
    /// registers. Writes to `$9010` / `$9030` are disregarded while
    /// it's held.
    #[test]
    fn vrc7_audio_reset_clears_registers_and_blocks_writes() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Pre-populate some registers.
        vrc7_write_reg(&mut chip, 0x30, 0x70);
        vrc7_write_reg(&mut chip, 0x20, 0x10);
        assert_eq!(chip.regs[0x30], 0x70);
        // Now assert audio reset (bit 6 = 0x40).
        chip.write(0xE000, 0x40);
        assert!(chip.audio_reset_held);
        // Registers should be cleared.
        assert_eq!(chip.regs[0x30], 0);
        assert_eq!(chip.regs[0x20], 0);
        // Channel state should be reset.
        assert_eq!(chip.channels[0].patch_index, 0);
        assert_eq!(chip.channels[0].volume, 0);
        assert!(!chip.channels[0].key_on);
        // Writes through the indirect ports should be ignored.
        chip.write(0x9010, 0x30);
        chip.write(0x9030, 0xFF);
        assert_eq!(
            chip.regs[0x30], 0,
            "$9030 write must be ignored when reset is held"
        );
        // Clear the reset and confirm writes are honoured again.
        chip.write(0xE000, 0x00);
        assert!(!chip.audio_reset_held);
        vrc7_write_reg(&mut chip, 0x30, 0x42);
        assert_eq!(chip.regs[0x30], 0x42);
    }

    /// §"Audio Reset ($E000)": ticking the chip while held outputs
    /// silence (`latched_output == 0`).
    #[test]
    fn vrc7_audio_reset_silences_chip_during_tick() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Get the chip producing audio first.
        vrc7_write_reg(&mut chip, 0x30, 0x70);
        vrc7_write_reg(&mut chip, 0x10, 0x00);
        vrc7_write_reg(&mut chip, 0x20, 0x18);
        chip.tick(50_000);
        // Hold reset.
        chip.write(0xE000, 0x40);
        chip.tick(1_000);
        assert_eq!(chip.latched_output, 0);
        // Multiple ticks: still zero.
        for _ in 0..100 {
            chip.tick(50);
            assert_eq!(chip.latched_output, 0);
        }
    }

    /// `Vrc7::tick` advances the built-in AM/VIB LFO phases once per
    /// emitted operator sample, at the spec'd 64 / 1024 cadence. With
    /// enough CPU cycles to emit ≥1024 operator samples, the tremolo
    /// phase has stepped many times and the vibrato phase at least
    /// once. Per `docs/audio/nsf/vrc7-audio-wiki.html`
    /// §"Test Register $0F".
    #[test]
    fn vrc7_tick_advances_lfo_phases() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // ~36 CPU cycles per operator sample; 1100 * 36 ≈ 39_600
        // cycles emits ~1100 operator samples > 1024.
        chip.tick(40_000);
        assert!(
            chip.lfo.tremolo_phase > 1,
            "tremolo phase advanced: {}",
            chip.lfo.tremolo_phase
        );
        assert!(
            chip.lfo.vibrato_phase >= 1,
            "vibrato phase advanced at least once: {}",
            chip.lfo.vibrato_phase
        );
        // Tremolo is 16× faster than vibrato in normal mode.
        assert!(chip.lfo.tremolo_phase > chip.lfo.vibrato_phase);
    }

    /// `$E000` audio reset clears the tremolo LFO phase but preserves
    /// the vibrato LFO phase per `docs/audio/nsf/vrc7-audio-wiki.html`
    /// §"Audio Reset ($E000)": "clear its registers (including
    /// tremolo LFO state, but not including vibrato LFO state)."
    #[test]
    fn vrc7_audio_reset_clears_tremolo_lfo_preserves_vibrato_lfo() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        chip.tick(80_000); // emit enough samples to step both phases
        assert!(chip.lfo.tremolo_phase > 0);
        let vib_before = chip.lfo.vibrato_phase;
        assert!(vib_before > 0);
        chip.write(0xE000, 0x40);
        assert_eq!(chip.lfo.tremolo_phase, 0, "tremolo cleared on audio reset");
        assert_eq!(
            chip.lfo.vibrato_phase, vib_before,
            "vibrato preserved across audio reset"
        );
    }

    /// §"Audio Reset ($E000)": only bit 6 matters for audio. Other
    /// bits (mirroring/WRAM) should not affect audio state.
    #[test]
    fn vrc7_audio_reset_only_reads_bit_six() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Write a value with bit 6 clear but other bits set (e.g.
        // mirroring control). The audio-reset flag should remain
        // false.
        chip.write(0xE000, 0x03); // bit 0-1 = mirroring control bits
        assert!(!chip.audio_reset_held);
        // Bit 6 set = reset on, regardless of other bits.
        chip.write(0xE000, 0x47); // bit 6 + mirroring bits
        assert!(chip.audio_reset_held);
    }

    /// YM2413 Application Manual §III-1-2 Table III-2 — when a
    /// program writes `$1X` / `$2X` to change the channel's pitch
    /// mid-note, both operators' `Rks` offsets re-derive from the
    /// new `(block, fnum_msb)` so the next envelope step picks up
    /// the new rate amplification. This is the
    /// `refresh_from_regs` → `refresh_rks` path on a pitch-only
    /// write.
    #[test]
    fn vrc7_pitch_only_write_refreshes_rks_on_both_operators() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Pick patch 4 ("Flute") which has carrier KSR=1 (`$01.D4`
        // set in the dumped instrument byte) — easy to verify the
        // KSR-on row.
        // Flute = 23 11 25 00 89 89 26 18 per the ROM dump.
        let flute = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[4]);
        // Sanity: carrier KSR bit is what we expect.
        let expected_mod_ksr = flute.mod_ksr;
        let expected_car_ksr = flute.car_ksr;
        // Program channel 0 with patch 4, volume 0, block=2, fnum=0x100
        // (fnum_msb=1, fnum_low=0).
        vrc7_write_reg(&mut chip, 0x30, 0x40); // patch=4, vol=0
        vrc7_write_reg(&mut chip, 0x10, 0x00); // fnum low = 0
                                               // $20 layout: ---STBBB H. Want key-on, block=2 (BBB=010),
                                               // fnum-high=1 (H=1) → 0b0001_0101 = 0x15.
        vrc7_write_reg(&mut chip, 0x20, 0x15);
        let pre_mod_rks = chip.opll_channels[0].modulator.env.rks;
        let pre_car_rks = chip.opll_channels[0].carrier.env.rks;
        // Sanity vs Table III-2 at (block=2, fnum_msb=1):
        //   KSR=0 row → Rks = 2>>1 = 1
        //   KSR=1 row → Rks = (2<<1)|1 = 5
        let expected_pre_mod = if expected_mod_ksr {
            (2 << 1) | 1
        } else {
            2 >> 1
        };
        let expected_pre_car = if expected_car_ksr {
            (2 << 1) | 1
        } else {
            2 >> 1
        };
        assert_eq!(pre_mod_rks, expected_pre_mod);
        assert_eq!(pre_car_rks, expected_pre_car);

        // Now do a pitch-only write that bumps the block to 6 while
        // keeping the same key-on + sustain bits.
        // $20 with block=6 (BBB=110), fnum-high=1, key-on=1 →
        // 0b0001_1101 = 0x1D.
        vrc7_write_reg(&mut chip, 0x20, 0x1D);
        // Same fnum_msb (1), new block (6).
        let post_mod_rks = chip.opll_channels[0].modulator.env.rks;
        let post_car_rks = chip.opll_channels[0].carrier.env.rks;
        let expected_post_mod = if expected_mod_ksr {
            (6 << 1) | 1
        } else {
            6 >> 1
        };
        let expected_post_car = if expected_car_ksr {
            (6 << 1) | 1
        } else {
            6 >> 1
        };
        assert_eq!(
            post_mod_rks, expected_post_mod,
            "mod Rks should re-derive against new block=6: pre={pre_mod_rks} post={post_mod_rks}"
        );
        assert_eq!(
            post_car_rks, expected_post_car,
            "car Rks should re-derive against new block=6: pre={pre_car_rks} post={post_car_rks}"
        );
    }

    /// A patch swap that changes the KSR bit must update `Rks` at
    /// the moment of the swap (without needing a subsequent pitch
    /// write). The `refresh_from_regs` → `load_patch` →
    /// `refresh_rks` path covers this.
    #[test]
    fn vrc7_patch_swap_updates_rks_via_load_patch() {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        // Channel 0 pitch: block=3, fnum=0x100 (msb=1).
        // $20 = 0b0001_0111 = 0x17 (key-on + block=3 + fnum-high=1).
        vrc7_write_reg(&mut chip, 0x10, 0x00);
        vrc7_write_reg(&mut chip, 0x20, 0x17);

        // Patch 1 "Buzzy Bell": $00=0x03 (K=0), $01=0x21 (K=0). Both
        // operators KSR=0 → Rks = 3>>1 = 1.
        vrc7_write_reg(&mut chip, 0x30, 0x10);
        assert_eq!(chip.opll_channels[0].modulator.env.rks, 1);
        assert_eq!(chip.opll_channels[0].carrier.env.rks, 1);

        // Swap to patch $A "Vibes" (`B5 01 ...`) which has mod K=1
        // (`$00 = 0xB5` → bit 4 set) and carrier K=0 (`$01 = 0x01`).
        // After the swap, mod Rks should jump to (3<<1)|1 = 7 while
        // carrier Rks stays on the KSR=0 row at 3>>1 = 1.
        let vibes = Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[0x0A]);
        assert!(
            vibes.mod_ksr,
            "patch $A modulator KSR must be set for this test"
        );
        assert!(!vibes.car_ksr);
        vrc7_write_reg(&mut chip, 0x30, 0xA0); // patch=A, vol=0
        assert_eq!(chip.opll_channels[0].modulator.env.rks, 7);
        assert_eq!(chip.opll_channels[0].carrier.env.rks, 1);
    }

    /// The 3 drum patches in the VRC7 instrument ROM dump, per the
    /// §"Internal patch set" footnote of
    /// `docs/audio/nsf/vrc7-audio-wiki.html` — including the one
    /// documented divergence from the YM2413 drum ROM ("byte $07 of
    /// the snare drum ($68) differs from YM2413 ($48)").
    #[test]
    fn vrc7_rhythm_rom_matches_documented_dump() {
        assert_eq!(
            VRC7_RHYTHM_ROM[0],
            [0x01, 0x01, 0x18, 0x0F, 0xDF, 0xF8, 0x6A, 0x6D],
            "Bass Drum"
        );
        assert_eq!(
            VRC7_RHYTHM_ROM[1],
            [0x01, 0x01, 0x00, 0x00, 0xC8, 0xD8, 0xA7, 0x68],
            "Snare Drum / Hi-Hat"
        );
        assert_eq!(
            VRC7_RHYTHM_ROM[2],
            [0x05, 0x01, 0x00, 0x00, 0xF8, 0xAA, 0x59, 0x55],
            "Tom / Top Cymbal"
        );
        assert_eq!(
            VRC7_RHYTHM_ROM[1][7], 0x68,
            "snare byte $07 is the documented VRC7-vs-YM2413 divergence"
        );
        // Every drum row decodes through the standard §"Custom Patch"
        // 8-byte layout (same format as the melody slots).
        for row in &VRC7_RHYTHM_ROM {
            let p = Vrc7Patch::from_bytes(row);
            assert!(p.mod_mult <= 0x0F && p.car_mult <= 0x0F);
        }
    }

    /// §"Rhythm Register $0E": the VRC7 treats the rhythm-mode bit as
    /// always enabled, has no rhythm DAC, and ignores `$0E` writes —
    /// `rhythm_control()` is constant and the 6 melody channels'
    /// output is bit-identical with or without a `$0E` write.
    #[test]
    fn vrc7_rhythm_register_is_inert() {
        let base = Vrc7::new().rhythm_control();
        assert!(base.rhythm_mode, "rhythm mode reads as always enabled");
        assert!(
            !(base.bd || base.sd || base.tom || base.t_cy || base.hh),
            "no drum key is ever effective (no rhythm DAC)"
        );

        // Two chips in lockstep; one also receives a $0E write with
        // every drum key + the rhythm bit set.
        let mut plain = Vrc7::new();
        let mut poked = Vrc7::new();
        for chip in [&mut plain, &mut poked] {
            chip.enabled = true;
            vrc7_write_reg(chip, 0x30, 0x10); // patch 1, vol 0
            vrc7_write_reg(chip, 0x10, 0x80); // fnum low
            vrc7_write_reg(chip, 0x20, 0x17); // key-on, block 3, fnum msb 1
        }
        vrc7_write_reg(&mut poked, 0x0E, 0x3F);
        // The raw byte is recorded as bookkeeping but never reaches
        // the synthesis path.
        assert_eq!(poked.regs[0x0E], 0x3F);
        assert_eq!(poked.rhythm_control(), base, "decode unaffected by write");
        for _ in 0..256 {
            plain.tick(36);
            poked.tick(36);
            assert_eq!(
                plain.latched_output, poked.latched_output,
                "melody output unaffected by $0E"
            );
        }
    }

    // ---------------- Round 386: NSFe VRC7 chunk application ----------------
    //
    // Spec source: `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7 —
    // byte 0 selects the device ("0 = VRC7, 1 = YM2413"), "The next
    // 128 or 152 bytes are optional, and contain a replacement patch
    // set for the device", and "If a replacement patch set is not
    // contained in this chunk, an appropriate default patch set
    // should be used for the selected device." YM2413 default bytes
    // from `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §2a.

    #[test]
    fn vrc7_nsfe_device_1_selects_the_ym2413_default_rom() {
        let mut chip = Vrc7::new();
        chip.apply_nsfe_chunk(1, None);
        assert!(chip.ym2413_variant);
        // Slot 1: YM2413 "Violin" vs VRC7 "Buzzy Bell" — different
        // silicon, different bytes, different decode.
        let got = chip.patch(1);
        assert_eq!(got, Vrc7Patch::from_bytes(&YM2413_INSTRUMENT_ROM[1]));
        assert_ne!(got, Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[1]));
        // Device 0 restores the VRC7 ROM.
        chip.apply_nsfe_chunk(0, None);
        assert_eq!(
            chip.patch(1),
            Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[1])
        );
    }

    #[test]
    fn vrc7_nsfe_replacement_patch_set_overrides_the_rom() {
        let mut chip = Vrc7::new();
        // A distinctive replacement table: slot 2 carries the VRC7's
        // own "Flute" bytes, everything else zero (slot 0 must be
        // zero per spec).
        let mut table = vec![0u8; 128];
        table[2 * 8..2 * 8 + 8].copy_from_slice(&VRC7_INSTRUMENT_ROM[4]);
        chip.apply_nsfe_chunk(0, Some(&table));
        assert_eq!(
            chip.patch(2),
            Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[4]),
            "slot 2 must decode the replacement bytes"
        );
        // Slot 0 still reads the live user-patch registers, not the
        // table ("on the VRC7 patch 0 is custom-only").
        chip.enabled = true;
        vrc7_write_reg(&mut chip, 0x00, 0x21);
        vrc7_write_reg(&mut chip, 0x01, 0x61);
        let user = chip.patch(0);
        assert_eq!(user.mod_mult, 0x01);
        assert_eq!(user.car_mult, 0x01);
        // A 152-byte table (with the trailing YM2413 rhythm bytes the
        // VRC7 cannot voice) is accepted identically.
        let mut long = vec![0u8; 152];
        long[..128].copy_from_slice(&table);
        let mut chip2 = Vrc7::new();
        chip2.apply_nsfe_chunk(1, Some(&long));
        assert_eq!(
            chip2.patch(2),
            Vrc7Patch::from_bytes(&VRC7_INSTRUMENT_ROM[4]),
            "152-byte form: melody table applies, rhythm tail ignored"
        );
    }

    #[test]
    fn ym2413_rom_slot_zero_is_blank_user_placeholder() {
        assert_eq!(YM2413_INSTRUMENT_ROM[0], [0u8; 8]);
        // Every preset decodes through the standard 8-byte layout.
        for row in &YM2413_INSTRUMENT_ROM[1..] {
            let p = Vrc7Patch::from_bytes(row);
            assert!(p.mod_mult <= 0x0F && p.car_mult <= 0x0F);
        }
    }

    // ---------------- Round 386: aggregate batch invariance ----------------
    //
    // Capstone for the expansion timing tier: with all six chips
    // enabled and playing at once, driving the aggregate for the same
    // total cycle count must land every chip in the same state no
    // matter how the CPU chunks its cycles. Each chip carries its own
    // sub-interval remainder (VRC6 per-cycle dividers, MMC5 /2
    // prescaler carry + 240 Hz accumulator, S5B 16-clock remainder,
    // N163 15-cycle accumulator, FDS per-cycle env/unit walk, VRC7
    // Q8 operator-clock accumulator).

    #[test]
    fn expansion_aggregate_state_is_invariant_to_batch_chunking() {
        let setup = |exp: &mut Expansion| {
            exp.set_flags(ExpansionChips(0x3F)); // all six chips
                                                 // VRC6: pulse 1 + saw.
            exp.write(0x9000, 0x0F);
            exp.write(0x9001, 0x37);
            exp.write(0x9002, 0x80);
            exp.write(0xB000, 0x08);
            exp.write(0xB001, 0x53);
            exp.write(0xB002, 0x80);
            // VRC7: key-on channel 0, patch 2.
            exp.write(0x9010, 0x30);
            exp.write(0x9030, 0x20);
            exp.write(0x9010, 0x10);
            exp.write(0x9030, 0x80);
            exp.write(0x9010, 0x20);
            exp.write(0x9030, 0x17);
            // MMC5: pulse 0 with envelope.
            exp.write(0x5015, 0x01);
            exp.write(0x5000, 0x02);
            exp.write(0x5002, 0x40);
            exp.write(0x5003, 0x08);
            // N163: one channel with a live frequency.
            exp.write(0xF800, 0x80 | 0x40); // addr $40, auto-inc
            for _ in 0..0x40 {
                exp.write(0x4800, 0x9A); // wave data + channel regs
            }
            // S5B: tone A + envelope.
            exp.write(0xC000, 0x00);
            exp.write(0xE000, 0x1B);
            exp.write(0xC000, 0x08);
            exp.write(0xE000, 0x10);
            exp.write(0xC000, 0x0B);
            exp.write(0xE000, 0x21);
            exp.write(0xC000, 0x0D);
            exp.write(0xE000, 0x0E);
            // FDS: wave + mod + both envelopes.
            exp.write(0x4089, 0x80);
            exp.write(0x4040, 0x20);
            exp.write(0x4041, 0x3F);
            exp.write(0x4089, 0x00);
            exp.write(0x4085, 0x08);
            exp.write(0x4088, 0x02);
            exp.write(0x4086, 0x7F);
            exp.write(0x4087, 0x01);
            exp.write(0x4084, 0x0A);
            exp.write(0x4080, 0x15);
            exp.write(0x408A, 0x20);
            exp.write(0x4082, 0x80);
            exp.write(0x4083, 0x01);
        };
        let mut whole = Expansion::new();
        let mut ragged = Expansion::new();
        setup(&mut whole);
        setup(&mut ragged);

        const TOTAL: u32 = 60_017;
        whole.tick(TOTAL);
        let chunks = [1u32, 2, 3, 5, 7, 11, 13, 36];
        let mut left = TOTAL;
        let mut i = 0usize;
        while left > 0 {
            let n = chunks[i % chunks.len()].min(left);
            ragged.tick(n);
            left -= n;
            i += 1;
        }

        // VRC6.
        assert_eq!(whole.vrc6.pulse[0].timer, ragged.vrc6.pulse[0].timer);
        assert_eq!(whole.vrc6.pulse[0].step, ragged.vrc6.pulse[0].step);
        assert_eq!(whole.vrc6.saw.step, ragged.vrc6.saw.step);
        assert_eq!(whole.vrc6.saw.accum, ragged.vrc6.saw.accum);
        // VRC7.
        assert_eq!(whole.vrc7.op_cycles_q8, ragged.vrc7.op_cycles_q8);
        assert_eq!(whole.vrc7.latched_output, ragged.vrc7.latched_output);
        // MMC5.
        assert_eq!(whole.mmc5.pulse[0].timer, ragged.mmc5.pulse[0].timer);
        assert_eq!(whole.mmc5.pulse[0].step, ragged.mmc5.pulse[0].step);
        assert_eq!(
            whole.mmc5.pulse[0].env_decay,
            ragged.mmc5.pulse[0].env_decay
        );
        assert_eq!(whole.mmc5.frame_acc, ragged.mmc5.frame_acc);
        // N163 — phase bytes live in sound RAM.
        assert_eq!(whole.n163.ram, ragged.n163.ram);
        assert_eq!(whole.n163.cycle_accum, ragged.n163.cycle_accum);
        // S5B.
        assert_eq!(whole.s5b.channels[0].timer, ragged.s5b.channels[0].timer);
        assert_eq!(whole.s5b.channels[0].level, ragged.s5b.channels[0].level);
        assert_eq!(whole.s5b.envelope.step, ragged.s5b.envelope.step);
        assert_eq!(whole.s5b.clock_rem, ragged.s5b.clock_rem);
        // FDS.
        assert_eq!(whole.fds.wave_acc, ragged.fds.wave_acc);
        assert_eq!(whole.fds.mod_counter, ragged.fds.mod_counter);
        assert_eq!(whole.fds.mod_gain, ragged.fds.mod_gain);
        assert_eq!(whole.fds.volume, ragged.fds.volume);
        assert_eq!(whole.fds.cycle_acc, ragged.fds.cycle_acc);

        // And the mixed output is identical.
        assert_eq!(whole.output(), ragged.output());
    }
}
