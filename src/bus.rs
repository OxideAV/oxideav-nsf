//! 64 KiB NES address-space bus.
//!
//! NSF only models the subset of the NES that matters for music
//! playback:
//!
//! | Range            | Mapping                                                    |
//! |------------------|------------------------------------------------------------|
//! | `$0000..=$07FF`  | 2 KiB CPU RAM                                               |
//! | `$0800..=$1FFF`  | three mirrors of the 2 KiB CPU RAM                          |
//! | `$2000..=$3FFF`  | PPU registers (open bus — NSF does not draw video)          |
//! | `$4000..=$4013`  | APU register file (pulse / triangle / noise / DMC)          |
//! | `$4014`          | OAM DMA — open bus for NSF                                  |
//! | `$4015`          | APU status                                                  |
//! | `$4016`          | controller 1 strobe (open bus for NSF)                      |
//! | `$4017`          | APU frame counter / controller 2 strobe                     |
//! | `$4018..=$4FFF`  | open bus / 2A03 expansion register space                    |
//! | `$5000..=$5FF7`  | MMC5 / VRC7 / N163 / 5B expansion register window           |
//! | `$5FF6..=$5FF7`  | NSF 2.x extra bank-select for `$6000` / `$7000` (FDS)       |
//! | `$5FF8..=$5FFF`  | NSF bank-select registers for `$8000..=$FFFF`               |
//! | `$6000..=$7FFF`  | 8 KiB optional cartridge RAM (some NSFs use it)             |
//! | `$8000..=$FFFF`  | NSF program ROM (loaded from `load_addr` upward)            |
//!
//! ## Bankswitching
//!
//! When the NSF header's `bankswitch_init` field is non-zero, the
//! program ROM is treated as a contiguous blob of 4 KiB banks; the
//! eight bank-select registers `$5FF8..=$5FFF` map one bank into each
//! of the eight 4 KiB windows that tile `$8000..=$FFFF`. The header's
//! `bankswitch_init` array seeds the registers up front. NSFs without
//! bankswitching (every byte is zero) load flat into PRG ROM.
//!
//! ## Expansion chips
//!
//! Writes inside `$5000..=$5FFF` and `$9000..=$FFFF` (for chips that
//! re-use the program ROM window for register access) are forwarded to
//! the registered expansion chips. The bus owns no chip state itself —
//! it just routes `(addr, value)` pairs to [`crate::apu::Apu2A03`]
//! which holds the per-chip mixers.

use crate::apu::Apu2A03;
use crate::header::NsfHeader;

// =====================================================================
// NSF2 IRQ timer device
// =====================================================================
//
// Per `docs/audio/nsf/nsf2-nesdev-wiki.html` §IRQ Timer, a cycle-counting
// timer hangs off three new player-side registers:
//
// | Reg    | Read                                      | Write                                |
// | ------ | ----------------------------------------- | ------------------------------------ |
// | $401B  | low 8 bits of counter reload              | low 8 bits of counter reload         |
// | $401C  | high 8 bits of counter reload             | high 8 bits of counter reload        |
// | $401D  | bit7=IRQ flag (clear-on-read), bit0=active| bit0: 1 = activate, 0 = deactivate   |
//
// When active, the counter decrements every CPU cycle. On underflow
// (going below 0) the IRQ flag latches and the counter is reloaded with
// `(reload_hi << 8) | reload_lo`. While the IRQ flag is set the IRQ line
// is asserted; reading $401D clears the flag. When inactive the counter
// is reloaded every cycle (held at `reload`).
//
// Reload value `N` ⇒ the IRQ repeats every `N+1` cycles.

#[derive(Clone, Default)]
pub struct Nsf2IrqTimer {
    /// Currently-feature-gated on; off for v1 / NSF2 without the IRQ
    /// support feature flag.
    pub enabled: bool,
    /// Latch for the next reload value (lo byte at $401B, hi at $401C).
    pub reload: u16,
    /// Live counter; decremented every cycle when `active`.
    pub counter: i32,
    /// Reflects bit 0 of writes to $401D.
    pub active: bool,
    /// Set on underflow, cleared on read of $401D. Drives the IRQ line.
    pub irq_flag: bool,
}

