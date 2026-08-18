//! Clean-room MOS 6502 CPU emulator (NES 2A03 variant — decimal mode
//! disabled).
//!
//! Round 1 implemented all 151 documented mnemonics × every legal
//! addressing-mode combination — covering all 256 official opcode bytes
//! that nesdev.org/wiki/CPU enumerates.
//!
//! Round 2 fills in the unofficial / "illegal" opcodes per
//! nesdev.org/wiki/CPU_unofficial_opcodes. Concretely:
//!
//! * **Stable**: LAX, SAX, DCP, ISB/ISC, SLO, RLA, SRE, RRA, ANC, ALR,
//!   ARR, SBX (AXS), the duplicate SBC (`$EB`), and the multi-byte NOP
//!   variants. KIL / JAM (`$02`, `$12`, `$22`, `$32`, `$42`, `$52`,
//!   `$62`, `$72`, `$92`, `$B2`, `$D2`, `$F2`) latch the CPU `halted`
//!   bit so the player loop short-circuits the rest of the period.
//! * **Unstable / "magic-constant"**: SHA, SHX, SHY, TAS, LAS, ANE/XAA,
//!   LXA. We pick the deterministic interpretation documented on the
//!   wiki ("magic = 0xFF" branch) — sufficient for the music engines we
//!   target. Anyone needing per-die-bug accuracy is in PPU territory.
//!
//! Cycle accounting is at instruction-completion granularity:
//! [`Cpu6502::step`] returns the total cycles consumed by one
//! instruction including the standard page-cross penalty on indexed
//! reads and the branch-taken / branch-page-cross penalties. Read-modify-
//! write unofficial ops (DCP / ISB / SLO / RLA / SRE / RRA) follow the
//! 6502's published RMW timing — abs,X / abs,Y / (zp),Y always pay the
//! 7-cycle penalty even when no page crossing occurred.
//!
//! Instruction *state* executes atomically, but every step hands the
//! bus its per-cycle read/write pattern ([`write_cycle_mask`]) so the
//! sub-instruction DMA engine can place DMC/OAM DMA halts on their
//! exact CPU cycles — including the write-cycle halt delays of
//! `docs/audio/nsf/apu-dma-wiki.html` §Behavior.

use crate::bus::NesBus;

/// Write-cycle bitmask for an IRQ/NMI dispatch: 7 cycles with the
/// three stack pushes (PCH, PCL, P) on cycles 2-4 — the "interrupts
/// having 3 [consecutive writes]" case of
/// `docs/audio/nsf/apu-dma-wiki.html` §Behavior's DMA-halt delays.
pub const INTERRUPT_WRITE_MASK: u32 = 0b001_1100;

