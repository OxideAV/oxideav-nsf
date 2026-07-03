//! `NsfPlayer` — bus + CPU + APU + clock-driven `init` / `play` calls.
//!
//! A run looks like this:
//!
//! 1. Construct an [`NsfPlayer`] from a parsed [`crate::NsfHeader`] and
//!    the desired sample rate (44.1 kHz by default).
//! 2. Call [`NsfPlayer::start_song`] to seed the CPU, run the `init`
//!    routine to completion, and prime the per-period scheduler.
//! 3. Call [`NsfPlayer::render`] repeatedly with output buffers — the
//!    player runs CPU cycles at a target NES clock rate, calls the
//!    `play` routine once per `play_period_us`, and resamples the APU
//!    output to the requested sample rate.
//!
//! ## NSF2 playback paradigms
//!
//! Per `docs/audio/nsf/nsf2-nesdev-wiki.html`:
//!
//! * **IRQ support** (`$7C` bit 4) — the NSF2 IRQ timer device at
//!   `$401B/$401C/$401D` may assert the CPU IRQ line; the program owns
//!   the IRQ vector at `$FFFE/$FFFF`. The bus arms the timer + vector
//!   overlay; the CPU services `irq()` when the I flag is clear.
//! * **Non-returning INIT** (`$7C` bit 5) — INIT is called twice. The
//!   first call (Y=$80) must return. After that the player enables
//!   NMI and calls INIT again with Y=$81; this call may run forever.
//!   PLAY is then driven from an NMI wrapper that preserves A/X/Y and
//!   ends with RTI back into the still-running INIT.
//! * **Suppressed PLAY** (`$7C` bit 6) — PLAY is never invoked.
//!   Combine with non-returning INIT to let INIT control output
//!   without periodic interruption.
//!
//! Implementation: the player installs a small NMI wrapper in RAM at
//! [`NMI_WRAPPER_ADDR`] (`$0200`) that does
//! `PHA / TXA / PHA / TYA / PHA / JSR play_addr / PLA / TAY / PLA / TAX
//! / PLA / RTI` and arms the bus's vector overlay so `$FFFA-$FFFB`
//! points at it. The PLAY-period scheduler then calls
//! [`crate::bus::NesBus::request_nmi`] every `play_period_cycles`.

use crate::bus::NesBus;
use crate::cpu::Cpu6502;
use crate::header::{NsfHeader, NsfRegion};

/// NTSC NES CPU clock.
const NTSC_CPU_HZ: u32 = 1_789_773;
/// PAL NES CPU clock.
const PAL_CPU_HZ: u32 = 1_662_607;
/// Dendy NES-clone CPU clock per
/// `docs/audio/nsf/apu-pulse-wiki.html` (the `f_CPU is … 1.773448 MHz
/// for Dendy` parenthetical in the pulse-period formula).
const DENDY_CPU_HZ: u32 = 1_773_448;

/// Sentinel return address pushed under the init / play subroutine
/// frame. When the routine RTS's, the CPU jumps here and we stop
/// executing for the current period.
const STOP_SENTINEL: u16 = 0x4FFF;

/// Maximum number of CPU cycles we'll spend inside one `init` or
/// `play` invocation. NSF rips that get stuck (or never RTS) would
/// otherwise hang the player indefinitely.
const MAX_CYCLES_PER_CALL: u32 = 1_000_000;

/// Where the NSF2 NMI wrapper lives in NES RAM (page 2 — safely below
/// the cart-RAM region and clear of the zero-page / stack / NSF state
/// that the music driver typically uses).
pub const NMI_WRAPPER_ADDR: u16 = 0x0200;

/// Player state machine.
/// Post-DAC analog signal conditioning per
/// `docs/audio/nsf/apu-mixer-wiki.html` §"The NES hardware follows the
/// DACs with a surprisingly involved circuit": two first-order high-pass
/// filters (90 Hz and 440 Hz) followed by a first-order low-pass at
/// 14 kHz. The high-passes remove the DC bias of the `[0, 1]`-ranged
/// mixer output (the APU DACs only swing positive) and the low-pass
/// tames the harshest aliasing, both of which markedly affect how an NSF
/// rip actually sounds. Filters run at the player's output sample rate.
struct OutputFilter {
    /// First-order high-pass state (previous input + output) per stage.
    hp90_prev_in: f32,
    hp90_prev_out: f32,
    hp440_prev_in: f32,
    hp440_prev_out: f32,
    /// First-order low-pass state.
    lp_prev_out: f32,
    /// Per-stage smoothing coefficients (derived from the cutoff and the
    /// output sample period).
    hp90_alpha: f32,
    hp440_alpha: f32,
    lp_alpha: f32,
}