impl Nsf2IrqTimer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        if !self.enabled {
            return;
        }
        match addr {
            0x401B => self.reload = (self.reload & 0xFF00) | value as u16,
            0x401C => self.reload = (self.reload & 0x00FF) | ((value as u16) << 8),
            0x401D => {
                self.active = value & 0x01 != 0;
                // Spec says reload happens automatically every cycle
                // while inactive; mirror that by snapping the counter
                // to the reload value when the toggle flips.
                if !self.active {
                    self.counter = self.reload as i32;
                }
            }
            _ => {}
        }
    }

    pub fn read(&mut self, addr: u16) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        match addr {
            0x401B => Some(self.reload as u8),
            0x401C => Some((self.reload >> 8) as u8),
            0x401D => {
                let v = (self.irq_flag as u8) << 7 | (self.active as u8);
                // The read acknowledges and clears the IRQ flag per spec
                // ("Bit 7 returns IRQ flag before clearing it").
                self.irq_flag = false;
                Some(v)
            }
            _ => None,
        }
    }

    pub fn tick(&mut self, cycles: u32) {
        if !self.enabled {
            return;
        }
        if !self.active {
            // Held at reload while inactive.
            self.counter = self.reload as i32;
            return;
        }
        for _ in 0..cycles {
            self.counter -= 1;
            if self.counter < 0 {
                self.irq_flag = true;
                self.counter = self.reload as i32;
            }
        }
    }

    /// True iff the timer is currently asserting the CPU's IRQ line.
    pub fn irq_line(&self) -> bool {
        self.enabled && self.irq_flag
    }
}

/// 2 KiB of work RAM.
pub const RAM_SIZE: usize = 0x0800;

/// 8 KiB of optional cartridge RAM at `$6000..=$7FFF`.
pub const CART_RAM_SIZE: usize = 0x2000;

/// 32 KiB of program ROM at `$8000..=$FFFF`. Used as the flat backing
/// store for non-bankswitched NSFs.
pub const PRG_ROM_SIZE: usize = 0x8000;

/// 4 KiB bank size — what the NSF bank-switch registers index.
pub const BANK_SIZE: usize = 0x1000;

/// Number of windows that tile `$8000..=$FFFF` at 4 KiB each.
pub const NUM_BANK_WINDOWS: usize = 8;

/// 64 KiB CPU view of the NES bus.
pub struct NesBus {
    pub ram: [u8; RAM_SIZE],
    pub cart_ram: [u8; CART_RAM_SIZE],
    /// Flat 32 KiB ROM image (no bankswitching). Used when
    /// [`NesBus::bankswitched`] is false.
    pub prg: [u8; PRG_ROM_SIZE],
    pub apu: Apu2A03,
    pub cycles: u64,

    // ---- Bankswitching state ----
    /// Pool of 4 KiB banks built from the NSF blob; addressed by the
    /// 8-bit bank-select registers.
    pub bank_pool: Vec<[u8; BANK_SIZE]>,
    /// `$5FF8..=$5FFF` mapping: which pool entry backs each window.
    pub bank_select: [u8; NUM_BANK_WINDOWS],
    /// True iff the header advertised non-zero bankswitching state.
    pub bankswitched: bool,

    /// True iff the FDS expansion chip is enabled — `$8000..=$FFFF`
    /// becomes RAM rather than ROM (writes go through).
    pub fds_enabled: bool,
    /// FDS RAM image at `$8000..=$FFFF` — populated from the program
    /// blob and writable.
    pub fds_ram: Vec<u8>,

    // ---- NSF2 IRQ / vector overlay ----
    /// NSF2 IRQ timer device (`$401B..=$401D`). Inactive until the
    /// header advertises the IRQ-support feature.
    pub nsf2_timer: Nsf2IrqTimer,
    /// True when the player has installed the `$FFFA..=$FFFF` vector
    /// overlay (required by the NSF2 IRQ + non-returning-INIT
    /// features). When set, reads of `$FFFA..=$FFFF` come from
    /// `vector_overlay` regardless of the underlying ROM/RAM, and
    /// writes to `$FFFE..=$FFFF` are captured into the overlay so the
    /// NSF program can install its own IRQ vector.
    pub vector_overlay_active: bool,
    /// 6 bytes of vector overlay at `$FFFA..=$FFFF` — NMI lo/hi,
    /// Reset lo/hi, IRQ lo/hi. NMI/Reset are reserved to the player;
    /// IRQ is owned by the NSF program.
    pub vector_overlay: [u8; 6],
    /// Pending NMI request; set by the player when it wants to vector
    /// the CPU through `$FFFA` (used to implement the NSF2
    /// non-returning INIT / NMI-driven PLAY path). Drained by the CPU
    /// on the next `step`.
    pub nmi_pending: bool,
}

