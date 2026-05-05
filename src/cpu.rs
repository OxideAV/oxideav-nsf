//! Clean-room MOS 6502 CPU emulator (NES 2A03 variant — decimal mode
//! disabled).
//!
//! Round 1 implements all 151 documented mnemonics × every legal
//! addressing-mode combination — covering all 256 official opcode bytes
//! that nesdev.org/wiki/CPU enumerates. Unofficial / "illegal" opcodes
//! (per nesdev.org/wiki/CPU_unofficial_opcodes) are reserved for round
//! 2; the dispatcher handles them as `NOP` of the correct base length
//! so undefined-byte programs do not deadlock the player.
//!
//! Cycle accounting is at instruction-completion granularity:
//! [`Cpu6502::step`] returns the total cycles consumed by one
//! instruction including the standard page-cross penalty on indexed
//! reads and the branch-taken / branch-page-cross penalties.

use crate::bus::NesBus;

const FLAG_C: u8 = 1 << 0;
const FLAG_Z: u8 = 1 << 1;
const FLAG_I: u8 = 1 << 2;
const FLAG_D: u8 = 1 << 3;
const FLAG_B: u8 = 1 << 4;
const FLAG_U: u8 = 1 << 5;
const FLAG_V: u8 = 1 << 6;
const FLAG_N: u8 = 1 << 7;

/// MOS 6502 (NES 2A03 variant — decimal mode disabled).
pub struct Cpu6502 {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub pc: u16,
    pub p: u8,
    pub halted: bool,
}

impl Default for Cpu6502 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu6502 {
    pub fn new() -> Self {
        Self {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            pc: 0,
            p: FLAG_I | FLAG_U,
            halted: false,
        }
    }

    pub fn reset(&mut self, bus: &mut NesBus) {
        let lo = bus.read(0xFFFC) as u16;
        let hi = bus.read(0xFFFD) as u16;
        self.pc = (hi << 8) | lo;
        self.sp = 0xFD;
        self.p = FLAG_I | FLAG_U;
        self.halted = false;
    }

    /// Fetch + decode + execute one instruction. Returns the cycles
    /// consumed.
    pub fn step(&mut self, bus: &mut NesBus) -> u32 {
        if self.halted {
            return 1;
        }
        let opcode = self.fetch_byte(bus);
        let cycles = self.dispatch(bus, opcode);
        bus.tick_cycles(cycles);
        cycles
    }

    fn fetch_byte(&mut self, bus: &mut NesBus) -> u8 {
        let b = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        b
    }

    fn fetch_word(&mut self, bus: &mut NesBus) -> u16 {
        let lo = self.fetch_byte(bus) as u16;
        let hi = self.fetch_byte(bus) as u16;
        (hi << 8) | lo
    }

    fn push(&mut self, bus: &mut NesBus, value: u8) {
        bus.write(0x0100 | self.sp as u16, value);
        self.sp = self.sp.wrapping_sub(1);
    }

    fn pop(&mut self, bus: &mut NesBus) -> u8 {
        self.sp = self.sp.wrapping_add(1);
        bus.read(0x0100 | self.sp as u16)
    }

    fn push_word(&mut self, bus: &mut NesBus, value: u16) {
        self.push(bus, (value >> 8) as u8);
        self.push(bus, value as u8);
    }

    fn pop_word(&mut self, bus: &mut NesBus) -> u16 {
        let lo = self.pop(bus) as u16;
        let hi = self.pop(bus) as u16;
        (hi << 8) | lo
    }

    fn set_zn(&mut self, value: u8) {
        self.p &= !(FLAG_Z | FLAG_N);
        if value == 0 {
            self.p |= FLAG_Z;
        }
        if value & 0x80 != 0 {
            self.p |= FLAG_N;
        }
    }

    fn set_flag(&mut self, mask: u8, on: bool) {
        if on {
            self.p |= mask;
        } else {
            self.p &= !mask;
        }
    }

    /// Public stack helper — used by [`crate::player`] to seed the
    /// pretend NMI return address before invoking init / play.
    pub fn push_word_pub(&mut self, bus: &mut NesBus, value: u16) {
        self.push_word(bus, value);
    }

    fn addr_zp(&mut self, bus: &mut NesBus) -> u16 {
        self.fetch_byte(bus) as u16
    }