impl OutputFilter {
    fn new(sample_rate: u32) -> Self {
        let dt = 1.0 / sample_rate as f32;
        // First-order high-pass: alpha = RC / (RC + dt) with RC =
        // 1 / (2*pi*fc).
        let hp_alpha = |fc: f32| {
            let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
            rc / (rc + dt)
        };
        // First-order low-pass: alpha = dt / (RC + dt).
        let lp_alpha = |fc: f32| {
            let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
            dt / (rc + dt)
        };
        Self {
            hp90_prev_in: 0.0,
            hp90_prev_out: 0.0,
            hp440_prev_in: 0.0,
            hp440_prev_out: 0.0,
            lp_prev_out: 0.0,
            hp90_alpha: hp_alpha(90.0),
            hp440_alpha: hp_alpha(440.0),
            lp_alpha: lp_alpha(14_000.0),
        }
    }

    /// Push one mixer sample through HP90 → HP440 → LP14k.
    fn process(&mut self, x: f32) -> f32 {
        // y[n] = alpha * (y[n-1] + x[n] - x[n-1])
        let y90 = self.hp90_alpha * (self.hp90_prev_out + x - self.hp90_prev_in);
        self.hp90_prev_in = x;
        self.hp90_prev_out = y90;

        let y440 = self.hp440_alpha * (self.hp440_prev_out + y90 - self.hp440_prev_in);
        self.hp440_prev_in = y90;
        self.hp440_prev_out = y440;

        // y[n] = y[n-1] + alpha * (x[n] - y[n-1])
        let ylp = self.lp_prev_out + self.lp_alpha * (y440 - self.lp_prev_out);
        self.lp_prev_out = ylp;
        ylp
    }

    fn reset(&mut self) {
        self.hp90_prev_in = 0.0;
        self.hp90_prev_out = 0.0;
        self.hp440_prev_in = 0.0;
        self.hp440_prev_out = 0.0;
        self.lp_prev_out = 0.0;
    }
}

pub struct NsfPlayer {
    pub cpu: Cpu6502,
    pub bus: NesBus,
    pub header: NsfHeader,
    cpu_hz: u32,
    sample_rate: u32,
    /// Post-DAC analog filter chain applied to the mixer output.
    filter: OutputFilter,
    /// Cycles between consecutive `play` calls.
    play_period_cycles: u32,
    /// CPU cycles since the last `play` call.
    cycles_since_play: u32,
    /// CPU cycles per output sample (fixed-point `cpu_hz / sample_rate`,
    /// kept as f64 for accuracy across long renders).
    cycles_per_sample: f64,
    /// Fractional carry across `render` calls.
    sample_acc: f64,
    /// Active song (1-based).
    song: u8,
    /// True after [`NsfPlayer::start_song`] succeeded.
    started: bool,
    /// NSF2 non-returning INIT in effect — PLAY is delivered via NMI
    /// instead of a JSR off the stop-sentinel.
    nsf2_nmi_play: bool,
    /// NSF2 suppressed-PLAY bit — never invoke the play routine.
    nsf2_suppress_play: bool,
}