impl Default for NesBus {
    fn default() -> Self {
        Self::new()
    }
}

impl NesBus {
    pub fn new() -> Self {
        Self {
            ram: [0u8; RAM_SIZE],
            cart_ram: [0u8; CART_RAM_SIZE],
            prg: [0u8; PRG_ROM_SIZE],
            apu: Apu2A03::new(),
            cycles: 0,
            bank_pool: Vec::new(),
            bank_select: [0u8; NUM_BANK_WINDOWS],
            bankswitched: false,
            fds_enabled: false,
            fds_ram: Vec::new(),
            nsf2_timer: Nsf2IrqTimer::new(),
            vector_overlay_active: false,
            vector_overlay: [0u8; 6],
            nmi_pending: false,
        }
    }

    /// Arm the NSF2 vector overlay at `$FFFA..=$FFFF`. The IRQ vector
    /// at `$FFFE..=$FFFF` is preloaded from whatever the underlying
    /// memory map returns (per spec — "before INIT the host system
    /// should initialize the IRQ vector RAM with the starting contents
    /// of $FFFE-$FFFF"). NMI / Reset are reserved to the player and
    /// initialised to a sentinel that lands inside our stop window.
    pub fn arm_vector_overlay(&mut self, nmi_handler: u16, reset_handler: u16) {
        // Preload the IRQ vector from whatever the underlying ROM
        // already had at $FFFE/$FFFF.
        let lo = self.read_raw_vector(0xFFFE);
        let hi = self.read_raw_vector(0xFFFF);
        self.vector_overlay[0] = nmi_handler as u8;
        self.vector_overlay[1] = (nmi_handler >> 8) as u8;
        self.vector_overlay[2] = reset_handler as u8;
        self.vector_overlay[3] = (reset_handler >> 8) as u8;
        self.vector_overlay[4] = lo;
        self.vector_overlay[5] = hi;
        self.vector_overlay_active = true;
    }

    fn read_raw_vector(&self, addr: u16) -> u8 {
        // Use the ROM / bank-resolved value directly — bypasses the
        // overlay so we can snapshot the "starting contents" cleanly.
        match addr {
            0x8000..=0xFFFF => {
                if self.bankswitched {
                    self.bank_read(addr)
                } else {
                    self.prg[(addr - 0x8000) as usize]
                }
            }
            _ => 0,
        }
    }

    /// Request an NMI on the next CPU step. Used by the NSF2
    /// non-returning-INIT path: the player schedules an NMI at the
    /// PLAY period; the NMI wrapper runs PLAY and RTI's back to INIT.
    pub fn request_nmi(&mut self) {
        self.nmi_pending = true;
    }

    /// Drain a pending NMI request (true exactly once per request).
    pub fn take_nmi(&mut self) -> bool {
        let p = self.nmi_pending;
        self.nmi_pending = false;
        p
    }

    /// True iff the bus is asserting the CPU's IRQ line. Three
    /// sources, OR'd together per nesdev wiki §APU + §NSF2:
    ///
    /// * the NSF2 timer device (`$401B/C/D` underflow, gated on the
    ///   header feature byte),
    /// * the APU frame-counter IRQ ($4017 bit-6 inhibit clear,
    ///   4-step mode end-of-frame),
    /// * the APU DMC IRQ ($4010 bit 7 set, sample stream finished).
    ///
    /// Each source latches its own flag and is acknowledged by its
    /// own register read (`$401D`, `$4015`, `$4015` respectively).
    pub fn irq_line(&self) -> bool {
        self.nsf2_timer.irq_line() || self.apu.irq_line()
    }

    /// Configure the bus from the parsed NSF header. This sets up the
    /// bank pool when bankswitching is in use, copies the program into
    /// flat ROM otherwise, and arms the expansion-chip outputs in the
    /// APU mixer.
    pub fn configure_from_header(&mut self, header: &NsfHeader) {
        let exp = header.expansion;
        self.apu.set_expansion(exp);
        self.fds_enabled = exp.fds();

        let bankswitched = header.bankswitch_init.iter().any(|&b| b != 0);
        if bankswitched {
            self.bankswitched = true;
            self.load_bankswitched(header);
        } else {
            self.bankswitched = false;
            self.load_program(header.load_addr, &header.program);
        }

        // NSF2: arm IRQ timer + vector overlay if the feature byte
        // requests it. The player layer drives `arm_vector_overlay`
        // with concrete sentinel addresses; we just gate the
        // timer-device address decode here.
        self.nsf2_timer.enabled = header.nsf2.irq_support();
    }