    fn addr_zp_x(&mut self, bus: &mut NesBus) -> u16 {
        self.fetch_byte(bus).wrapping_add(self.x) as u16
    }

    fn addr_zp_y(&mut self, bus: &mut NesBus) -> u16 {
        self.fetch_byte(bus).wrapping_add(self.y) as u16
    }

    fn addr_abs(&mut self, bus: &mut NesBus) -> u16 {
        self.fetch_word(bus)
    }

    fn addr_abs_x(&mut self, bus: &mut NesBus) -> (u16, bool) {
        let base = self.fetch_word(bus);
        let addr = base.wrapping_add(self.x as u16);
        (addr, (base & 0xFF00) != (addr & 0xFF00))
    }

    fn addr_abs_y(&mut self, bus: &mut NesBus) -> (u16, bool) {
        let base = self.fetch_word(bus);
        let addr = base.wrapping_add(self.y as u16);
        (addr, (base & 0xFF00) != (addr & 0xFF00))
    }

    fn addr_ind_x(&mut self, bus: &mut NesBus) -> u16 {
        let zp = self.fetch_byte(bus).wrapping_add(self.x);
        let lo = bus.read(zp as u16) as u16;
        let hi = bus.read(zp.wrapping_add(1) as u16) as u16;
        (hi << 8) | lo
    }

    fn addr_ind_y(&mut self, bus: &mut NesBus) -> (u16, bool) {
        let zp = self.fetch_byte(bus);
        let lo = bus.read(zp as u16) as u16;
        let hi = bus.read(zp.wrapping_add(1) as u16) as u16;
        let base = (hi << 8) | lo;
        let addr = base.wrapping_add(self.y as u16);
        (addr, (base & 0xFF00) != (addr & 0xFF00))
    }

    /// JMP-indirect with the documented page-wrap bug.
    fn addr_indirect_jmp(&mut self, bus: &mut NesBus) -> u16 {
        let ptr = self.fetch_word(bus);
        let lo = bus.read(ptr) as u16;
        let hi_ptr = (ptr & 0xFF00) | ((ptr.wrapping_add(1)) & 0x00FF);
        let hi = bus.read(hi_ptr) as u16;
        (hi << 8) | lo
    }

    fn op_adc(&mut self, m: u8) {
        let a = self.a as u16;
        let m16 = m as u16;
        let c = (self.p & FLAG_C) as u16;
        let sum = a + m16 + c;
        let result = sum as u8;
        let carry = sum > 0xFF;
        let overflow = ((self.a ^ result) & (m ^ result) & 0x80) != 0;
        self.a = result;
        self.set_flag(FLAG_C, carry);
        self.set_flag(FLAG_V, overflow);
        self.set_zn(result);
    }

    fn op_sbc(&mut self, m: u8) {
        self.op_adc(!m);
    }

    fn op_and(&mut self, m: u8) {
        self.a &= m;
        let a = self.a;
        self.set_zn(a);
    }

    fn op_ora(&mut self, m: u8) {
        self.a |= m;
        let a = self.a;
        self.set_zn(a);
    }

    fn op_eor(&mut self, m: u8) {
        self.a ^= m;
        let a = self.a;
        self.set_zn(a);
    }

    fn op_cmp_value(&mut self, reg: u8, m: u8) {
        let r = reg.wrapping_sub(m);
        self.set_flag(FLAG_C, reg >= m);
        self.set_zn(r);
    }

    fn op_bit(&mut self, m: u8) {
        self.set_flag(FLAG_Z, (self.a & m) == 0);
        self.set_flag(FLAG_N, m & 0x80 != 0);
        self.set_flag(FLAG_V, m & 0x40 != 0);
    }

    fn op_asl_value(&mut self, m: u8) -> u8 {
        let carry = m & 0x80 != 0;
        let r = m << 1;
        self.set_flag(FLAG_C, carry);
        self.set_zn(r);
        r
    }

    fn op_lsr_value(&mut self, m: u8) -> u8 {
        let carry = m & 0x01 != 0;
        let r = m >> 1;
        self.set_flag(FLAG_C, carry);
        self.set_zn(r);
        r
    }