impl NsfPlayer {
    /// Build a player for `header` at `sample_rate` Hz.
    pub fn new(header: NsfHeader, sample_rate: u32) -> Self {
        let cpu_hz = match header.region {
            NsfRegion::Pal => PAL_CPU_HZ,
            NsfRegion::Dendy => DENDY_CPU_HZ,
            NsfRegion::Ntsc | NsfRegion::Dual => NTSC_CPU_HZ,
        };
        let mut bus = NesBus::new();
        bus.apu.set_cpu_hz(cpu_hz);
        bus.configure_from_header(&header);

        // `play_period_us` honours the Dendy speed (NSFe RATE byte
        // $0004) with PAL fallback, matching the spec.
        let play_us = header.play_period_us();
        let play_us_eff = if play_us != 0 {
            play_us as u32
        } else {
            match header.region {
                NsfRegion::Pal | NsfRegion::Dendy => 19_997,
                NsfRegion::Ntsc | NsfRegion::Dual => 16_666,
            }
        };
        let play_period_cycles = ((play_us_eff as u64 * cpu_hz as u64) / 1_000_000) as u32;

        // Apply NSFe `mixe` per-device gain overrides to the APU
        // mixer. The mixer entries are already decoded in the header
        // metadata; the APU stores them as a linear-gain table indexed
        // by NSFe device id.
        if !header.metadata.mixer.is_empty() {
            bus.apu.apply_mixe_overrides(&header.metadata.mixer);
        }

        // Apply the NSFe `VRC7` chunk (device variant + optional
        // replacement patch set) to the VRC7 synthesis path. Per
        // `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7 the chunk
        // swaps the instrument ROM: device 1 selects the YM2413
        // default set, and a supplied 128-/152-byte table replaces
        // the built-in patches outright.
        if let Some(v) = &header.metadata.vrc7 {
            bus.apu
                .expansion
                .vrc7
                .apply_nsfe_chunk(v.device, v.patches.as_deref());
        }

        let nsf2_nmi_play = header.nsf2.non_returning_init();
        let nsf2_suppress_play = header.nsf2.suppressed_play();

        Self {
            cpu: Cpu6502::new(),
            bus,
            header,
            cpu_hz,
            sample_rate,
            filter: OutputFilter::new(sample_rate),
            play_period_cycles,
            cycles_since_play: 0,
            cycles_per_sample: cpu_hz as f64 / sample_rate as f64,
            sample_acc: 0.0,
            song: 0,
            started: false,
            nsf2_nmi_play,
            nsf2_suppress_play,
        }
    }

    /// Install the NSF2 NMI wrapper at [`NMI_WRAPPER_ADDR`] and arm
    /// the bus's vector overlay. The wrapper runs at `$0200` and is
    ///
    /// ```text
    ///   PHA TXA PHA TYA PHA          ; preserve A / X / Y (3 cycles each)
    ///   JSR play_addr                ; PLAY may RTS naturally
    ///   PLA TAY PLA TAX PLA          ; restore Y / X / A
    ///   RTI                          ; return into the still-running INIT
    /// ```
    fn install_nmi_wrapper(&mut self) {
        let play = self.header.play_addr;
        let lo = play as u8;
        let hi = (play >> 8) as u8;
        let wrapper: [u8; 14] = [
            0x48, // PHA
            0x8A, // TXA
            0x48, // PHA
            0x98, // TYA
            0x48, // PHA
            0x20, lo, hi,   // JSR play_addr
            0x68, // PLA
            0xA8, // TAY
            0x68, // PLA
            0xAA, // TAX
            0x68, // PLA
            0x40, // RTI
        ];
        // bus.ram is 2KiB; $0200 maps to offset 0x0200.
        for (i, b) in wrapper.iter().enumerate() {
            self.bus.ram[NMI_WRAPPER_ADDR as usize + i] = *b;
        }
        // Vector overlay: NMI → NMI_WRAPPER_ADDR, Reset → stop sentinel
        // (we never reset mid-playback). IRQ slot starts wherever the
        // underlying ROM said it should — the NSF program will
        // overwrite if needed.
        self.bus.arm_vector_overlay(NMI_WRAPPER_ADDR, STOP_SENTINEL);
    }

