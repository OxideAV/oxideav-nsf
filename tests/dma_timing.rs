//! End-to-end checks for the sub-instruction DMA engine
//! (`docs/audio/nsf/apu-dma-wiki.html`): the CPU/bus/APU cycle
//! accounting must stay consistent while DMC and OAM DMA steal time
//! out of real instruction streams.

use oxideav_nsf::{Cpu6502, NesBus};

/// Load `program` at $0200 and return a (cpu, bus) pair ready to step.
fn machine_with(program: &[u8]) -> (Cpu6502, NesBus) {
    let mut bus = NesBus::new();
    bus.ram[0x0200..0x0200 + program.len()].copy_from_slice(program);
    let mut cpu = Cpu6502::new();
    cpu.pc = 0x0200;
    cpu.p = 0x24; // I + U
    (cpu, bus)
}

#[test]
fn machine_cycles_match_step_accounting_under_dma_pressure() {
    // Every CPU cycle and every DMA-stolen cycle must be accounted
    // exactly once: the bus clock's advance has to equal the sum of
    // `Cpu6502::step` returns (instruction cycles + drained DMA
    // stalls). A bookkeeping slip here would silently skew the PLAY
    // cadence of every DPCM-heavy rip.
    let program: &[u8] = &[
        0xA9, 0x4F, // LDA #$4F        (DMC: loop, fastest rate)
        0x8D, 0x10, 0x40, // STA $4010
        0xA9, 0x00, // LDA #$00
        0x8D, 0x12, 0x40, // STA $4012  (sample address $C000)
        0x8D, 0x13, 0x40, // STA $4013  (1-byte sample)
        0xA9, 0x10, // LDA #$10
        0x8D, 0x15, 0x40, // STA $4015  (DMC on → load DMA)
        // loop: an RMW on $4014 (write-delayed OAM halt), a plain
        // store to $4014, some reads, repeat.
        0xEE, 0x14, 0x40, // INC $4014
        0xAD, 0x15, 0x40, // LDA $4015
        0x8D, 0x14, 0x40, // STA $4014
        0xEA, // NOP
        0x4C, 0x0F, 0x02, // JMP loop ($020F)
    ];
    let (mut cpu, mut bus) = machine_with(program);
    let start = bus.cycles;
    let mut reported: u64 = 0;
    for _ in 0..2_000 {
        reported += cpu.step(&mut bus) as u64;
    }
    assert_eq!(
        bus.cycles - start,
        reported,
        "bus clock and step() accounting must agree cycle-for-cycle"
    );
    assert!(
        reported > 2_000 * 4,
        "the OAM DMAs must actually have stolen time (got {reported})"
    );
}

#[test]
fn implicit_stop_rip_keeps_accounting_consistent() {
    // Non-looping 1-byte sample restarted forever: every restart ends
    // with the implicit-stop unexpected DMA (NTSC), so the §Bugs path
    // runs constantly inside a real instruction stream.
    let program: &[u8] = &[
        0xA9, 0x0F, // LDA #$0F        (DMC: no loop, fastest rate)
        0x8D, 0x10, 0x40, // STA $4010
        0xA9, 0x00, // LDA #$00
        0x8D, 0x12, 0x40, // STA $4012
        0x8D, 0x13, 0x40, // STA $4013  (1-byte sample)
        // loop: restart the sample, poll status, occasionally abort
        // the armed fetch with an RMW disable ($10 → $0F clears D4).
        0xA9, 0x10, // LDA #$10
        0x8D, 0x15, 0x40, // STA $4015  (DMC on)
        0xAD, 0x15, 0x40, // LDA $4015
        0xCE, 0x15, 0x40, // DEC $4015  (RMW: D4 off mid-instruction)
        0x4C, 0x0D, 0x02, // JMP loop ($020D)
    ];
    let (mut cpu, mut bus) = machine_with(program);
    let start = bus.cycles;
    let mut reported: u64 = 0;
    for _ in 0..20_000 {
        reported += cpu.step(&mut bus) as u64;
    }
    assert_eq!(
        bus.cycles - start,
        reported,
        "§Bugs stop-timing paths must not desync the clock"
    );
}

#[test]
fn interrupt_dispatch_write_cycles_delay_dma_halt() {
    // apu-dma-wiki §Behavior: "interrupts having 3 [consecutive
    // writes]". An IRQ dispatch's stack pushes occupy cycles 2-4; a
    // DMC DMA scheduled into them must wait for a read cycle, and the
    // 7-cycle dispatch itself completes unstalled if the halt lands
    // after it.
    let mut bus = NesBus::new();
    // Live DPCM so reload DMAs keep landing around the dispatches.
    bus.write(0x4010, 0x4F);
    bus.write(0x4012, 0x00);
    bus.write(0x4013, 0x00);
    bus.write(0x4015, 0x10);
    // NSF2 IRQ timer as the interrupt source.
    bus.nsf2_timer.enabled = true;
    // Handler at $0300: just RTI. Vector via cart RAM is not mapped
    // for $FFFE on a fresh bus (open ROM = 0x00), so park the handler
    // at $0000 and make it a NOP sled into RTI.
    bus.ram[0x0000] = 0xEA; // NOP
    bus.ram[0x0001] = 0x40; // RTI
    bus.write(0x401B, 0x02); // reload lo
    bus.write(0x401C, 0x00); // reload hi
    bus.write(0x401D, 0x01); // activate: IRQ every 3 cycles
    let mut cpu = Cpu6502::new();
    bus.ram[0x0200] = 0xEA;
    bus.ram[0x0201] = 0xEA;
    cpu.pc = 0x0200;
    cpu.p = 0x20; // I clear — IRQs serviced
    let start = bus.cycles;
    let mut reported: u64 = 0;
    for _ in 0..1_000 {
        reported += cpu.step(&mut bus) as u64;
    }
    assert_eq!(
        bus.cycles - start,
        reported,
        "interrupt dispatch pattern must keep the clock consistent"
    );
}