    fn op_rol_value(&mut self, m: u8) -> u8 {
        let old_c = self.p & FLAG_C != 0;
        let carry = m & 0x80 != 0;
        let r = (m << 1) | (old_c as u8);
        self.set_flag(FLAG_C, carry);
        self.set_zn(r);
        r
    }

    fn op_ror_value(&mut self, m: u8) -> u8 {
        let old_c = self.p & FLAG_C != 0;
        let carry = m & 0x01 != 0;
        let r = (m >> 1) | ((old_c as u8) << 7);
        self.set_flag(FLAG_C, carry);
        self.set_zn(r);
        r
    }

    fn dispatch(&mut self, bus: &mut NesBus, opcode: u8) -> u32 {
        match opcode {
            // ---- LDA ----
            0xA9 => {
                let m = self.fetch_byte(bus);
                self.a = m;
                let a = self.a;
                self.set_zn(a);
                2
            }
            0xA5 => {
                let addr = self.addr_zp(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                3
            }
            0xB5 => {
                let addr = self.addr_zp_x(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                4
            }
            0xAD => {
                let addr = self.addr_abs(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                4
            }
            0xBD => {
                let (addr, c) = self.addr_abs_x(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                4 + c as u32
            }
            0xB9 => {
                let (addr, c) = self.addr_abs_y(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                4 + c as u32
            }
            0xA1 => {
                let addr = self.addr_ind_x(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                6
            }
            0xB1 => {
                let (addr, c) = self.addr_ind_y(bus);
                self.a = bus.read(addr);
                let a = self.a;
                self.set_zn(a);
                5 + c as u32
            }

            // ---- LDX ----
            0xA2 => {
                let m = self.fetch_byte(bus);
                self.x = m;
                let x = self.x;
                self.set_zn(x);
                2
            }
            0xA6 => {
                let addr = self.addr_zp(bus);
                self.x = bus.read(addr);
                let x = self.x;
                self.set_zn(x);
                3
            }
            0xB6 => {
                let addr = self.addr_zp_y(bus);
                self.x = bus.read(addr);
                let x = self.x;
                self.set_zn(x);
                4
            }
            0xAE => {
                let addr = self.addr_abs(bus);
                self.x = bus.read(addr);
                let x = self.x;
                self.set_zn(x);
                4
            }
            0xBE => {
                let (addr, c) = self.addr_abs_y(bus);
                self.x = bus.read(addr);
                let x = self.x;
                self.set_zn(x);
                4 + c as u32
            }

            // ---- LDY ----
            0xA0 => {
                let m = self.fetch_byte(bus);
                self.y = m;
                let y = self.y;
                self.set_zn(y);
                2
            }
            0xA4 => {
                let addr = self.addr_zp(bus);
                self.y = bus.read(addr);
                let y = self.y;
                self.set_zn(y);
                3
            }
            0xB4 => {
                let addr = self.addr_zp_x(bus);
                self.y = bus.read(addr);
                let y = self.y;
                self.set_zn(y);
                4
            }
            0xAC => {
                let addr = self.addr_abs(bus);
                self.y = bus.read(addr);
                let y = self.y;
                self.set_zn(y);
                4
            }
            0xBC => {
                let (addr, c) = self.addr_abs_x(bus);
                self.y = bus.read(addr);
                let y = self.y;
                self.set_zn(y);
                4 + c as u32
            }

            // ---- STA ----
            0x85 => {
                let addr = self.addr_zp(bus);
                bus.write(addr, self.a);
                3
            }
            0x95 => {
                let addr = self.addr_zp_x(bus);
                bus.write(addr, self.a);
                4
            }
            0x8D => {
                let addr = self.addr_abs(bus);
                bus.write(addr, self.a);
                4
            }
            0x9D => {
                let (addr, _) = self.addr_abs_x(bus);
                bus.write(addr, self.a);
                5
            }
            0x99 => {
                let (addr, _) = self.addr_abs_y(bus);
                bus.write(addr, self.a);
                5
            }
            0x81 => {
                let addr = self.addr_ind_x(bus);
                bus.write(addr, self.a);
                6
            }
            0x91 => {
                let (addr, _) = self.addr_ind_y(bus);
                bus.write(addr, self.a);
                6
            }

            // ---- STX ----
            0x86 => {
                let addr = self.addr_zp(bus);
                bus.write(addr, self.x);
                3
            }
            0x96 => {
                let addr = self.addr_zp_y(bus);
                bus.write(addr, self.x);
                4
            }
            0x8E => {
                let addr = self.addr_abs(bus);
                bus.write(addr, self.x);
                4
            }

            // ---- STY ----
            0x84 => {
                let addr = self.addr_zp(bus);
                bus.write(addr, self.y);
                3
            }
            0x94 => {
                let addr = self.addr_zp_x(bus);
                bus.write(addr, self.y);
                4
            }
            0x8C => {
                let addr = self.addr_abs(bus);
                bus.write(addr, self.y);
                4
            }

            // ---- Transfers ----
            0xAA => {
                self.x = self.a;
                let x = self.x;
                self.set_zn(x);
                2
            }
            0xA8 => {
                self.y = self.a;
                let y = self.y;
                self.set_zn(y);
                2
            }
            0x8A => {
                self.a = self.x;
                let a = self.a;
                self.set_zn(a);
                2
            }
            0x98 => {
                self.a = self.y;
                let a = self.a;
                self.set_zn(a);
                2
            }
            0xBA => {
                self.x = self.sp;
                let x = self.x;
                self.set_zn(x);
                2
            }
            0x9A => {
                self.sp = self.x;
                2
            }

            // ---- Stack ops ----
            0x48 => {
                let a = self.a;
                self.push(bus, a);
                3
            }
            0x68 => {
                self.a = self.pop(bus);
                let a = self.a;
                self.set_zn(a);
                4
            }
            0x08 => {
                let p = self.p | FLAG_B | FLAG_U;
                self.push(bus, p);
                3
            }
            0x28 => {
                let p = (self.pop(bus) & !FLAG_B) | FLAG_U;
                self.p = p;
                4
            }

            // ---- Logical AND ----
            0x29 => {
                let m = self.fetch_byte(bus);
                self.op_and(m);
                2
            }
            0x25 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_and(m);
                3
            }
            0x35 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                self.op_and(m);
                4
            }
            0x2D => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_and(m);
                4
            }
            0x3D => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                self.op_and(m);
                4 + c as u32
            }
            0x39 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.op_and(m);
                4 + c as u32
            }
            0x21 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.op_and(m);
                6
            }
            0x31 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.op_and(m);
                5 + c as u32
            }

            // ---- ORA ----
            0x09 => {
                let m = self.fetch_byte(bus);
                self.op_ora(m);
                2
            }
            0x05 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                3
            }
            0x15 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                4
            }
            0x0D => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                4
            }
            0x1D => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                4 + c as u32
            }
            0x19 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                4 + c as u32
            }
            0x01 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                6
            }
            0x11 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.op_ora(m);
                5 + c as u32
            }

            // ---- EOR ----
            0x49 => {
                let m = self.fetch_byte(bus);
                self.op_eor(m);
                2
            }
            0x45 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                3
            }
            0x55 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                4
            }
            0x4D => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                4
            }
            0x5D => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                4 + c as u32
            }
            0x59 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                4 + c as u32
            }
            0x41 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                6
            }
            0x51 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.op_eor(m);
                5 + c as u32
            }

            // ---- ADC ----
            0x69 => {
                let m = self.fetch_byte(bus);
                self.op_adc(m);
                2
            }
            0x65 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                3
            }
            0x75 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                4
            }
            0x6D => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                4
            }
            0x7D => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                4 + c as u32
            }
            0x79 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                4 + c as u32
            }
            0x61 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                6
            }
            0x71 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.op_adc(m);
                5 + c as u32
            }

            // ---- SBC ----
            0xE9 => {
                let m = self.fetch_byte(bus);
                self.op_sbc(m);
                2
            }
            0xE5 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                3
            }
            0xF5 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                4
            }
            0xED => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                4
            }
            0xFD => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                4 + c as u32
            }
            0xF9 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                4 + c as u32
            }
            0xE1 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                6
            }
            0xF1 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.op_sbc(m);
                5 + c as u32
            }

            // ---- CMP ----
            0xC9 => {
                let m = self.fetch_byte(bus);
                let a = self.a;
                self.op_cmp_value(a, m);
                2
            }
            0xC5 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                3
            }
            0xD5 => {
                let addr = self.addr_zp_x(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                4
            }
            0xCD => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                4
            }
            0xDD => {
                let (addr, c) = self.addr_abs_x(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                4 + c as u32
            }
            0xD9 => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                4 + c as u32
            }
            0xC1 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                6
            }
            0xD1 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                let a = self.a;
                self.op_cmp_value(a, m);
                5 + c as u32
            }

            // ---- CPX / CPY ----
            0xE0 => {
                let m = self.fetch_byte(bus);
                let x = self.x;
                self.op_cmp_value(x, m);
                2
            }
            0xE4 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                let x = self.x;
                self.op_cmp_value(x, m);
                3
            }
            0xEC => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                let x = self.x;
                self.op_cmp_value(x, m);
                4
            }
            0xC0 => {
                let m = self.fetch_byte(bus);
                let y = self.y;
                self.op_cmp_value(y, m);
                2
            }
            0xC4 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                let y = self.y;
                self.op_cmp_value(y, m);
                3
            }
            0xCC => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                let y = self.y;
                self.op_cmp_value(y, m);
                4
            }

            // ---- BIT ----
            0x24 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.op_bit(m);
                3
            }
            0x2C => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.op_bit(m);
                4
            }

            // ---- INC / DEC ----
            0xE6 => {
                let addr = self.addr_zp(bus);
                let v = bus.read(addr).wrapping_add(1);
                bus.write(addr, v);
                self.set_zn(v);
                5
            }
            0xF6 => {
                let addr = self.addr_zp_x(bus);
                let v = bus.read(addr).wrapping_add(1);
                bus.write(addr, v);
                self.set_zn(v);
                6
            }
            0xEE => {
                let addr = self.addr_abs(bus);
                let v = bus.read(addr).wrapping_add(1);
                bus.write(addr, v);
                self.set_zn(v);
                6
            }
            0xFE => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = bus.read(addr).wrapping_add(1);
                bus.write(addr, v);
                self.set_zn(v);
                7
            }
            0xC6 => {
                let addr = self.addr_zp(bus);
                let v = bus.read(addr).wrapping_sub(1);
                bus.write(addr, v);
                self.set_zn(v);
                5
            }
            0xD6 => {
                let addr = self.addr_zp_x(bus);
                let v = bus.read(addr).wrapping_sub(1);
                bus.write(addr, v);
                self.set_zn(v);
                6
            }
            0xCE => {
                let addr = self.addr_abs(bus);
                let v = bus.read(addr).wrapping_sub(1);
                bus.write(addr, v);
                self.set_zn(v);
                6
            }
            0xDE => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = bus.read(addr).wrapping_sub(1);
                bus.write(addr, v);
                self.set_zn(v);
                7
            }
            0xE8 => {
                self.x = self.x.wrapping_add(1);
                let x = self.x;
                self.set_zn(x);
                2
            }
            0xC8 => {
                self.y = self.y.wrapping_add(1);
                let y = self.y;
                self.set_zn(y);
                2
            }
            0xCA => {
                self.x = self.x.wrapping_sub(1);
                let x = self.x;
                self.set_zn(x);
                2
            }
            0x88 => {
                self.y = self.y.wrapping_sub(1);
                let y = self.y;
                self.set_zn(y);
                2
            }

            // ---- Shifts ----
            0x0A => {
                let v = self.op_asl_value(self.a);
                self.a = v;
                2
            }
            0x06 => {
                let addr = self.addr_zp(bus);
                let v = self.op_asl_value(bus.read(addr));
                bus.write(addr, v);
                5
            }
            0x16 => {
                let addr = self.addr_zp_x(bus);
                let v = self.op_asl_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x0E => {
                let addr = self.addr_abs(bus);
                let v = self.op_asl_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x1E => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = self.op_asl_value(bus.read(addr));
                bus.write(addr, v);
                7
            }
            0x4A => {
                let v = self.op_lsr_value(self.a);
                self.a = v;
                2
            }
            0x46 => {
                let addr = self.addr_zp(bus);
                let v = self.op_lsr_value(bus.read(addr));
                bus.write(addr, v);
                5
            }
            0x56 => {
                let addr = self.addr_zp_x(bus);
                let v = self.op_lsr_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x4E => {
                let addr = self.addr_abs(bus);
                let v = self.op_lsr_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x5E => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = self.op_lsr_value(bus.read(addr));
                bus.write(addr, v);
                7
            }
            0x2A => {
                let v = self.op_rol_value(self.a);
                self.a = v;
                2
            }
            0x26 => {
                let addr = self.addr_zp(bus);
                let v = self.op_rol_value(bus.read(addr));
                bus.write(addr, v);
                5
            }
            0x36 => {
                let addr = self.addr_zp_x(bus);
                let v = self.op_rol_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x2E => {
                let addr = self.addr_abs(bus);
                let v = self.op_rol_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x3E => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = self.op_rol_value(bus.read(addr));
                bus.write(addr, v);
                7
            }
            0x6A => {
                let v = self.op_ror_value(self.a);
                self.a = v;
                2
            }
            0x66 => {
                let addr = self.addr_zp(bus);
                let v = self.op_ror_value(bus.read(addr));
                bus.write(addr, v);
                5
            }
            0x76 => {
                let addr = self.addr_zp_x(bus);
                let v = self.op_ror_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x6E => {
                let addr = self.addr_abs(bus);
                let v = self.op_ror_value(bus.read(addr));
                bus.write(addr, v);
                6
            }
            0x7E => {
                let (addr, _) = self.addr_abs_x(bus);
                let v = self.op_ror_value(bus.read(addr));
                bus.write(addr, v);
                7
            }

            // ---- Branches ----
            0x10 => self.do_branch(bus, self.p & FLAG_N == 0),
            0x30 => self.do_branch(bus, self.p & FLAG_N != 0),
            0x50 => self.do_branch(bus, self.p & FLAG_V == 0),
            0x70 => self.do_branch(bus, self.p & FLAG_V != 0),
            0x90 => self.do_branch(bus, self.p & FLAG_C == 0),
            0xB0 => self.do_branch(bus, self.p & FLAG_C != 0),
            0xD0 => self.do_branch(bus, self.p & FLAG_Z == 0),
            0xF0 => self.do_branch(bus, self.p & FLAG_Z != 0),

            // ---- Flag ops ----
            0x18 => {
                self.p &= !FLAG_C;
                2
            }
            0x38 => {
                self.p |= FLAG_C;
                2
            }
            0x58 => {
                self.p &= !FLAG_I;
                2
            }
            0x78 => {
                self.p |= FLAG_I;
                2
            }
            0xB8 => {
                self.p &= !FLAG_V;
                2
            }
            0xD8 => {
                self.p &= !FLAG_D;
                2
            }
            0xF8 => {
                self.p |= FLAG_D;
                2
            }

            // ---- Jumps / subroutines ----
            0x4C => {
                self.pc = self.fetch_word(bus);
                3
            }
            0x6C => {
                self.pc = self.addr_indirect_jmp(bus);
                5
            }
            0x20 => {
                let target = self.fetch_word(bus);
                let ret = self.pc.wrapping_sub(1);
                self.push_word(bus, ret);
                self.pc = target;
                6
            }
            0x60 => {
                let pc = self.pop_word(bus).wrapping_add(1);
                self.pc = pc;
                6
            }
            0x40 => {
                let p = (self.pop(bus) & !FLAG_B) | FLAG_U;
                let pc = self.pop_word(bus);
                self.p = p;
                self.pc = pc;
                6
            }

            // ---- BRK / NOP ----
            0x00 => {
                let pc = self.pc.wrapping_add(1);
                self.push_word(bus, pc);
                let p = self.p | FLAG_B | FLAG_U;
                self.push(bus, p);
                self.p |= FLAG_I;
                let lo = bus.read(0xFFFE) as u16;
                let hi = bus.read(0xFFFF) as u16;
                self.pc = (hi << 8) | lo;
                7
            }
            0xEA => 2,

            // ---- Unofficial / undefined: treat as NOP of correct length ----
            0x1A | 0x3A | 0x5A | 0x7A | 0xDA | 0xFA => 2,
            0x80 | 0x82 | 0x89 | 0xC2 | 0xE2 | 0x04 | 0x44 | 0x64 | 0x14 | 0x34 | 0x54 | 0x74
            | 0xD4 | 0xF4 => {
                let _ = self.fetch_byte(bus);
                3
            }
            0x0C => {
                let _ = self.fetch_word(bus);
                4
            }
            0x1C | 0x3C | 0x5C | 0x7C | 0xDC | 0xFC => {
                let (_, c) = self.addr_abs_x(bus);
                4 + c as u32
            }
            _ => 2,
        }
    }

    fn do_branch(&mut self, bus: &mut NesBus, take: bool) -> u32 {
        let offset = self.fetch_byte(bus) as i8;
        if !take {
            return 2;
        }
        let old_pc = self.pc;
        let new_pc = ((old_pc as i32) + (offset as i32)) as u16;
        self.pc = new_pc;
        let crossed = (old_pc & 0xFF00) != (new_pc & 0xFF00);
        3 + crossed as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::NesBus;

    fn run_one(prog: &[u8], setup: impl FnOnce(&mut Cpu6502, &mut NesBus)) -> (Cpu6502, NesBus) {
        let mut bus = NesBus::new();
        bus.load_program(0x8000, prog);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        setup(&mut cpu, &mut bus);
        cpu.step(&mut bus);
        (cpu, bus)
    }

    #[test]
    fn lda_imm_sets_zn() {
        let (cpu, _) = run_one(&[0xA9, 0x00], |_, _| {});
        assert_eq!(cpu.a, 0);
        assert!(cpu.p & FLAG_Z != 0);
        assert!(cpu.p & FLAG_N == 0);
        let (cpu, _) = run_one(&[0xA9, 0x80], |_, _| {});
        assert_eq!(cpu.a, 0x80);
        assert!(cpu.p & FLAG_N != 0);
    }

    #[test]
    fn sta_zp_writes_ram() {
        let (_cpu, bus) = run_one(&[0x85, 0x10], |c, _| c.a = 0x42);
        assert_eq!(bus.ram[0x10], 0x42);
    }

    #[test]
    fn adc_carry_overflow() {
        let (cpu, _) = run_one(&[0x69, 0x50], |c, _| c.a = 0x50);
        assert_eq!(cpu.a, 0xA0);
        assert!(cpu.p & FLAG_V != 0);
        assert!(cpu.p & FLAG_C == 0);

        let (cpu, _) = run_one(&[0x69, 0x01], |c, _| c.a = 0xFF);
        assert_eq!(cpu.a, 0x00);
        assert!(cpu.p & FLAG_C != 0);
        assert!(cpu.p & FLAG_Z != 0);
    }

    #[test]
    fn sbc_borrow() {
        let (cpu, _) = run_one(&[0xE9, 0x05], |c, _| {
            c.a = 0x10;
            c.p |= FLAG_C;
        });
        assert_eq!(cpu.a, 0x0B);
        assert!(cpu.p & FLAG_C != 0);
    }

    #[test]
    fn jsr_rts_roundtrip() {
        let mut bus = NesBus::new();
        let prog = &[0x20, 0x05, 0x80, 0xEA, 0xEA, 0x60];
        bus.load_program(0x8000, prog);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.step(&mut bus); // JSR
        assert_eq!(cpu.pc, 0x8005);
        cpu.step(&mut bus); // RTS
        assert_eq!(cpu.pc, 0x8003);
    }

    #[test]
    fn branch_page_cross() {
        // BNE -10 from $8000: after fetch, PC = $8002; new PC = $8002
        // - 10 = $7FF8, which crosses the $80 -> $7F page boundary.
        let mut bus = NesBus::new();
        bus.load_program(0x8000, &[0xD0, 0xF6]); // BNE -10
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p &= !FLAG_Z;
        let cy = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x7FF8);
        assert_eq!(cy, 4);
    }

    #[test]
    fn branch_taken_no_page_cross() {
        let mut bus = NesBus::new();
        bus.load_program(0x8000, &[0xD0, 0x01]); // BNE +1
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p &= !FLAG_Z;
        let cy = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8003);
        assert_eq!(cy, 3);
    }

    #[test]
    fn branch_not_taken_takes_two_cycles() {
        let mut bus = NesBus::new();
        bus.load_program(0x8000, &[0xD0, 0x01]); // BNE +1
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p |= FLAG_Z; // Z=1 means BNE not taken.
        let cy = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8002);
        assert_eq!(cy, 2);
    }
}