    /// Choose `song` (1-based) and run its `init` routine to completion.
    pub fn start_song(&mut self, song: u8) {
        self.song = song;
        let a = song.saturating_sub(1);
        // Per `docs/audio/nsf/nsf-nesdev-wiki.html` §INIT and
        // `docs/audio/nsf/nsfe-nesdev-wiki.html` §regn: X carries the
        // region selector — 0 NTSC, 1 PAL, 2 Dendy.
        let x = match self.header.region {
            NsfRegion::Pal => 1,
            NsfRegion::Dendy => 2,
            NsfRegion::Ntsc | NsfRegion::Dual => 0,
        };

        // The documented pre-INIT scrub (§"Initializing a tune"):
        // clear $0000-$07FF + $6000-$7FFF, zero the sound registers,
        // $00-then-$0F to $4015, $40 to $4017, and re-seed the bank
        // registers from the header. This makes every song start from
        // the same machine state — switching tracks no longer leaks
        // the previous song's RAM contents or APU register state.
        self.bus.reset_for_tune(&self.header);

        // NSF2 vector-overlay paradigms need the wrapper installed
        // BEFORE the first INIT call so the program can poke its IRQ
        // vector at $FFFE during INIT. Plain NSF v1 / NSF2-without-
        // overlay just runs INIT directly.
        if self.header.nsf2.needs_vector_overlay() {
            self.install_nmi_wrapper();
        }

        // First-phase INIT. For NSF2 non-returning-INIT this is the
        // "Y = $80" pre-pass that MUST return (spec).
        let y_first = if self.nsf2_nmi_play { 0x80 } else { 0x00 };
        self.invoke_init(a, x, y_first);

        if self.nsf2_nmi_play {
            // Second-phase INIT: Y = $81. Runs indefinitely; PLAY
            // arrives as NMI. We push the same stop-sentinel so that
            // *if* the program does happen to RTS the fallback infinite
            // loop is our stop window (spec: "The second INIT is
            // allowed to return, in which case the player should fall
            // back to its own infinite loop").
            let ret_minus1 = STOP_SENTINEL.wrapping_sub(1);
            self.cpu.push_word_pub(&mut self.bus, ret_minus1);
            self.cpu.a = a;
            self.cpu.x = x;
            self.cpu.y = 0x81;
            self.cpu.pc = self.header.init_addr;
            // We do NOT run-to-stop here: the second INIT may never
            // return. The render loop will tick it cycle-by-cycle and
            // schedule NMIs at the PLAY period.
        }

        self.started = true;
        self.cycles_since_play = 0;
        self.sample_acc = 0.0;
        self.filter.reset();
    }

    /// Push the stop sentinel, seed registers, and jump to INIT.
    /// Run-to-stop is appropriate for any INIT call that must return
    /// (v1 INIT, NSF2 first-phase Y=$80 INIT).
    fn invoke_init(&mut self, a: u8, x: u8, y: u8) {
        self.cpu.a = a;
        self.cpu.x = x;
        self.cpu.y = y;
        self.cpu.sp = 0xFD;
        self.cpu.p = 0x24; // I + U set
        self.cpu.halted = false;
        let ret_minus1 = STOP_SENTINEL.wrapping_sub(1);
        self.cpu.push_word_pub(&mut self.bus, ret_minus1);
        self.cpu.pc = self.header.init_addr;
        self.run_until_stop();
    }

    /// Run the CPU until PC reaches the stop-sentinel window, the CPU
    /// halts, or we exceed [`MAX_CYCLES_PER_CALL`] cycles.
    fn run_until_stop(&mut self) {
        let mut spent = 0u32;
        while !self.cpu.halted && spent < MAX_CYCLES_PER_CALL {
            // RTS pops PC and adds 1, so the post-RTS PC equals
            // STOP_SENTINEL exactly.
            if self.cpu.pc == STOP_SENTINEL {
                break;
            }
            let cy = self.cpu.step(&mut self.bus);
            spent = spent.saturating_add(cy);
        }
    }

