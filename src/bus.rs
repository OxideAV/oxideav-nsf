//! 64 KiB NES address-space bus.
//!
//! NSF only models the subset of the NES that matters for music
//! playback:
//!
//! | Range            | Mapping                                            |
//! |------------------|----------------------------------------------------|
//! | `$0000..=$07FF`  | 2 KiB CPU RAM                                       |
//! | `$0800..=$1FFF`  | three mirrors of the 2 KiB CPU RAM                  |
//! | `$2000..=$3FFF`  | PPU registers (open bus — NSF does not draw video)  |
//! | `$4000..=$4013`  | APU register file (pulse / triangle / noise / DMC)  |
//! | `$4014`          | OAM DMA — open bus for NSF                          |
//! | `$4015`          | APU status                                          |
//! | `$4016`          | controller 1 strobe (open bus for NSF)              |
//! | `$4017`          | APU frame counter / controller 2 strobe             |
//! | `$4018..=$5FFF`  | open bus / expansion-chip register space            |
//! | `$6000..=$7FFF`  | 8 KiB optional cartridge RAM (some NSFs use it)     |
//! | `$8000..=$FFFF`  | NSF program ROM (loaded from `load_addr` upward)    |
//!
//! Bankswitching (NSF 2.x and any NSF that fills `bankswitch_init`)
//! and expansion-chip registers are **not** wired up in round 1 — the
//! bus reports open-bus reads (returns `0xFF`) and silently drops
//! writes outside the APU + RAM windows.

use crate::apu::Apu2A03;

/// 2 KiB of work RAM.
pub const RAM_SIZE: usize = 0x0800;

/// 8 KiB of optional cartridge RAM at `$6000..=$7FFF`.
pub const CART_RAM_SIZE: usize = 0x2000;

/// 32 KiB of program ROM at `$8000..=$FFFF`.
pub const PRG_ROM_SIZE: usize = 0x8000;

/// 64 KiB CPU view of the NES bus.
pub struct NesBus {
    pub ram: [u8; RAM_SIZE],
    pub cart_ram: [u8; CART_RAM_SIZE],
    pub prg: [u8; PRG_ROM_SIZE],
    pub apu: Apu2A03,
    pub cycles: u64,
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
        }
    }

    /// Load the NSF program blob at `load_addr`. Bytes that fall past
    /// `$FFFF` are silently dropped.
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

    pub fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize],
            0x2000..=0x3FFF => 0xFF,
            0x4000..=0x4014 => 0xFF,
            0x4015 => self.apu.read_status(),
            0x4016 | 0x4017 => 0xFF,
            0x4018..=0x5FFF => 0xFF,
            0x6000..=0x7FFF => self.cart_ram[(addr - 0x6000) as usize],
            0x8000..=0xFFFF => self.prg[(addr - 0x8000) as usize],
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
            0x4018..=0x5FFF => {}
            0x6000..=0x7FFF => self.cart_ram[(addr - 0x6000) as usize] = value,
            0x8000..=0xFFFF => self.prg[(addr - 0x8000) as usize] = value,
        }
    }

    /// Inform the bus that `cycles` CPU clocks elapsed; forwards them
    /// to the APU so the frame counter advances.
    pub fn tick_cycles(&mut self, cycles: u32) {
        self.cycles = self.cycles.wrapping_add(cycles as u64);
        self.apu.tick_cpu_cycles(cycles);
    }
}
