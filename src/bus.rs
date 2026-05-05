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
        }
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
            // 5B / N163 / MMC5 status reads land here.
            0x4018..=0x5FFF => self.apu.read_expansion(addr),
            0x6000..=0x7FFF => self.cart_ram[(addr - 0x6000) as usize],
            0x8000..=0xFFFF => {
                if self.bankswitched {
                    self.bank_read(addr)
                } else {
                    self.prg[(addr - 0x8000) as usize]
                }
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
            // Drain pending DMC fetches.
            while let Some(addr) = self.apu.dmc_pending_fetch() {
                let byte = self.read(addr);
                self.apu.dmc_supply_byte(byte);
            }
            remaining -= n;
        }
    }
}