    /// Run CPU cycles + emit one sample at the player's output rate.
    /// Returns the i16 PCM sample.
    fn step_one_sample(&mut self) -> i16 {
        if !self.started {
            return 0;
        }
        // Spend `cycles_per_sample` worth of CPU cycles (fractional).
        self.sample_acc += self.cycles_per_sample;
        let target = self.sample_acc as u32;
        self.sample_acc -= target as f64;

        let mut spent = 0u32;
        let mut cpu_steps_this_sample = 0u32;
        while spent < target {
            // Schedule the next PLAY/NMI event when its period elapses.
            if !self.nsf2_suppress_play && self.cycles_since_play >= self.play_period_cycles {
                self.cycles_since_play -= self.play_period_cycles;
                if self.nsf2_nmi_play {
                    // NMI wrapper runs PLAY and RTI's back to INIT.
                    self.bus.request_nmi();
                } else if self.cpu.pc == STOP_SENTINEL {
                    // Classic JSR path: only re-arm PLAY when the
                    // previous one has finished (PC parked at sentinel).
                    self.invoke_play();
                }
                continue;
            }

            // If we're idling at the sentinel and not yet ready for the
            // next play, just tick the APU clock — don't execute the
            // open-bus garbage at $4FFF. EXCEPT: when the bus is
            // asserting an IRQ (NSF2 timer device) we still need to
            // step the CPU so it vectors through $FFFE.
            if self.cpu.pc == STOP_SENTINEL
                && !self.nsf2_nmi_play
                && !self.bus.irq_line()
                && !self.bus.nmi_pending
            {
                let leftover = target.saturating_sub(spent);
                let need = if self.nsf2_suppress_play {
                    leftover
                } else {
                    self.play_period_cycles
                        .saturating_sub(self.cycles_since_play)
                        .min(leftover)
                };
                let chunk = need.max(1);
                self.bus.tick_cycles(chunk);
                // DMC sample DMA steals CPU cycles even while we idle
                // at the sentinel — fold the stall into the elapsed
                // time so the PLAY cadence stays true to the APU state.
                let elapsed = chunk.saturating_add(self.bus.take_dmc_stall());
                self.cycles_since_play = self.cycles_since_play.saturating_add(elapsed);
                spent = spent.saturating_add(elapsed);
                continue;
            }

            // Inside an init / play subroutine (or the non-returning
            // INIT body): step the CPU. Cap the total steps so a
            // runaway routine cannot freeze the sample loop.
            let cy = self.cpu.step(&mut self.bus);
            spent = spent.saturating_add(cy);
            self.cycles_since_play = self.cycles_since_play.saturating_add(cy);
            cpu_steps_this_sample = cpu_steps_this_sample.saturating_add(1);
            if cpu_steps_this_sample > 10_000 {
                // Routine ran past its budget — break out so the host
                // can decide what to do (the next render call will
                // resume from wherever the CPU is parked).
                break;
            }
        }
        // Raw mixer output is in [0, ~1] (the APU DACs only swing
        // positive). Run it through the documented post-DAC analog
        // filter chain, which removes the DC bias (centring the signal
        // about zero) and rolls off the harshest highs, then scale to
        // i16 with a touch of headroom.
        let level = self.bus.apu.output_sample();
        let filtered = self.filter.process(level);
        (filtered * 28_000.0).clamp(-32_768.0, 32_767.0) as i16
    }

    fn invoke_play(&mut self) {
        let ret_minus1 = STOP_SENTINEL.wrapping_sub(1);
        self.cpu.push_word_pub(&mut self.bus, ret_minus1);
        self.cpu.pc = self.header.play_addr;
    }

    /// Fill `out` with mono i16 PCM samples. Returns the number of
    /// frames written (always `out.len()` unless the player has
    /// halted).
    pub fn render(&mut self, out: &mut [i16]) -> usize {
        for (i, slot) in out.iter_mut().enumerate() {
            if self.cpu.halted {
                return i;
            }
            *slot = self.step_one_sample();
        }
        out.len()
    }

