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

use crate::bus::NesBus;
use crate::cpu::Cpu6502;
use crate::header::{NsfHeader, NsfRegion};

/// NTSC NES CPU clock.
const NTSC_CPU_HZ: u32 = 1_789_773;
/// PAL NES CPU clock.
const PAL_CPU_HZ: u32 = 1_662_607;

/// Sentinel return address pushed under the init / play subroutine
/// frame. When the routine RTS's, the CPU jumps here and we stop
/// executing for the current period.
const STOP_SENTINEL: u16 = 0x4FFF;

/// Maximum number of CPU cycles we'll spend inside one `init` or
/// `play` invocation. NSF rips that get stuck (or never RTS) would
/// otherwise hang the player indefinitely.
const MAX_CYCLES_PER_CALL: u32 = 1_000_000;

/// Player state machine.
pub struct NsfPlayer {
    pub cpu: Cpu6502,
    pub bus: NesBus,
    pub header: NsfHeader,
    cpu_hz: u32,
    sample_rate: u32,
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
}

impl NsfPlayer {
    /// Build a player for `header` at `sample_rate` Hz.
    pub fn new(header: NsfHeader, sample_rate: u32) -> Self {
        let cpu_hz = match header.region {
            NsfRegion::Pal => PAL_CPU_HZ,
            NsfRegion::Ntsc | NsfRegion::Dual => NTSC_CPU_HZ,
        };
        let mut bus = NesBus::new();
        bus.apu.set_cpu_hz(cpu_hz);
        bus.load_program(header.load_addr, &header.program);

        let play_us = match header.region {
            NsfRegion::Pal => header.pal_speed_us,
            NsfRegion::Ntsc | NsfRegion::Dual => header.ntsc_speed_us,
        };
        let play_us_eff = if play_us != 0 {
            play_us as u32
        } else {
            match header.region {
                NsfRegion::Pal => 19_997,
                NsfRegion::Ntsc | NsfRegion::Dual => 16_666,
            }
        };
        let play_period_cycles = ((play_us_eff as u64 * cpu_hz as u64) / 1_000_000) as u32;

        Self {
            cpu: Cpu6502::new(),
            bus,
            header,
            cpu_hz,
            sample_rate,
            play_period_cycles,
            cycles_since_play: 0,
            cycles_per_sample: cpu_hz as f64 / sample_rate as f64,
            sample_acc: 0.0,
            song: 0,
            started: false,
        }
    }

    /// Choose `song` (1-based) and run its `init` routine to completion.
    pub fn start_song(&mut self, song: u8) {
        self.song = song;
        self.cpu.a = song.saturating_sub(1);
        self.cpu.x = match self.header.region {
            NsfRegion::Pal => 1,
            _ => 0,
        };
        self.cpu.y = 0;
        self.cpu.sp = 0xFD;
        self.cpu.p = 0x24; // I + U set
        self.cpu.halted = false;
        // Push the sentinel return address so the init's RTS lands in
        // our stop window.
        let ret_minus1 = STOP_SENTINEL.wrapping_sub(1);
        self.cpu.push_word_pub(&mut self.bus, ret_minus1);
        self.cpu.pc = self.header.init_addr;
        self.run_until_stop();
        self.started = true;
        self.cycles_since_play = 0;
        self.sample_acc = 0.0;
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
            // If we're idling at the sentinel and not yet ready for the
            // next play, just tick the APU clock — don't execute the
            // open-bus garbage at $4FFF.
            if self.cpu.pc == STOP_SENTINEL {
                if self.cycles_since_play >= self.play_period_cycles {
                    self.cycles_since_play -= self.play_period_cycles;
                    self.invoke_play();
                    continue;
                }
                let need = self
                    .play_period_cycles
                    .saturating_sub(self.cycles_since_play);
                let leftover = target.saturating_sub(spent);
                let chunk = need.min(leftover).max(1);
                self.bus.tick_cycles(chunk);
                self.cycles_since_play = self.cycles_since_play.saturating_add(chunk);
                spent = spent.saturating_add(chunk);
                continue;
            }

            // Inside an init / play subroutine: step the CPU. Cap the
            // total steps so a runaway routine cannot freeze the
            // sample loop.
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
        let level = self.bus.apu.output_sample();
        // level is in [0, ~1]. Scale to i16 with a touch of headroom.
        (level * 28_000.0).clamp(-32_768.0, 32_767.0) as i16
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
}