    /// Build the 4 KiB-bank pool out of the NSF program blob. Per the
    /// nesdev.org wiki: the load address determines the in-bank offset
    /// of the very first byte; bytes before that offset are zero-padded.
    fn load_bankswitched(&mut self, header: &NsfHeader) {
        let pad = (header.load_addr as usize) & (BANK_SIZE - 1);
        let mut linear = vec![0u8; pad];
        linear.extend_from_slice(&header.program);
        // Round up to a whole-bank boundary.
        let rem = linear.len() % BANK_SIZE;
        if rem != 0 {
            linear.resize(linear.len() + (BANK_SIZE - rem), 0);
        }
        let n_banks = linear.len() / BANK_SIZE;
        self.bank_pool = (0..n_banks)
            .map(|i| {
                let mut b = [0u8; BANK_SIZE];
                b.copy_from_slice(&linear[i * BANK_SIZE..(i + 1) * BANK_SIZE]);
                b
            })
            .collect();
        self.bank_select = header.bankswitch_init;
        if self.fds_enabled {
            // FDS extends bank-select to cover `$6000..=$7FFF` via
            // `$5FF6..=$5FF7`. Round 2: prime the FDS RAM image from the
            // bank pool.
            self.fds_ram = vec![0u8; 0x8000];
            for (window, &sel) in self.bank_select.iter().enumerate() {
                if let Some(bank) = self.bank_pool.get(sel as usize) {
                    let dst = window * BANK_SIZE;
                    self.fds_ram[dst..dst + BANK_SIZE].copy_from_slice(bank);
                }
            }
        }
    }

    /// Load the NSF program blob at `load_addr`. Bytes that fall past
    /// `$FFFF` are silently dropped. Used only when bankswitching is
    /// disabled.
    pub fn load_program(&mut self, load_addr: u16, program: &[u8]) {
        if load_addr < 0x8000 {
            let mut addr = load_addr as usize;
            for &b in program {
                if addr >= 0x8000 {
                    let off = addr - 0x8000;
                    if off < PRG_ROM_SIZE {
                        self.prg[off] = b;
                    }
                } else if (0x6000..0x8000).contains(&addr) {
                    self.cart_ram[addr - 0x6000] = b;
                }
                addr = addr.wrapping_add(1);
            }
            return;
        }
        let off = load_addr as usize - 0x8000;
        let n = program.len().min(PRG_ROM_SIZE.saturating_sub(off));
        self.prg[off..off + n].copy_from_slice(&program[..n]);
    }