    /// Read-only view of the configured output sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn cpu_hz(&self) -> u32 {
        self.cpu_hz
    }

    /// Number of entries in the NSFe `plst` music playlist (zero if
    /// the file ships none). Per `docs/audio/nsf/nsfe-nesdev-wiki.html`
    /// §plst the chunk is a flat sequence of 1-byte song-indexes (the
    /// values are 0-based song indexes; we surface them unchanged).
    pub fn playlist_len(&self) -> usize {
        self.header.metadata.playlist.len()
    }

    /// Number of entries in the NSFe `psfx` sound-effect playlist
    /// (separate from `plst` per spec — typically used by ripping
    /// tools to flag the SFX bank for non-musical sound tests).
    pub fn sfx_playlist_len(&self) -> usize {
        self.header.metadata.sfx_playlist.len()
    }

    /// 1-based song number for entry `index` of the `plst` playlist,
    /// or `None` when no playlist exists or `index` is out of range.
    /// `plst` stores 0-based song indexes; this getter converts to the
    /// 1-based convention used by [`NsfPlayer::start_song`] /
    /// [`NsfPlayer::start_playlist_entry`].
    pub fn playlist_song(&self, index: usize) -> Option<u8> {
        self.header
            .metadata
            .playlist
            .get(index)
            .map(|&zero_based| zero_based.saturating_add(1))
    }

    /// 1-based song number for entry `index` of the `psfx` playlist,
    /// using the same 0→1 base conversion as [`playlist_song`].
    pub fn sfx_playlist_song(&self, index: usize) -> Option<u8> {
        self.header
            .metadata
            .sfx_playlist
            .get(index)
            .map(|&zero_based| zero_based.saturating_add(1))
    }

    /// Start the `index`-th entry of the music playlist. Equivalent to
    /// `start_song(self.playlist_song(index)?)`. Returns the 1-based
    /// song number that was started, or `None` when no such entry
    /// exists.
    pub fn start_playlist_entry(&mut self, index: usize) -> Option<u8> {
        let song = self.playlist_song(index)?;
        self.start_song(song);
        Some(song)
    }

    /// Iterator over the resolved 1-based song numbers of the `plst`
    /// playlist. Convenience for callers that want to walk every track.
    pub fn playlist_iter(&self) -> impl Iterator<Item = u8> + '_ {
        self.header
            .metadata
            .playlist
            .iter()
            .map(|&zero_based| zero_based.saturating_add(1))
    }
}

/// Convenience: load + start the requested song with a fresh CPU /
/// bus / APU. Returns a primed [`NsfPlayer`] ready for [`NsfPlayer::render`].
pub fn open(header: NsfHeader, song: u8, sample_rate: u32) -> NsfPlayer {
    let mut p = NsfPlayer::new(header, sample_rate);
    p.start_song(song);
    p
}