/// Bitmask of which cycles of the instruction `opcode` (taking
/// `cycles` total) are CPU **write** cycles — bit `i` set means cycle
/// offset `i` writes. Everything else is a read; the 6502 performs a
/// bus access on every cycle.
///
/// This is the per-cycle bus behaviour the DMA engine needs:
/// `docs/audio/nsf/apu-dma-wiki.html` §Behavior — "DMA can only halt
/// on CPU read cycles. On write cycles, the halt fails and the DMA
/// unit tries again next CPU cycle […] Delays of up to 3 cycles are
/// possible, with read-modify-write instructions having 2 consecutive
/// writes and interrupts having 3." Stores (official and unofficial)
/// write on their final cycle; read-modify-write instructions spend
/// their final two cycles writing (old value, then new); `PHA`/`PHP`
/// push on their final cycle; `JSR` pushes the return address on
/// cycles 3-4; `BRK` pushes PCH/PCL/P on cycles 2-4 (the interrupt
/// shape). All other opcodes only read.
pub fn write_cycle_mask(opcode: u8, cycles: u32) -> u32 {
    match opcode {
        // BRK: fetch, pad fetch, push PCH/PCL/P, vector lo, vector hi.
        0x00 => 0b001_1100,
        // JSR: fetch, operand lo, internal, push PCH, push PCL,
        // operand hi.
        0x20 => 0b001_1000,
        // Single-write stores + stack pushes: the write is the final
        // cycle. STA/STX/STY, unofficial SAX/SHA/SHX/SHY/TAS, PHA/PHP.
        0x85 | 0x95 | 0x8D | 0x9D | 0x99 | 0x81 | 0x91 // STA
        | 0x86 | 0x96 | 0x8E // STX
        | 0x84 | 0x94 | 0x8C // STY
        | 0x87 | 0x97 | 0x8F | 0x83 // SAX
        | 0x9F | 0x93 | 0x9E | 0x9C | 0x9B // SHA/SHX/SHY/TAS
        | 0x48 | 0x08 // PHA/PHP
            => 1u32 << (cycles - 1),
        // Read-modify-write: the final two cycles write (the 6502
        // writes the unmodified value back, then the new one).
        // ASL/LSR/ROL/ROR/INC/DEC + unofficial SLO/RLA/SRE/RRA/DCP/ISB.
        0x06 | 0x16 | 0x0E | 0x1E // ASL
        | 0x46 | 0x56 | 0x4E | 0x5E // LSR
        | 0x26 | 0x36 | 0x2E | 0x3E // ROL
        | 0x66 | 0x76 | 0x6E | 0x7E // ROR
        | 0xE6 | 0xF6 | 0xEE | 0xFE // INC
        | 0xC6 | 0xD6 | 0xCE | 0xDE // DEC
        | 0x07 | 0x17 | 0x0F | 0x1F | 0x1B | 0x03 | 0x13 // SLO
        | 0x27 | 0x37 | 0x2F | 0x3F | 0x3B | 0x23 | 0x33 // RLA
        | 0x47 | 0x57 | 0x4F | 0x5F | 0x5B | 0x43 | 0x53 // SRE
        | 0x67 | 0x77 | 0x6F | 0x7F | 0x7B | 0x63 | 0x73 // RRA
        | 0xC7 | 0xD7 | 0xCF | 0xDF | 0xDB | 0xC3 | 0xD3 // DCP
        | 0xE7 | 0xF7 | 0xEF | 0xFF | 0xFB | 0xE3 | 0xF3 // ISB
            => 0b11u32 << (cycles - 2),
        _ => 0,
    }
}

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
    /// consumed — including any CPU cycles stolen by DMC sample-byte
    /// DMA during the instruction (the §"Memory reader" stall from
    /// `docs/audio/nsf/apu-dmc-wiki.html`; "The processor will
    /// continue on from where it was stalled"), so callers scheduling
    /// off the return value see the DMA-stretched wall-clock time.
    pub fn step(&mut self, bus: &mut NesBus) -> u32 {
        if self.halted {
            return 1;
        }
        // NMI is edge-triggered and ignores the I flag — service it
        // before any pending IRQ check (the NSF2 non-returning-INIT
        // path uses NMI to interrupt INIT and run PLAY).
        if bus.take_nmi() {
            bus.begin_instruction();
            let cy = self.service_interrupt(bus, 0xFFFA);
            bus.run_instruction(cy, INTERRUPT_WRITE_MASK);
            return cy + bus.take_dma_stall();
        }
        // IRQ is level-triggered: serviced whenever the I flag is
        // clear and the bus is asserting the IRQ line (NSF2 timer
        // device — see `docs/audio/nsf/nsf2-nesdev-wiki.html` §IRQ
        // Support).
        if (self.p & FLAG_I) == 0 && bus.irq_line() {
            bus.begin_instruction();
            let cy = self.service_interrupt(bus, 0xFFFE);
            bus.run_instruction(cy, INTERRUPT_WRITE_MASK);
            return cy + bus.take_dma_stall();
        }
        bus.begin_instruction();
        let opcode = self.fetch_byte(bus);
        let cycles = self.dispatch(bus, opcode);
        // Hand the bus the instruction's per-cycle read/write pattern
        // so DMA halts land on their true cycles (apu-dma-wiki
        // §Behavior write-cycle halt delays).
        bus.run_instruction(cycles, write_cycle_mask(opcode, cycles));
        cycles + bus.take_dma_stall()
    }

    /// Push PC + processor flags (B=0, U=1), set I, then jump through
    /// the 16-bit vector at `vector` / `vector+1`. Used for both
    /// hardware IRQ (`$FFFE`) and NMI (`$FFFA`) entry — they differ
    /// only in which vector is read; both push P with B clear.
    fn service_interrupt(&mut self, bus: &mut NesBus, vector: u16) -> u32 {
        let pc = self.pc;
        self.push_word(bus, pc);
        let p = (self.p & !FLAG_B) | FLAG_U;
        self.push(bus, p);
        self.p |= FLAG_I;
        let lo = bus.read(vector) as u16;
        let hi = bus.read(vector.wrapping_add(1)) as u16;
        self.pc = (hi << 8) | lo;
        // Hardware takes 7 cycles to dispatch IRQ/NMI on the 6502.
        7
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

            // ---- Unofficial NOPs (single-byte, no operand fetch) ----
            0x1A | 0x3A | 0x5A | 0x7A | 0xDA | 0xFA => 2,
            // ---- Unofficial NOPs (immediate / zero-page) ----
            0x80 | 0x82 | 0x89 | 0xC2 | 0xE2 => {
                let _ = self.fetch_byte(bus);
                2
            }
            0x04 | 0x44 | 0x64 => {
                let _ = self.fetch_byte(bus);
                3
            }
            0x14 | 0x34 | 0x54 | 0x74 | 0xD4 | 0xF4 => {
                let _ = self.fetch_byte(bus);
                4
            }
            // ---- Unofficial NOPs (absolute / absolute,X) ----
            0x0C => {
                let _ = self.fetch_word(bus);
                4
            }
            0x1C | 0x3C | 0x5C | 0x7C | 0xDC | 0xFC => {
                let (_, c) = self.addr_abs_x(bus);
                4 + c as u32
            }

            // ---- Duplicate SBC ----
            0xEB => {
                let m = self.fetch_byte(bus);
                self.op_sbc(m);
                2
            }

            // ---- LAX (LDA + LDX combined) ----
            0xA7 => {
                let addr = self.addr_zp(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                3
            }
            0xB7 => {
                let addr = self.addr_zp_y(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                4
            }
            0xAF => {
                let addr = self.addr_abs(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                4
            }
            0xBF => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                4 + c as u32
            }
            0xA3 => {
                let addr = self.addr_ind_x(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                6
            }
            0xB3 => {
                let (addr, c) = self.addr_ind_y(bus);
                let m = bus.read(addr);
                self.a = m;
                self.x = m;
                self.set_zn(m);
                5 + c as u32
            }

            // ---- SAX (store A AND X — no flags affected) ----
            0x87 => {
                let addr = self.addr_zp(bus);
                bus.write(addr, self.a & self.x);
                3
            }
            0x97 => {
                let addr = self.addr_zp_y(bus);
                bus.write(addr, self.a & self.x);
                4
            }
            0x8F => {
                let addr = self.addr_abs(bus);
                bus.write(addr, self.a & self.x);
                4
            }
            0x83 => {
                let addr = self.addr_ind_x(bus);
                bus.write(addr, self.a & self.x);
                6
            }

            // ---- DCP (DEC then CMP A,result) ----
            0xC7 => {
                let addr = self.addr_zp(bus);
                self.rmw_dcp(bus, addr);
                5
            }
            0xD7 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_dcp(bus, addr);
                6
            }
            0xCF => {
                let addr = self.addr_abs(bus);
                self.rmw_dcp(bus, addr);
                6
            }
            0xDF => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_dcp(bus, addr);
                7
            }
            0xDB => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_dcp(bus, addr);
                7
            }
            0xC3 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_dcp(bus, addr);
                8
            }
            0xD3 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_dcp(bus, addr);
                8
            }

            // ---- ISB / ISC (INC then SBC) ----
            0xE7 => {
                let addr = self.addr_zp(bus);
                self.rmw_isb(bus, addr);
                5
            }
            0xF7 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_isb(bus, addr);
                6
            }
            0xEF => {
                let addr = self.addr_abs(bus);
                self.rmw_isb(bus, addr);
                6
            }
            0xFF => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_isb(bus, addr);
                7
            }
            0xFB => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_isb(bus, addr);
                7
            }
            0xE3 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_isb(bus, addr);
                8
            }
            0xF3 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_isb(bus, addr);
                8
            }

            // ---- SLO (ASL then ORA) ----
            0x07 => {
                let addr = self.addr_zp(bus);
                self.rmw_slo(bus, addr);
                5
            }
            0x17 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_slo(bus, addr);
                6
            }
            0x0F => {
                let addr = self.addr_abs(bus);
                self.rmw_slo(bus, addr);
                6
            }
            0x1F => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_slo(bus, addr);
                7
            }
            0x1B => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_slo(bus, addr);
                7
            }
            0x03 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_slo(bus, addr);
                8
            }
            0x13 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_slo(bus, addr);
                8
            }

            // ---- RLA (ROL then AND) ----
            0x27 => {
                let addr = self.addr_zp(bus);
                self.rmw_rla(bus, addr);
                5
            }
            0x37 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_rla(bus, addr);
                6
            }
            0x2F => {
                let addr = self.addr_abs(bus);
                self.rmw_rla(bus, addr);
                6
            }
            0x3F => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_rla(bus, addr);
                7
            }
            0x3B => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_rla(bus, addr);
                7
            }
            0x23 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_rla(bus, addr);
                8
            }
            0x33 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_rla(bus, addr);
                8
            }

            // ---- SRE (LSR then EOR) ----
            0x47 => {
                let addr = self.addr_zp(bus);
                self.rmw_sre(bus, addr);
                5
            }
            0x57 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_sre(bus, addr);
                6
            }
            0x4F => {
                let addr = self.addr_abs(bus);
                self.rmw_sre(bus, addr);
                6
            }
            0x5F => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_sre(bus, addr);
                7
            }
            0x5B => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_sre(bus, addr);
                7
            }
            0x43 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_sre(bus, addr);
                8
            }
            0x53 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_sre(bus, addr);
                8
            }

            // ---- RRA (ROR then ADC) ----
            0x67 => {
                let addr = self.addr_zp(bus);
                self.rmw_rra(bus, addr);
                5
            }
            0x77 => {
                let addr = self.addr_zp_x(bus);
                self.rmw_rra(bus, addr);
                6
            }
            0x6F => {
                let addr = self.addr_abs(bus);
                self.rmw_rra(bus, addr);
                6
            }
            0x7F => {
                let (addr, _) = self.addr_abs_x(bus);
                self.rmw_rra(bus, addr);
                7
            }
            0x7B => {
                let (addr, _) = self.addr_abs_y(bus);
                self.rmw_rra(bus, addr);
                7
            }
            0x63 => {
                let addr = self.addr_ind_x(bus);
                self.rmw_rra(bus, addr);
                8
            }
            0x73 => {
                let (addr, _) = self.addr_ind_y(bus);
                self.rmw_rra(bus, addr);
                8
            }

            // ---- ANC (AND #imm; carry copies bit 7) ----
            0x0B | 0x2B => {
                let m = self.fetch_byte(bus);
                self.a &= m;
                let a = self.a;
                self.set_zn(a);
                self.set_flag(FLAG_C, a & 0x80 != 0);
                2
            }

            // ---- ALR (AND #imm then LSR A) ----
            0x4B => {
                let m = self.fetch_byte(bus);
                self.a &= m;
                let v = self.op_lsr_value(self.a);
                self.a = v;
                2
            }

            // ---- ARR (AND #imm then ROR A; flags peculiar) ----
            0x6B => {
                let m = self.fetch_byte(bus);
                self.a &= m;
                let old_c = self.p & FLAG_C != 0;
                let r = (self.a >> 1) | ((old_c as u8) << 7);
                self.a = r;
                self.set_zn(r);
                // C from bit 6 of result, V from bit 6 ^ bit 5.
                self.set_flag(FLAG_C, r & 0x40 != 0);
                self.set_flag(FLAG_V, ((r >> 6) ^ (r >> 5)) & 0x01 != 0);
                2
            }

            // ---- SBX / AXS (X = (A & X) - imm; sets C like CMP) ----
            0xCB => {
                let m = self.fetch_byte(bus);
                let lhs = self.a & self.x;
                let r = lhs.wrapping_sub(m);
                self.set_flag(FLAG_C, lhs >= m);
                self.x = r;
                self.set_zn(r);
                2
            }

            // ---- LXA / ATX ($AB) — A,X = (A | magic) & imm. Use magic = 0xFF
            //      (the most common literature value), reducing to A,X = imm.
            0xAB => {
                let m = self.fetch_byte(bus);
                let r = m;
                self.a = r;
                self.x = r;
                self.set_zn(r);
                2
            }

            // ---- ANE / XAA ($8B) — A = (A | magic) & X & imm. Same magic
            //      assumption as LXA.
            0x8B => {
                let m = self.fetch_byte(bus);
                let r = self.x & m;
                self.a = r;
                self.set_zn(r);
                2
            }

            // ---- LAS ($BB) — A,X,SP = mem(abs,Y) & SP. ----
            0xBB => {
                let (addr, c) = self.addr_abs_y(bus);
                let m = bus.read(addr) & self.sp;
                self.a = m;
                self.x = m;
                self.sp = m;
                self.set_zn(m);
                4 + c as u32
            }

            // ---- TAS ($9B) — SP = A & X; mem = SP & (high+1). ----
            0x9B => {
                let base = self.fetch_word(bus);
                let addr = base.wrapping_add(self.y as u16);
                self.sp = self.a & self.x;
                let v = self.sp & ((base >> 8) as u8).wrapping_add(1);
                bus.write(addr, v);
                5
            }

            // ---- SHA / AHX ($93,$9F) — mem = A & X & (high+1). ----
            0x9F => {
                let base = self.fetch_word(bus);
                let addr = base.wrapping_add(self.y as u16);
                let v = self.a & self.x & ((base >> 8) as u8).wrapping_add(1);
                bus.write(addr, v);
                5
            }
            0x93 => {
                let zp = self.fetch_byte(bus);
                let lo = bus.read(zp as u16) as u16;
                let hi = bus.read(zp.wrapping_add(1) as u16) as u16;
                let base = (hi << 8) | lo;
                let addr = base.wrapping_add(self.y as u16);
                let v = self.a & self.x & ((base >> 8) as u8).wrapping_add(1);
                bus.write(addr, v);
                6
            }

            // ---- SHX ($9E) — mem(abs,Y) = X & (high+1). ----
            0x9E => {
                let base = self.fetch_word(bus);
                let addr = base.wrapping_add(self.y as u16);
                let v = self.x & ((base >> 8) as u8).wrapping_add(1);
                bus.write(addr, v);
                5
            }

            // ---- SHY ($9C) — mem(abs,X) = Y & (high+1). ----
            0x9C => {
                let base = self.fetch_word(bus);
                let addr = base.wrapping_add(self.x as u16);
                let v = self.y & ((base >> 8) as u8).wrapping_add(1);
                bus.write(addr, v);
                5
            }

            // ---- KIL / JAM (every $x2 except the legal $A2 LDX#imm) ----
            0x02 | 0x12 | 0x22 | 0x32 | 0x42 | 0x52 | 0x62 | 0x72 | 0x92 | 0xB2 | 0xD2 | 0xF2 => {
                self.halted = true;
                2
            }
        }
    }

    fn rmw_dcp(&mut self, bus: &mut NesBus, addr: u16) {
        let v = bus.read(addr).wrapping_sub(1);
        bus.write(addr, v);
        let a = self.a;
        self.op_cmp_value(a, v);
    }

    fn rmw_isb(&mut self, bus: &mut NesBus, addr: u16) {
        let v = bus.read(addr).wrapping_add(1);
        bus.write(addr, v);
        self.op_sbc(v);
    }

    fn rmw_slo(&mut self, bus: &mut NesBus, addr: u16) {
        let v = self.op_asl_value(bus.read(addr));
        bus.write(addr, v);
        self.op_ora(v);
    }

    fn rmw_rla(&mut self, bus: &mut NesBus, addr: u16) {
        let v = self.op_rol_value(bus.read(addr));
        bus.write(addr, v);
        self.op_and(v);
    }

    fn rmw_sre(&mut self, bus: &mut NesBus, addr: u16) {
        let v = self.op_lsr_value(bus.read(addr));
        bus.write(addr, v);
        self.op_eor(v);
    }

    fn rmw_rra(&mut self, bus: &mut NesBus, addr: u16) {
        let v = self.op_ror_value(bus.read(addr));
        bus.write(addr, v);
        self.op_adc(v);
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

    // ---- Unofficial opcodes ----

    #[test]
    fn lax_loads_a_and_x() {
        let (cpu, _) = run_one(&[0xA7, 0x10], |_, b| b.ram[0x10] = 0x77);
        assert_eq!(cpu.a, 0x77);
        assert_eq!(cpu.x, 0x77);
    }

    #[test]
    fn sax_writes_a_and_x() {
        let (_cpu, bus) = run_one(&[0x87, 0x10], |c, _| {
            c.a = 0xF0;
            c.x = 0x3F;
        });
        assert_eq!(bus.ram[0x10], 0x30);
    }

    #[test]
    fn dcp_decrements_then_compares() {
        let (cpu, bus) = run_one(&[0xC7, 0x20], |c, b| {
            c.a = 0x09;
            b.ram[0x20] = 0x0A;
        });
        assert_eq!(bus.ram[0x20], 0x09);
        // 0x09 == 0x09 → Z=1, C=1
        assert!(cpu.p & FLAG_Z != 0);
        assert!(cpu.p & FLAG_C != 0);
    }

    #[test]
    fn isb_increments_then_subtracts() {
        let (cpu, bus) = run_one(&[0xE7, 0x20], |c, b| {
            c.a = 0x10;
            c.p |= FLAG_C;
            b.ram[0x20] = 0x04;
        });
        assert_eq!(bus.ram[0x20], 0x05);
        // 0x10 - 0x05 = 0x0B
        assert_eq!(cpu.a, 0x0B);
    }

    #[test]
    fn slo_shifts_then_ors() {
        let (cpu, bus) = run_one(&[0x07, 0x20], |c, b| {
            c.a = 0x01;
            b.ram[0x20] = 0x40;
        });
        assert_eq!(bus.ram[0x20], 0x80);
        assert_eq!(cpu.a, 0x81);
    }

    #[test]
    fn rla_rotates_then_ands() {
        let (cpu, bus) = run_one(&[0x27, 0x20], |c, b| {
            c.a = 0xFF;
            c.p |= FLAG_C;
            b.ram[0x20] = 0x40;
        });
        // 0x40 ROL with C=1 → 0x81; A & 0x81 = 0x81
        assert_eq!(bus.ram[0x20], 0x81);
        assert_eq!(cpu.a, 0x81);
    }

    #[test]
    fn anc_copies_n_to_c() {
        let (cpu, _) = run_one(&[0x0B, 0x80], |c, _| c.a = 0xF0);
        assert_eq!(cpu.a, 0x80);
        assert!(cpu.p & FLAG_C != 0);
        assert!(cpu.p & FLAG_N != 0);
    }

    #[test]
    fn sbx_subtracts_from_a_and_x() {
        let (cpu, _) = run_one(&[0xCB, 0x05], |c, _| {
            c.a = 0xF0;
            c.x = 0x0F;
        });
        // (0xF0 & 0x0F) = 0x00; 0x00 - 0x05 wraps; C=0
        assert_eq!(cpu.x, 0xFB);
        assert_eq!(cpu.p & FLAG_C, 0);

        let (cpu, _) = run_one(&[0xCB, 0x05], |c, _| {
            c.a = 0xFF;
            c.x = 0x0F;
        });
        // (0xFF & 0x0F) = 0x0F; 0x0F - 0x05 = 0x0A; C=1
        assert_eq!(cpu.x, 0x0A);
        assert!(cpu.p & FLAG_C != 0);
    }

    #[test]
    fn jam_halts_the_cpu() {
        let mut bus = NesBus::new();
        bus.load_program(0x8000, &[0x02]);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.step(&mut bus);
        assert!(cpu.halted);
        // Subsequent step is a 1-cycle no-op.
        let cy = cpu.step(&mut bus);
        assert_eq!(cy, 1);
    }

    #[test]
    fn duplicate_sbc_eb_matches_e9() {
        let (cpu1, _) = run_one(&[0xE9, 0x05], |c, _| {
            c.a = 0x10;
            c.p |= FLAG_C;
        });
        let (cpu2, _) = run_one(&[0xEB, 0x05], |c, _| {
            c.a = 0x10;
            c.p |= FLAG_C;
        });
        assert_eq!(cpu1.a, cpu2.a);
        assert_eq!(cpu1.p, cpu2.p);
    }

    #[test]
    fn unofficial_nops_advance_pc_correctly() {
        // $80 #imm → 2 bytes, 2 cycles.
        let mut bus = NesBus::new();
        bus.load_program(0x8000, &[0x80, 0xAA, 0xEA]);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        let cy = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0x8002);
        assert_eq!(cy, 2);
    }

    // ---- NSF2 IRQ + NMI servicing ----

    fn arm_irq_bus(handler: u16) -> NesBus {
        // Fill PRG with NOPs and place the IRQ vector at 0xFFFE/0xFFFF
        // by armed overlay. Header is faked with NSF2 IRQ-support
        // enabled so the bus routes $401B-$401D into the timer.
        let mut bus = NesBus::new();
        let mut prog = vec![0xEAu8; crate::bus::PRG_ROM_SIZE];
        // Stuff RTI ($40) somewhere reachable so a serviced IRQ can
        // return cleanly if the test cares.
        prog[0x100] = 0x40;
        let h = crate::header::NsfHeader {
            version: 2,
            total_songs: 1,
            starting_song: 1,
            load_addr: 0x8000,
            init_addr: 0x8000,
            play_addr: 0x8001,
            song_name: String::new(),
            artist: String::new(),
            copyright: String::new(),
            ntsc_speed_us: 16666,
            pal_speed_us: 19997,
            bankswitch_init: [0u8; 8],
            region: crate::header::NsfRegion::Ntsc,
            expansion: crate::header::ExpansionChips(0),
            program: prog,
            track_labels: Vec::new(),
            is_nsfe: false,
            nsf2: crate::header::Nsf2Features(0x10),
            nsf2_metadata: Vec::new(),
            metadata: crate::nsfe::NsfeMetadata::default(),
        };
        bus.configure_from_header(&h);
        bus.arm_vector_overlay(0x4FFE, 0x4FFE);
        bus.write(0xFFFE, handler as u8);
        bus.write(0xFFFF, (handler >> 8) as u8);
        bus
    }

    #[test]
    fn cpu_services_irq_when_i_flag_clear_and_line_asserted() {
        let mut bus = arm_irq_bus(0x9100);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.sp = 0xFD;
        cpu.p = FLAG_U; // I=0
                        // Arm the NSF2 timer with a tiny period so an IRQ is pending
                        // by the time we step.
        bus.write(0x401B, 1);
        bus.write(0x401C, 0);
        bus.write(0x401D, 1);
        bus.tick_cycles(4);
        assert!(bus.irq_line());
        let cy = cpu.step(&mut bus);
        assert_eq!(cy, 7, "IRQ dispatch should take 7 cycles");
        assert_eq!(cpu.pc, 0x9100, "should have jumped through $FFFE/$FFFF");
        assert!(cpu.p & FLAG_I != 0, "I should be set after IRQ service");
        // The pushed P should have B clear and U set.
        let pushed = bus.ram[0x0100 + cpu.sp as usize + 1];
        assert_eq!(pushed & FLAG_B, 0);
        assert_eq!(pushed & FLAG_U, FLAG_U);
    }

    #[test]
    fn step_includes_dmc_dma_stall_cycles() {
        // A DMC sample-byte fetch during an instruction stalls the CPU
        // (§"Memory reader"); step() must report the stretched cycle
        // count so callers scheduling off the return value stay in
        // sync with the APU.
        let mut bus = NesBus::new();
        bus.write(0x4010, 0x4F); // loop, fastest rate
        bus.write(0x4012, 0x00);
        bus.write(0x4013, 0x00); // 1-byte sample
        bus.write(0x4015, 0x10); // enable → load DMA halts at cycle 4
        let mut cpu = Cpu6502::new();
        bus.ram[0x0200] = 0xEA; // NOP
        bus.ram[0x0201] = 0xEA;
        bus.ram[0x0202] = 0xEA;
        cpu.pc = 0x0200;
        cpu.p = FLAG_U | FLAG_I;
        // §"DMC DMA": the load DMA is scheduled for "a get cycle
        // during the 2nd APU cycle after the write" — cycle 4 for the
        // cycle-0 write above. The first two NOPs (cycles 0-3) run
        // unstalled; the third begins on cycle 4 and eats the stall.
        assert_eq!(cpu.step(&mut bus), 2, "cycles 0-1: before the halt");
        assert_eq!(cpu.step(&mut bus), 2, "cycles 2-3: before the halt");
        let cy = cpu.step(&mut bus);
        assert_eq!(
            cy,
            2 + crate::apu::DMC_DMA_LOAD_STALL_CYCLES,
            "NOP (2 cycles) + the cycle-4 3-cycle load-DMA stall"
        );
        // With the buffer full, the next instruction runs unstalled.
        bus.ram[0x0203] = 0xEA;
        let cy2 = cpu.step(&mut bus);
        assert_eq!(cy2, 2, "no fetch pending → no stall");
    }

    #[test]
    fn write_cycle_mask_matches_documented_shapes() {
        // apu-dma-wiki §Behavior: stores write on their final cycle,
        // "read-modify-write instructions having 2 consecutive writes
        // and interrupts having 3".
        assert_eq!(write_cycle_mask(0x8D, 4), 0b1000, "STA abs");
        assert_eq!(write_cycle_mask(0x9D, 5), 0b10000, "STA abs,X");
        assert_eq!(write_cycle_mask(0x91, 6), 0b100000, "STA (zp),Y");
        assert_eq!(write_cycle_mask(0x48, 3), 0b100, "PHA");
        assert_eq!(write_cycle_mask(0xEE, 6), 0b110000, "INC abs (RMW)");
        assert_eq!(write_cycle_mask(0x1E, 7), 0b1100000, "ASL abs,X (RMW)");
        assert_eq!(write_cycle_mask(0x03, 8), 0b11000000, "SLO (zp,X) (RMW)");
        assert_eq!(write_cycle_mask(0x00, 7), 0b0011100, "BRK pushes on 2-4");
        assert_eq!(write_cycle_mask(0x20, 6), 0b011000, "JSR pushes on 3-4");
        assert_eq!(write_cycle_mask(0xAD, 4), 0, "LDA abs never writes");
        assert_eq!(write_cycle_mask(0xEA, 2), 0, "NOP never writes");
        assert_eq!(INTERRUPT_WRITE_MASK, 0b0011100, "IRQ/NMI pushes on 2-4");
    }

    #[test]
    fn rmw_4014_write_delays_oam_halt_into_next_instruction() {
        // apu-dma-wiki §"OAM DMA": "read-modify-write instructions
        // such as INC $4014 […] are able to perform a second write
        // before the CPU can be halted" — the OAM halt slips past the
        // RMW's back-to-back write cycles onto the next instruction's
        // opcode fetch, and the stall lands on THAT step's cycle
        // count.
        let mut bus = NesBus::new();
        let mut cpu = Cpu6502::new();
        bus.ram[0x0200] = 0xEE; // INC $4014
        bus.ram[0x0201] = 0x14;
        bus.ram[0x0202] = 0x40;
        bus.ram[0x0203] = 0xEA; // NOP
        cpu.pc = 0x0200;
        cpu.p = FLAG_U | FLAG_I;
        let cy = cpu.step(&mut bus);
        assert_eq!(cy, 6, "the RMW itself finishes unstalled");
        // Halt lands on cycle 6 (a get) → alignment → 514 cycles.
        let cy2 = cpu.step(&mut bus);
        assert_eq!(cy2, 2 + 514, "NOP + the write-delayed 514-cycle OAM DMA");
    }

    #[test]
    fn sta_4014_stall_lands_on_following_instruction() {
        // The plain-store case for contrast: STA $4014's write is its
        // final cycle, the halt is "scheduled … on the first cycle
        // after the register write" — the next instruction's fetch.
        let mut bus = NesBus::new();
        let mut cpu = Cpu6502::new();
        bus.ram[0x0200] = 0x8D; // STA $4014
        bus.ram[0x0201] = 0x14;
        bus.ram[0x0202] = 0x40;
        bus.ram[0x0203] = 0xEA; // NOP
        cpu.pc = 0x0200;
        cpu.p = FLAG_U | FLAG_I;
        assert_eq!(cpu.step(&mut bus), 4, "the store itself is unstalled");
        // Halt on cycle 4 (a get) → alignment cycle → 514.
        assert_eq!(cpu.step(&mut bus), 2 + 514, "NOP + 514-cycle OAM DMA");
    }

    #[test]
    fn cpu_skips_irq_when_i_flag_set() {
        let mut bus = arm_irq_bus(0x9100);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p = FLAG_U | FLAG_I;
        bus.write(0x401B, 1);
        bus.write(0x401D, 1);
        bus.tick_cycles(4);
        assert!(bus.irq_line());
        let _ = cpu.step(&mut bus);
        // PC advanced past the NOP at $8000, not into the handler.
        assert_eq!(cpu.pc, 0x8001);
    }

    fn arm_nmi_bus(nmi_handler: u16) -> NesBus {
        let mut bus = NesBus::new();
        let prog = vec![0xEAu8; crate::bus::PRG_ROM_SIZE];
        let h = crate::header::NsfHeader {
            version: 2,
            total_songs: 1,
            starting_song: 1,
            load_addr: 0x8000,
            init_addr: 0x8000,
            play_addr: 0x8001,
            song_name: String::new(),
            artist: String::new(),
            copyright: String::new(),
            ntsc_speed_us: 16666,
            pal_speed_us: 19997,
            bankswitch_init: [0u8; 8],
            region: crate::header::NsfRegion::Ntsc,
            expansion: crate::header::ExpansionChips(0),
            program: prog,
            track_labels: Vec::new(),
            is_nsfe: false,
            nsf2: crate::header::Nsf2Features(0x20), // non-returning INIT
            nsf2_metadata: Vec::new(),
            metadata: crate::nsfe::NsfeMetadata::default(),
        };
        bus.configure_from_header(&h);
        // NMI slot is reserved to the player — install via arm.
        bus.arm_vector_overlay(nmi_handler, 0x4FFE);
        bus
    }

    #[test]
    fn cpu_services_nmi_regardless_of_i_flag() {
        let mut bus = arm_nmi_bus(0xA200);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p = FLAG_U | FLAG_I; // I set — NMI must still fire
        cpu.sp = 0xFD;
        bus.request_nmi();
        let cy = cpu.step(&mut bus);
        assert_eq!(cy, 7);
        assert_eq!(cpu.pc, 0xA200);
        assert!(cpu.p & FLAG_I != 0);
    }

    #[test]
    fn nmi_request_drains_once() {
        let mut bus = arm_nmi_bus(0xA200);
        let mut cpu = Cpu6502::new();
        cpu.pc = 0x8000;
        cpu.p = FLAG_U;
        cpu.sp = 0xFD;
        bus.request_nmi();
        let _ = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0xA200);
        // Step again: no further NMI; $A200 is NOP-filled PRG, so PC
        // advances past one NOP.
        let _ = cpu.step(&mut bus);
        assert_eq!(cpu.pc, 0xA201);
    }
}