    /// Resolve a `$8000..=$FFFF` CPU address through the active bank
    /// table to a byte in the bank pool. Returns 0xFF if the bank
    /// register points past the pool (genuinely undefined behaviour;
    /// real NES hardware floats the bus).
    fn bank_read(&self, addr: u16) -> u8 {
        if self.fds_enabled {
            return self.fds_ram[(addr - 0x8000) as usize];
        }
        let window = ((addr - 0x8000) / BANK_SIZE as u16) as usize;
        let off = (addr as usize) & (BANK_SIZE - 1);
        let sel = self.bank_select[window] as usize;
        match self.bank_pool.get(sel) {
            Some(bank) => bank[off],
            None => 0xFF,
        }
    }

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => 0xFF,
            0x4000..=0x4014 => 0xFF,
            0x4015 => self.apu.read_status(),
            0x4016 | 0x4017 => 0xFF,
            // NSF2 IRQ-timer reads ($401B-$401D). Fall through to the
            // expansion-chip read window if the timer is disabled.
            0x401B..=0x401D => {
                if let Some(v) = self.nsf2_timer.read(addr) {
                    v
                } else {
                    self.apu.read_expansion(addr)
                }
            }
            // 5B / N163 / MMC5 status reads land here.
            0x4018..=0x5FFF => self.apu.read_expansion(addr),
            0x6000..=0x7FFF => self.cart_ram[(addr - 0x6000) as usize],
            0xFFFA..=0xFFFF if self.vector_overlay_active => {
                self.vector_overlay[(addr - 0xFFFA) as usize]
            }
            0x8000..=0xFFFF => {
                let byte = if self.bankswitched {
                    self.bank_read(addr)
                } else {
                    self.prg[(addr - 0x8000) as usize]
                };
                // `docs/audio/nsf/mmc5-audio-wiki.html` §"Raw PCM ($5011)":
                // when MMC5 is enabled and in PCM read-mode, a CPU read
                // from `$8000..=$BFFF` "writes-by-read" the observed
                // byte into the MMC5 DAC update path (a `$00` byte
                // sets irqTrip instead of changing the DAC; non-zero
                // updates the DAC and clears irqTrip). The router is
                // a no-op outside that window and when read-mode is
                // off, and runs after the byte has been resolved so
                // the DAC sees the same byte the CPU observed.
                self.apu.observe_prg_read(addr, byte);
                byte
            }
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = value,
            0x2000..=0x3FFF => {}
            0x4000..=0x4013 => self.apu.write_register(addr, value),
            0x4014 => {}
            0x4015 => self.apu.write_status(value),
            0x4016 => {}
            0x4017 => self.apu.write_frame_counter(value),
            // NSF2 IRQ-timer writes. When disabled, fall through to
            // the expansion-chip router (preserves prior $401B-$401D
            // open-bus semantics for v1 / non-IRQ NSF2 files).
            0x401B..=0x401D => {
                if self.nsf2_timer.enabled {
                    self.nsf2_timer.write(addr, value);
                } else {
                    self.apu.write_expansion(addr, value);
                }
            }
            // NSF bank-select registers + expansion-chip writes.
            0x5FF6 | 0x5FF7 if self.fds_enabled => {
                // FDS bank-select for the `$6000..=$7FFF` region: copy
                // the selected bank into cart RAM at the matching offset.
                if let Some(bank) = self.bank_pool.get(value as usize) {
                    let off = if addr == 0x5FF6 { 0 } else { BANK_SIZE };
                    let n = BANK_SIZE.min(CART_RAM_SIZE - off);
                    self.cart_ram[off..off + n].copy_from_slice(&bank[..n]);
                }
            }
            0x5FF8..=0x5FFF => {
                let window = (addr - 0x5FF8) as usize;
                self.bank_select[window] = value;
                if self.fds_enabled {
                    if let Some(bank) = self.bank_pool.get(value as usize) {
                        let dst = window * BANK_SIZE;
                        self.fds_ram[dst..dst + BANK_SIZE].copy_from_slice(bank);
                    }
                }
            }
            0x4018..=0x5FF7 => self.apu.write_expansion(addr, value),
            0x6000..=0x7FFF => self.cart_ram[(addr - 0x6000) as usize] = value,
            0xFFFE..=0xFFFF if self.vector_overlay_active => {
                // NSF program installing its own IRQ vector. NMI /
                // Reset slots (`$FFFA-$FFFD`) are reserved to the
                // player per spec and ignore writes.
                self.vector_overlay[(addr - 0xFFFA) as usize] = value;
                if self.fds_enabled {
                    self.fds_ram[(addr - 0x8000) as usize] = value;
                }
            }
            0x8000..=0xFFFF => {
                // FDS treats this region as RAM; expansion chips like
                // VRC6 / VRC7 / MMC5 / N163 / 5B map their registers in
                // here. Forward to the APU expansion router which
                // ignores writes when no chip is interested.
                if self.fds_enabled {
                    self.fds_ram[(addr - 0x8000) as usize] = value;
                }
                self.apu.write_expansion(addr, value);
                // Plain ROM is read-only: do not touch self.prg.
            }
        }
    }

    /// Inform the bus that `cycles` CPU clocks elapsed; forwards them
    /// to the APU so the frame counter advances. The DMC fetcher pulls
    /// bytes back through this same bus on demand.
    pub fn tick_cycles(&mut self, cycles: u32) {
        self.cycles = self.cycles.wrapping_add(cycles as u64);
        // Run the APU; whenever the DMC needs a sample byte, fetch it
        // through this bus's read path. Doing this in two passes keeps
        // the borrow checker happy.
        let mut remaining = cycles;
        const CHUNK: u32 = 8;
        while remaining > 0 {
            let n = remaining.min(CHUNK);
            self.apu.tick_cpu_cycles(n);
            self.nsf2_timer.tick(n);
            // Drain pending DMC fetches.
            while let Some(addr) = self.apu.dmc_pending_fetch() {
                let byte = self.read(addr);
                self.apu.dmc_supply_byte(byte);
            }
            remaining -= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{ExpansionChips, NsfHeader, NsfRegion};

    fn fake_header(load: u16, prog: Vec<u8>, banks: [u8; 8]) -> NsfHeader {
        NsfHeader {
            version: 1,
            total_songs: 1,
            starting_song: 1,
            load_addr: load,
            init_addr: 0x8000,
            play_addr: 0x8003,
            song_name: String::new(),
            artist: String::new(),
            copyright: String::new(),
            ntsc_speed_us: 16666,
            pal_speed_us: 19997,
            bankswitch_init: banks,
            region: NsfRegion::Ntsc,
            expansion: ExpansionChips(0),
            program: prog,
            track_labels: Vec::new(),
            is_nsfe: false,
            nsf2: crate::header::Nsf2Features(0),
            nsf2_metadata: Vec::new(),
            metadata: crate::nsfe::NsfeMetadata::default(),
        }
    }

    #[test]
    fn flat_load_places_program_at_load_addr() {
        let mut bus = NesBus::new();
        let h = fake_header(0x8000, vec![0xAA, 0xBB, 0xCC], [0u8; 8]);
        bus.configure_from_header(&h);
        assert!(!bus.bankswitched);
        assert_eq!(bus.read(0x8000), 0xAA);
        assert_eq!(bus.read(0x8001), 0xBB);
        assert_eq!(bus.read(0x8002), 0xCC);
    }

    #[test]
    fn bankswitching_routes_through_bank_select() {
        // Two 4 KiB banks: bank0 filled with 0x11, bank1 with 0x22.
        let mut prog = vec![0x11; BANK_SIZE];
        prog.extend(std::iter::repeat_n(0x22, BANK_SIZE));
        // Header bank-select: window0 = bank0, window1 = bank1, rest zero.
        let mut banks = [0u8; 8];
        banks[1] = 1;
        let h = fake_header(0x8000, prog, banks);
        let mut bus = NesBus::new();
        bus.configure_from_header(&h);
        assert!(bus.bankswitched);
        // $8000..=$8FFF reads bank 0.
        assert_eq!(bus.read(0x8000), 0x11);
        assert_eq!(bus.read(0x8FFF), 0x11);
        // $9000..=$9FFF reads bank 1.
        assert_eq!(bus.read(0x9000), 0x22);
        assert_eq!(bus.read(0x9FFF), 0x22);
        // Now hot-swap window 0 to bank 1.
        bus.write(0x5FF8, 1);
        assert_eq!(bus.read(0x8000), 0x22);
    }

    #[test]
    fn writes_to_8000_window_are_dropped_without_fds() {
        let mut bus = NesBus::new();
        let h = fake_header(0x8000, vec![0x55], [0u8; 8]);
        bus.configure_from_header(&h);
        // Plain ROM: writes silently dropped; read still returns ROM.
        bus.write(0x8000, 0xFF);
        assert_eq!(bus.read(0x8000), 0x55);
    }

    #[test]
    fn nsf2_irq_timer_fires_after_n_plus_one_cycles() {
        let mut t = Nsf2IrqTimer::new();
        t.enabled = true;
        // Reload = 3 → period 4 cycles per spec.
        t.write(0x401B, 0x03);
        t.write(0x401C, 0x00);
        t.write(0x401D, 0x01); // activate
        t.counter = t.reload as i32;
        assert!(!t.irq_line());
        t.tick(4); // exactly N+1 cycles → first underflow at cycle 4
        assert!(t.irq_line(), "timer should assert IRQ after N+1 cycles");
        // Reading $401D acknowledges.
        let v = t.read(0x401D).unwrap();
        assert_eq!(v & 0x80, 0x80, "bit7 should reflect the pending IRQ");
        assert_eq!(v & 0x01, 0x01, "bit0 should reflect active status");
        assert!(!t.irq_line(), "read of $401D should clear the IRQ flag");
    }

    #[test]
    fn nsf2_irq_timer_inactive_holds_counter_at_reload() {
        let mut t = Nsf2IrqTimer::new();
        t.enabled = true;
        t.write(0x401B, 0x10);
        t.write(0x401C, 0x00);
        // Activate then deactivate to set the counter.
        t.write(0x401D, 0x01);
        t.write(0x401D, 0x00);
        t.tick(1000);
        assert_eq!(t.counter, 0x10);
        assert!(!t.irq_line());
    }

    #[test]
    fn nsf2_irq_timer_disabled_when_gate_off() {
        let mut t = Nsf2IrqTimer::new();
        // Don't set enabled.
        t.write(0x401B, 0xFF);
        t.write(0x401D, 0x01);
        t.tick(10);
        assert!(!t.irq_line());
        assert_eq!(t.read(0x401D), None);
    }

    #[test]
    fn vector_overlay_intercepts_ffff_reads_and_writes() {
        let mut bus = NesBus::new();
        // Fill PRG with 0xAA so the IRQ vector preload is observable.
        let prog = vec![0xAAu8; PRG_ROM_SIZE];
        let h = fake_header(0x8000, prog, [0u8; 8]);
        bus.configure_from_header(&h);
        bus.arm_vector_overlay(0x4FFE, 0x4FFE);
        // Reset & NMI slots preloaded from arm_vector_overlay.
        assert_eq!(bus.read(0xFFFA), 0xFE);
        assert_eq!(bus.read(0xFFFB), 0x4F);
        assert_eq!(bus.read(0xFFFC), 0xFE);
        assert_eq!(bus.read(0xFFFD), 0x4F);
        // IRQ slot preloaded from the underlying ROM (0xAA padding).
        assert_eq!(bus.read(0xFFFE), 0xAA);
        // NSF program writes its own IRQ handler.
        bus.write(0xFFFE, 0x34);
        bus.write(0xFFFF, 0x12);
        assert_eq!(bus.read(0xFFFE), 0x34);
        assert_eq!(bus.read(0xFFFF), 0x12);
    }

    #[test]
    fn vector_overlay_dropped_when_inactive() {
        let mut bus = NesBus::new();
        let prog = vec![0xCDu8; PRG_ROM_SIZE];
        let h = fake_header(0x8000, prog, [0u8; 8]);
        bus.configure_from_header(&h);
        // No arm_vector_overlay call: reads pass through to the ROM.
        assert_eq!(bus.read(0xFFFE), 0xCD);
        // Writes also dropped — ROM stays put.
        bus.write(0xFFFE, 0x77);
        assert_eq!(bus.read(0xFFFE), 0xCD);
    }

    #[test]
    fn nsf2_irq_timer_routed_through_bus_when_enabled() {
        let mut bus = NesBus::new();
        let mut h = fake_header(0x8000, vec![0x60], [0u8; 8]);
        h.nsf2 = crate::header::Nsf2Features(0x10); // IRQ support
        bus.configure_from_header(&h);
        bus.write(0x401B, 5);
        bus.write(0x401C, 0);
        bus.write(0x401D, 1);
        assert!(!bus.irq_line());
        bus.tick_cycles(6); // 5 + 1 = N+1 → first underflow
        assert!(bus.irq_line(), "bus should expose timer-driven IRQ line");
        // $401D read acknowledges.
        let _ = bus.read(0x401D);
        assert!(!bus.irq_line());
    }

    #[test]
    fn ram_mirrors_at_2k_boundary() {
        let mut bus = NesBus::new();
        bus.write(0x0010, 0x42);
        // $0810 mirrors $0010.
        assert_eq!(bus.read(0x0810), 0x42);
        assert_eq!(bus.read(0x1010), 0x42);
        assert_eq!(bus.read(0x1810), 0x42);
    }

    // -------- Round 18: MMC5 PCM IRQ + read-mode write-by-read --------
    //
    // Spec source: `docs/audio/nsf/mmc5-audio-wiki.html`
    // §"PCM Mode/IRQ ($5010)" + §"Raw PCM ($5011)" + §"PCM description"
    // + §"IRQ operation".

    fn mmc5_header_with_program(prog: Vec<u8>) -> NsfHeader {
        let mut h = fake_header(0x8000, prog, [0u8; 8]);
        // §"Expansion bits" in `docs/audio/nsf/nsfspec-kevtris-v1.61.txt`:
        // bit 3 (0x08) selects MMC5.
        h.expansion = ExpansionChips(0x08);
        h
    }

    #[test]
    fn bus_routes_mmc5_pcm_irq_into_cpu_irq_line() {
        // §"IRQ operation" final line `Cart IRQ line = (irqTrip AND
        // irqEnable)` — verifies the chain
        // Mmc5 -> Expansion::irq_line -> Apu2A03::irq_line ->
        // NesBus::irq_line.
        let mut bus = NesBus::new();
        let h = mmc5_header_with_program(vec![0x60]);
        bus.configure_from_header(&h);
        assert!(!bus.irq_line());
        // Enable PCM IRQ and trip it.
        bus.write(0x5010, 0x80); // I=1, M=0
        bus.write(0x5011, 0x00); // trip
        assert!(
            bus.irq_line(),
            "MMC5 PCM trip + enable must surface on the bus IRQ line"
        );
        // Acknowledge via $5010 read.
        let v = bus.read(0x5010);
        assert_eq!(v & 0x80, 0x80, "ack read returns the IRQ status bit");
        assert!(!bus.irq_line(), "$5010 read clears the trip");
    }

    #[test]
    fn bus_8000_bfff_read_in_read_mode_writes_dac_via_observe() {
        // §"PCM description": "MMC5's DAC is changed either by
        // writing a value to $5011 (in write mode) or reading a value
        // from $8000-BFFF (in read mode)." A flat-load NSF with a
        // $42 byte at $8000 must update the DAC when the CPU reads
        // it under MMC5 PCM read-mode.
        let mut bus = NesBus::new();
        // Two distinct non-zero bytes so we can show the DAC follows.
        let h = mmc5_header_with_program(vec![0x42, 0x88, 0x00, 0x55]);
        bus.configure_from_header(&h);
        bus.write(0x5010, 0x81); // I=1, M=1 (read mode + IRQ enable)
        assert_eq!(bus.read(0x8000), 0x42);
        // The DAC must have followed.
        assert_eq!(bus.apu.expansion.mmc5.pcm, 0x42);
        // A second read updates the DAC again.
        assert_eq!(bus.read(0x8001), 0x88);
        assert_eq!(bus.apu.expansion.mmc5.pcm, 0x88);
        // A read of a $00 byte trips the IRQ without changing the DAC.
        assert!(!bus.apu.expansion.mmc5.irq_trip);
        assert_eq!(bus.read(0x8002), 0x00);
        assert_eq!(
            bus.apu.expansion.mmc5.pcm, 0x88,
            "DAC stays put on $00 byte"
        );
        assert!(bus.apu.expansion.mmc5.irq_trip);
        assert!(bus.irq_line(), "trip propagates to the bus IRQ line");
        // A subsequent non-zero byte clears the trip.
        assert_eq!(bus.read(0x8003), 0x55);
        assert!(!bus.apu.expansion.mmc5.irq_trip);
    }

    #[test]
    fn bus_c000_to_ffff_read_does_not_touch_dac() {
        // §"PCM description" window stops at $BFFF. A read on
        // $C000..=$FFFF must not update the MMC5 DAC even in read
        // mode — the wiki window is exclusive of the upper half of
        // PRG ROM.
        let mut bus = NesBus::new();
        // Fill PRG so $C000 (offset 0x4000) holds a known byte.
        let mut prog = vec![0u8; PRG_ROM_SIZE];
        prog[0x4000] = 0xAB;
        let h = mmc5_header_with_program(prog);
        bus.configure_from_header(&h);
        bus.write(0x5010, 0x01); // read mode
        assert_eq!(bus.read(0xC000), 0xAB);
        assert_eq!(
            bus.apu.expansion.mmc5.pcm, 0x00,
            "$C000 read must NOT trigger write-by-read"
        );
    }

    #[test]
    fn bus_8000_read_in_write_mode_does_not_touch_dac() {
        // The wiki gates write-by-read on PCM read-mode being active.
        // Verify the bus respects that gate: write mode + a non-zero
        // ROM byte must leave the DAC at its prior value.
        let mut bus = NesBus::new();
        let h = mmc5_header_with_program(vec![0x77]);
        bus.configure_from_header(&h);
        bus.write(0x5010, 0x80); // I=1, M=0 (write mode)
        assert_eq!(bus.read(0x8000), 0x77);
        assert_eq!(
            bus.apu.expansion.mmc5.pcm, 0x00,
            "write-mode reads must not update the DAC"
        );
    }
}