/// Standalone helper: assemble a tiny NSF program for tests.
#[doc(hidden)]
pub fn _tiny_test_program() -> ([u8; 0x80], Vec<u8>) {
    // init: enable pulse 1, set duty/period, RTS.
    // play: NOP, RTS.
    // The NSF header points init at $8000, play at $8010.
    let mut header = [0u8; 0x80];
    header[..5].copy_from_slice(&crate::header::NSF_MAGIC);
    header[0x05] = 1;
    header[0x06] = 1;
    header[0x07] = 1;
    header[0x08..0x0A].copy_from_slice(&0x8000u16.to_le_bytes()); // load
    header[0x0A..0x0C].copy_from_slice(&0x8000u16.to_le_bytes()); // init
    header[0x0C..0x0E].copy_from_slice(&0x8010u16.to_le_bytes()); // play
    header[0x6E..0x70].copy_from_slice(&16666u16.to_le_bytes());
    // Move play routine to $8030 to leave room for the init.
    header[0x0C..0x0E].copy_from_slice(&0x8030u16.to_le_bytes()); // play
    let mut prog: Vec<u8> = vec![
        // $8000: init
        0xA9, 0x01, // LDA #$01
        0x8D, 0x15, 0x40, // STA $4015 (enable pulse 1)
        0xA9, 0xBF, // LDA #$BF (duty 50, halt, vol 15)
        0x8D, 0x00, 0x40, // STA $4000
        0xA9, 0x40, // LDA #$40
        0x8D, 0x02, 0x40, // STA $4002 (period lo)
        0xA9, 0x00, // LDA #$00
        0x8D, 0x03, 0x40, // STA $4003 (period hi + length-counter load)
        0x60, // RTS
    ];
    while prog.len() < 0x30 {
        prog.push(0xEA);
    }
    // $8030: play — NOP, RTS.
    prog.push(0xEA);
    prog.push(0x60);
    (header, prog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::parse_nsf;

    #[test]
    fn output_filter_removes_dc_bias() {
        // A constant input (pure DC, like a held silenced channel) must
        // decay toward zero through the two high-pass stages.
        let mut f = OutputFilter::new(44_100);
        let mut last = 0.0;
        for _ in 0..44_100 {
            last = f.process(0.5);
        }
        assert!(last.abs() < 1e-3, "DC not removed: {last}");
    }

    #[test]
    fn output_filter_passes_audible_ac() {
        // A 1 kHz square-ish alternating signal (well inside the
        // 90 Hz..14 kHz passband) must survive with substantial
        // amplitude rather than being filtered to nothing.
        let mut f = OutputFilter::new(44_100);
        // Prime to steady state, then measure peak-to-peak.
        let mut peak = 0.0f32;
        for n in 0..44_100 {
            // ~1 kHz: 44 samples per cycle, swing 0..1 about 0.5.
            let x = if (n / 22) % 2 == 0 { 1.0 } else { 0.0 };
            let y = f.process(x);
            if n > 4_410 {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak > 0.2, "audible AC over-attenuated: peak {peak}");
    }

    #[test]
    fn output_filter_coefficients_are_in_unit_range() {
        let f = OutputFilter::new(44_100);
        for a in [f.hp90_alpha, f.hp440_alpha, f.lp_alpha] {
            assert!((0.0..=1.0).contains(&a), "filter alpha out of range: {a}");
        }
        // The 14 kHz low-pass is near Nyquist at 44.1 kHz, so its alpha
        // is large; the 90 Hz high-pass alpha is very close to 1.
        assert!(f.hp90_alpha > 0.98);
        assert!(f.hp90_alpha > f.hp440_alpha);
    }

    #[test]
    fn renders_nonzero_pcm_from_tiny_program() {
        let (hdr_bytes, prog) = _tiny_test_program();
        let mut whole = hdr_bytes.to_vec();
        whole.extend_from_slice(&prog);
        let header = parse_nsf(&whole).unwrap();
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        let mut buf = vec![0i16; 4096];
        let n = player.render(&mut buf);
        assert_eq!(n, buf.len(), "player halted prematurely");
        let nonzero = buf.iter().any(|&s| s != 0);
        assert!(nonzero, "tiny test program produced no audio");
        // Peak should be safely below i16 limits but well above noise.
        let peak = buf.iter().map(|s| s.unsigned_abs() as u32).max().unwrap();
        assert!(peak > 1000, "peak {peak} too quiet");
    }

    #[test]
    fn play_period_is_honoured() {
        let (hdr_bytes, prog) = _tiny_test_program();
        let mut whole = hdr_bytes.to_vec();
        whole.extend_from_slice(&prog);
        let header = parse_nsf(&whole).unwrap();
        let mut player = NsfPlayer::new(header, 44_100);
        let expected = ((16666u64 * 1_789_773) / 1_000_000) as u32;
        assert_eq!(player.play_period_cycles, expected);
        player.start_song(1);
    }

    /// Build an NSF2 file with the requested feature byte. Program is
    /// always a stub: INIT increments `$00`, returns on Y=$80, then
    /// loops on Y=$81. PLAY toggles `$01`. INIT_ADDR=$8000, PLAY=$8050.
    fn fake_nsf2(features: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 0x80];
        buf[..5].copy_from_slice(&crate::header::NSF_MAGIC);
        buf[0x05] = 2;
        buf[0x06] = 1;
        buf[0x07] = 1;
        buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0a..0x0c].copy_from_slice(&0x8000u16.to_le_bytes()); // init
        buf[0x0c..0x0e].copy_from_slice(&0x8050u16.to_le_bytes()); // play
        buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes());
        buf[0x7c] = features;

        // INIT at $8000:
        //   E6 00       INC $00            ; bump init-call counter
        //   C0 81       CPY #$81
        //   F0 01       BEQ +1 → JMP at $8007 (skip the RTS)
        //   60          RTS                ; Y != $81 → return
        // loop ($8007):
        //   4C 07 80    JMP loop
        let init = [
            0xE6, 0x00, // INC $00
            0xC0, 0x81, // CPY #$81
            0xF0, 0x01, // BEQ +1 → land at $8007 (JMP)
            0x60, // RTS at $8006
            0x4C, 0x07, 0x80, // JMP $8007 (spin) at $8007..=$8009
        ];

        // PLAY at $8050:
        //   E6 01       INC $01            ; bump play-call counter
        //   60          RTS
        let play_at = 0x50usize;
        let play = [0xE6, 0x01, 0x60];

        let mut prog = vec![0xEAu8; play_at + play.len()];
        prog[..init.len()].copy_from_slice(&init);
        prog[play_at..play_at + play.len()].copy_from_slice(&play);

        buf.extend_from_slice(&prog);
        buf
    }

    #[test]
    fn nsf2_non_returning_init_runs_init_twice_with_correct_y() {
        let bytes = fake_nsf2(0x20); // non-returning INIT, no IRQ, no suppress
        let header = parse_nsf(&bytes).unwrap();
        assert!(header.nsf2.non_returning_init());
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        // After start_song the first-phase INIT has completed (Y=$80
        // returning path) → `$00` = 1. The second-phase INIT (Y=$81)
        // is staged but not yet stepped; render a few samples to let
        // its `INC $00` execute (it will then enter the spin loop).
        assert_eq!(player.bus.ram[0x00], 1);
        let mut buf = vec![0i16; 512];
        let _ = player.render(&mut buf);
        assert_eq!(
            player.bus.ram[0x00], 2,
            "INIT must be called twice for non-returning paradigm"
        );
        // After the second INIT the CPU spins inside $8007..$800A.
        assert!(
            player.cpu.pc >= 0x8007 && player.cpu.pc <= 0x800A,
            "second INIT should be spinning, got PC = ${:04X}",
            player.cpu.pc
        );
    }

    #[test]
    fn nsf2_non_returning_init_drives_play_via_nmi() {
        let bytes = fake_nsf2(0x20);
        let header = parse_nsf(&bytes).unwrap();
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        let mut buf = vec![0i16; 8192]; // ~186 ms at 44.1 kHz
        let _ = player.render(&mut buf);
        let play_calls = player.bus.ram[0x01];
        // 186 ms at 60 Hz → ~11 play calls expected. Tolerate jitter.
        assert!(
            play_calls >= 5,
            "expected several NMI-driven PLAYs, got {play_calls}"
        );
    }

    #[test]
    fn nsf2_suppressed_play_skips_play_entirely() {
        let bytes = fake_nsf2(0x20 | 0x40); // non-returning INIT + suppress PLAY
        let header = parse_nsf(&bytes).unwrap();
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        let mut buf = vec![0i16; 8192];
        let _ = player.render(&mut buf);
        assert_eq!(
            player.bus.ram[0x01], 0,
            "suppressed PLAY must never invoke the play routine"
        );
    }

    #[test]
    fn nsf2_irq_feature_arms_timer_device() {
        let bytes = fake_nsf2(0x10); // IRQ support only
        let header = parse_nsf(&bytes).unwrap();
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        assert!(
            player.bus.nsf2_timer.enabled,
            "NSF2 IRQ feature must enable the timer device"
        );
        // Vector overlay should be armed.
        assert!(player.bus.vector_overlay_active);
        // NMI wrapper installed.
        assert_eq!(
            player.bus.ram[NMI_WRAPPER_ADDR as usize], 0x48,
            "NMI wrapper should start with PHA ($48)"
        );
    }

    #[test]
    fn nsfe_vrc7_chunk_reaches_the_synthesis_path() {
        // Per `docs/audio/nsf/nsfe-nesdev-wiki.html` §VRC7 the chunk
        // must actually change the instrument set the chip decodes —
        // device 1 selects the YM2413 default ROM.
        let (hdr_bytes, prog) = _tiny_test_program();
        let mut whole = hdr_bytes.to_vec();
        whole.extend_from_slice(&prog);
        let mut header = parse_nsf(&whole).unwrap();
        header.metadata.vrc7 = Some(crate::nsfe::NsfeVrc7 {
            device: 1,
            patches: None,
        });
        let player = NsfPlayer::new(header, 44_100);
        assert!(
            player.bus.apu.expansion.vrc7.ym2413_variant,
            "player must forward the NSFe VRC7 chunk to the chip"
        );
    }

    #[test]
    fn v1_player_does_not_arm_vector_overlay() {
        let (hdr_bytes, prog) = _tiny_test_program();
        let mut whole = hdr_bytes.to_vec();
        whole.extend_from_slice(&prog);
        let header = parse_nsf(&whole).unwrap();
        assert!(!header.nsf2.needs_vector_overlay());
        let mut player = NsfPlayer::new(header, 44_100);
        player.start_song(1);
        assert!(!player.bus.vector_overlay_active);
        assert!(!player.bus.nsf2_timer.enabled);
    }
}
