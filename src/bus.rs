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
//! | `$4014`          | OAM DMA — 513/514-cycle CPU halt (no PPU to receive it)     |
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

/// Base CPU cycles a `$4014` OAM DMA steals when its halt lands on a
/// put cycle: the halt cycle + 256 get/put pairs. A get-half halt
/// adds one alignment cycle for the documented 514-cycle case
/// (`docs/audio/nsf/apu-dma-wiki.html` §"OAM DMA": "taking 513 or 514
/// cycles, depending on whether alignment is needed").
pub const OAM_DMA_BASE_STALL_CYCLES: u32 = 513;

/// Extra CPU cycles a DMC sample fetch costs when it collides with an
/// in-progress OAM DMA in the common (mid-window) case: "DMC DMA
/// occurring during OAM DMA will cost only 2 cycles: 1 cycle for the
/// DMC DMA get and then 1 cycle for OAM DMA to align back to a get"
/// (`docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA during OAM DMA").
/// The end-of-window special cases cost 1 (second-to-last put) or 3
/// (last put) cycles instead and fall out of the cycle-stepped window
/// walk in [`NesBus::run_oam_window`].
pub const DMC_DMA_DURING_OAM_STALL_CYCLES: u32 = 2;

// =====================================================================
// Sub-instruction DMA engine
// =====================================================================
//
// `docs/audio/nsf/apu-dma-wiki.html`:
//
// * §Cadence — "The CPU alternates between cycles on which DMA can get
//   (read) and cycles on which DMA can put (write). These are the
//   first and second halves of APU cycles, respectively." This crate
//   pins the power-up alignment (random on hardware) to even CPU
//   cycles = get, matching the frame counter's event tables.
// * §Behavior — "DMA can only halt on CPU read cycles. On write
//   cycles, the halt fails and the DMA unit tries again next CPU
//   cycle, repeating until successful. […] Delays of up to 3 cycles
//   are possible, with read-modify-write instructions having 2
//   consecutive writes and interrupts having 3."
// * §"DMC DMA" — load DMAs "are scheduled to halt the CPU on a get
//   cycle during the 2nd APU cycle after the write (that is, the 3rd
//   or 4th CPU cycle)"; reload DMAs "attempt to halt on a put cycle".
//   "load DMAs take 3 cycles and reload DMAs take 4 unless the halt
//   is delayed by an odd number of cycles" — i.e. the stall is
//   3 cycles for a get-cycle halt (halt + dummy + sample get) and 4
//   for a put-cycle halt (an alignment cycle lands between the dummy
//   and the get).
//
// The 6502 core executes each instruction's *state* atomically, but
// hands the bus the instruction's per-cycle read/write pattern
// ([`crate::cpu::write_cycle_mask`]); the engine walks the pattern one
// CPU cycle at a time and places every DMA halt on its true cycle, so
// write-cycle halt delays — and the get/put parity flips they cause —
// are modelled exactly.

/// Which flavour of DMC DMA is scheduled
/// (`docs/audio/nsf/apu-dma-wiki.html` §"DMC DMA" + §Bugs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DmcDmaKind {
    /// Post-`$4015` load DMA (halt scheduled on a get cycle).
    Load,
    /// Buffer-emptied reload DMA (halt scheduled on a put cycle).
    Reload,
    /// §Bugs aborted DMA: playback stopped during the APU cycle
    /// before the reload would schedule — the DMA "is aborted after a
    /// single cycle" (its halt cycle), or skipped entirely when the
    /// halt attempt falls on a write cycle.
    Aborted,
    /// §Bugs unexpected DMA (RP2A03H / late RP2A03G): playback stopped
    /// implicitly on the same APU cycle the reload would schedule — a
    /// full reload runs anyway "from the same address" and its byte
    /// "goes into the sample buffer".
    Unexpected,
}

/// A DMC DMA whose halt attempts begin at `halt_from`.
#[derive(Clone, Copy, Debug)]
struct ScheduledDmcDma {
    addr: u16,
    kind: DmcDmaKind,
    /// Absolute CPU cycle of the first halt attempt. Attempts repeat
    /// every cycle until they land on a CPU read cycle (§Behavior),
    /// except for [`DmcDmaKind::Aborted`], which only exists at its
    /// scheduled attempt.
    halt_from: u64,
}

/// First get (even) cycle at or after `c`.
fn first_get(c: u64) -> u64 {
    if c & 1 == 0 {
        c
    } else {
        c + 1
    }
}

/// First put (odd) cycle at or after `c`.
fn first_put(c: u64) -> u64 {
    if c & 1 == 1 {
        c
    } else {
        c + 1
    }
}

/// Whether cycle `offset` of an instruction with write-cycle bitmask
/// `write_mask` is a CPU read cycle.
fn is_read_cycle(write_mask: u32, offset: u32) -> bool {
    offset >= u32::BITS || (write_mask >> offset) & 1 == 0
}

/// Cycle offset of the `n`-th (0-based) write cycle in `write_mask`.
/// The CPU performs its bus-write calls in the same order as the
/// hardware's write cycles, so the n-th `NesBus::write` of an
/// instruction happened on the n-th set bit. (An RMW instruction
/// models one write call for the hardware's two write cycles; mapping
/// it to the FIRST write cycle is exactly right for `$4014`/`$4015`
/// triggers — the halt attempt then starts on the second write cycle
/// and is delayed past it, the doc's `INC $4014` behaviour.)
fn nth_write_offset(write_mask: u32, n: u32, len: u32) -> u32 {
    let mut seen = 0u32;
    for o in 0..u32::BITS {
        if (write_mask >> o) & 1 == 1 {
            if seen == n {
                return o;
            }
            seen += 1;
        }
    }
    len.saturating_sub(1)
}

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

    /// CPU cycles stolen by DMA (DMC sample-byte fetches + `$4014`
    /// OAM DMA) since the last [`NesBus::take_dma_stall`].
    pending_dma_stall: u32,

    // ---- Sub-instruction DMA engine ----
    /// Scheduled DMC DMA, if any. Serviced during the per-cycle walk.
    dmc_dma: Option<ScheduledDmcDma>,
    /// Absolute CPU cycle from which a pending `$4014` OAM DMA
    /// attempts to halt the CPU ("scheduled to halt the CPU on the
    /// first cycle after the register write").
    oam_halt_from: Option<u64>,
    /// Halt-attempt cycle for the next load DMA, latched when `$4015`
    /// D4 is set: "a get cycle during the 2nd APU cycle after the
    /// write (that is, the 3rd or 4th CPU cycle)".
    dmc_load_halt_from: Option<u64>,
    /// True between [`NesBus::begin_instruction`] and
    /// [`NesBus::run_instruction`] — bus writes are counted so the
    /// `$4014`/`$4015` triggers can be mapped to their true cycle
    /// offsets within the instruction.
    in_instruction: bool,
    /// Number of `NesBus::write` calls made by the current instruction.
    instr_write_calls: u32,
    /// Write-call index of the `$4014` OAM DMA trigger, if the current
    /// instruction wrote it.
    oam_trigger_call: Option<u32>,
    /// Time-sensitive register writes made by the current instruction,
    /// as `(write-call index, addr, value)`. Applied at their true
    /// cycle offsets during the walk so the `$4017` phase-dependent
    /// frame-reset delay, the `$4015` D4 DMA scheduling / §Bugs
    /// stop-timing, and the cycle-counting NSF2 timer all see the
    /// hardware write cycle instead of the instruction's first cycle.
    deferred_writes: Vec<(u32, u16, u8)>,
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
            pending_dma_stall: 0,
            dmc_dma: None,
            oam_halt_from: None,
            dmc_load_halt_from: None,
            in_instruction: false,
            instr_write_calls: 0,
            oam_trigger_call: None,
            deferred_writes: Vec::new(),
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

        // FDS turns `$8000..=$FFFF` into RAM, so every read/write in
        // that window is serviced from `fds_ram`. The bankswitched path
        // (`load_bankswitched`) sizes + primes it from the bank pool;
        // the non-bankswitched `load_program` path leaves the program
        // in `self.prg`, so mirror it into `fds_ram` here. Without this
        // an FDS-flagged non-bankswitched header left `fds_ram` empty
        // and any `$8000..` access (read at `bank_read`, write at
        // `0x8000..=0xFFFF`) indexed a zero-length vector and panicked.
        if self.fds_enabled && self.fds_ram.is_empty() {
            self.fds_ram = vec![0u8; 0x8000];
            // `self.prg` holds the loaded program at `load_addr - 0x8000`
            // offsets; copy it into the FDS RAM image so reads see the
            // program bytes the player just installed.
            self.fds_ram.copy_from_slice(&self.prg[..0x8000]);
        }

        // NSF2: arm IRQ timer + vector overlay if the feature byte
        // requests it. The player layer drives `arm_vector_overlay`
        // with concrete sentinel addresses; we just gate the
        // timer-device address decode here.
        self.nsf2_timer.enabled = header.nsf2.irq_support();
    }

    /// The documented per-tune initialization scrub, per
    /// `docs/audio/nsf/nsf-nesdev-wiki.html` §"Initializing a tune"
    /// (mirrored by `docs/audio/nsf/nsfspec-kevtris-v1.61.txt`
    /// §"'Proper' way to init a tune"):
    ///
    /// * "Write $00 to all RAM at $0000-$07FF and $6000-$7FFF."
    /// * "Initialize the sound registers by writing $00 to
    ///   $4000-$4013, and $00 then $0F to $4015."
    /// * "Initialize the frame counter to 4-step mode ($40 to $4017)."
    /// * "If the tune is bank switched, load the bank values from
    ///   $070-$077 into $5FF8-$5FFF."
    ///
    /// Called before every `INIT` invocation so switching songs starts
    /// from the same state as the first one. A non-bankswitched tune
    /// whose load address falls below `$8000` keeps its program bytes:
    /// they are reloaded into `$6000-$7FFF` after the RAM clear (the
    /// documented sequence clears RAM *before* the tune data is
    /// placed). For FDS, the RAM image at `$8000..=$FFFF` is re-primed
    /// from the bank pool / program so a previous song's
    /// self-modifications are discarded, and the `$5FF6..=$5FF7`
    /// extended bank registers are re-seeded alongside `$5FF8-$5FFF`.
    pub fn reset_for_tune(&mut self, header: &NsfHeader) {
        self.ram.fill(0);
        self.cart_ram.fill(0);
        for reg in 0x4000..=0x4013u16 {
            self.write(reg, 0x00);
        }
        self.write(0x4015, 0x00);
        self.write(0x4015, 0x0F);
        self.write(0x4017, 0x40);
        if self.bankswitched {
            self.bank_select = header.bankswitch_init;
            if self.fds_enabled {
                // Re-prime the FDS RAM image from the (immutable) bank
                // pool, exactly as the initial load did, then re-seed
                // the FDS-extended `$5FF6..=$5FF7` window registers.
                for (window, &sel) in self.bank_select.iter().enumerate() {
                    if let Some(bank) = self.bank_pool.get(sel as usize) {
                        let dst = window * BANK_SIZE;
                        self.fds_ram[dst..dst + BANK_SIZE].copy_from_slice(bank);
                    }
                }
                self.write(0x5FF6, header.bankswitch_init[6]);
                self.write(0x5FF7, header.bankswitch_init[7]);
            }
        } else {
            // Reload the program image: a load address below $8000
            // places tune bytes in the $6000-$7FFF RAM we just cleared.
            self.load_program(header.load_addr, &header.program);
            if self.fds_enabled && self.fds_ram.len() == 0x8000 {
                self.fds_ram.copy_from_slice(&self.prg[..0x8000]);
            }
        }
        self.nmi_pending = false;
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
                } else if self.fds_enabled {
                    // FDS makes `$8000..=$FFFF` writable RAM; reads must
                    // come from the same `fds_ram` image the write path
                    // updates so a self-modifying FDS program sees its
                    // own writes (the bankswitched path already routes
                    // through `bank_read` → `fds_ram`).
                    self.fds_ram[(addr - 0x8000) as usize]
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
        if self.in_instruction {
            self.instr_write_calls = self.instr_write_calls.wrapping_add(1);
            match addr {
                // Replayed at the write's true cycle offset by
                // `run_instruction` — the halt is "scheduled … on the
                // first cycle after the register write".
                0x4014 => {
                    self.oam_trigger_call = Some(self.instr_write_calls - 1);
                    return;
                }
                // Time-sensitive 2A03 APU + NSF2-timer registers are
                // deferred to the write's true cycle offset: on
                // hardware the register changes on the store's final
                // cycle, not on the instruction's first — the `$4017`
                // frame-reset delay is phase-dependent, the `$4015`
                // D4 edge schedules the load DMA, and the NSF2 timer
                // counts CPU cycles. Memory-class writes (RAM, cart
                // RAM, FDS RAM, bank selects) apply immediately so
                // the instruction-atomic core stays self-consistent;
                // expansion-chip registers also stay immediate —
                // their internal clocks are documented as
                // batch-stepped in the README's known gaps.
                0x4000..=0x4013 | 0x4015 | 0x4017 | 0x401B..=0x401D => {
                    self.deferred_writes
                        .push((self.instr_write_calls - 1, addr, value));
                    return;
                }
                _ => {}
            }
        } else if addr == 0x4014 {
            // Direct (CPU-less) write, e.g. from tests: the write
            // occupies the current cycle and the halt lands on the
            // next one.
            self.tick_machine(1);
            self.run_oam_window();
            return;
        }
        self.apply_write(addr, value);
    }

    /// Apply a bus write's side effects (see [`NesBus::write`] for the
    /// deferral rules — this runs either immediately or at the write's
    /// true cycle offset during the instruction walk).
    fn apply_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram[(addr & 0x07FF) as usize] = value,
            0x2000..=0x3FFF => {}
            0x4000..=0x4013 => self.apu.write_register(addr, value),
            // Handled in `write` / the walk — never deferred here.
            0x4014 => {}
            0x4015 => {
                self.apu.write_status_except_dmc_enable(value);
                self.apply_dmc_enable(value & 0x10 != 0);
            }
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
    ///
    /// All cycles are treated as CPU *read* cycles (the sentinel-idle
    /// spin has no writes), so a scheduled DMA halts at its earliest
    /// documented attempt. CPU-executed instructions instead go
    /// through [`NesBus::run_instruction`], which carries the
    /// instruction's true read/write cycle pattern.
    pub fn tick_cycles(&mut self, cycles: u32) {
        self.walk_pattern(cycles, 0, None, &[]);
    }

    /// Mark the start of a CPU instruction (or interrupt dispatch):
    /// bus writes are counted from here so `$4014`/`$4015` triggers
    /// can be replayed at their true cycle offsets by
    /// [`NesBus::run_instruction`].
    pub fn begin_instruction(&mut self) {
        self.in_instruction = true;
        self.instr_write_calls = 0;
        self.oam_trigger_call = None;
        self.deferred_writes.clear();
    }

    /// Advance machine time for one executed instruction of `cycles`
    /// CPU cycles whose write cycles are the set bits of `write_mask`
    /// ([`crate::cpu::write_cycle_mask`]). DMA halts land on their
    /// exact cycles: attempts fail on write cycles and retry next
    /// cycle (`docs/audio/nsf/apu-dma-wiki.html` §Behavior), flipping
    /// the get/put parity — and with it the 3/4-cycle DMC stall and
    /// the 513/514-cycle OAM window — when the delay is odd.
    pub fn run_instruction(&mut self, cycles: u32, write_mask: u32) {
        self.in_instruction = false;
        let oam = self
            .oam_trigger_call
            .take()
            .map(|k| nth_write_offset(write_mask, k, cycles));
        // Map each deferred register write's call index onto its true
        // cycle offset within the instruction.
        let mut deferred = std::mem::take(&mut self.deferred_writes);
        for entry in deferred.iter_mut() {
            entry.0 = nth_write_offset(write_mask, entry.0, cycles);
        }
        self.walk_pattern(cycles, write_mask, oam, &deferred);
        deferred.clear();
        self.deferred_writes = deferred;
    }

    /// Walk `len` CPU cycles with the given write-cycle mask, applying
    /// deferred `$4015`/`$4014` triggers at their true offsets and
    /// servicing scheduled DMAs on their exact halt cycles.
    fn walk_pattern(
        &mut self,
        len: u32,
        write_mask: u32,
        oam_trigger: Option<u32>,
        deferred: &[(u32, u16, u8)],
    ) {
        let mut o: u32 = 0;
        while o < len {
            let read_cycle = is_read_cycle(write_mask, o);
            // Deferred register writes land on their write cycles.
            for &(off, addr, value) in deferred {
                if off == o {
                    self.apply_write(addr, value);
                }
            }
            // A $4014 write schedules the OAM halt for the next cycle.
            if let Some(to) = oam_trigger {
                if to == o {
                    self.oam_halt_from = Some(self.cycles + 1);
                }
            }
            // Service any DMC DMA due before this CPU cycle executes:
            // the stall inserts ahead of the halted read, which the
            // CPU then performs ("When the DMA process completes, the
            // CPU performs the read it attempted when halted").
            self.service_dmc_dma(read_cycle);
            // OAM DMA halts on the first read cycle at/after its
            // schedule point.
            if let Some(hf) = self.oam_halt_from {
                if read_cycle && self.cycles >= hf {
                    self.oam_halt_from = None;
                    self.run_oam_window();
                    // The window may have left a DMC DMA scheduled
                    // right past its end.
                    self.service_dmc_dma(read_cycle);
                }
            }
            self.tick_machine(1);
            o += 1;
        }
    }

    /// Apply a `$4015` D4 write at the current cycle. Enables latch
    /// the load-DMA halt schedule; disables run the apu-dma-wiki
    /// §Bugs stop rules against any scheduled reload DMA.
    fn apply_dmc_enable(&mut self, on: bool) {
        if on {
            self.apu.dmc_set_enabled(true);
            // §"DMC DMA": the load DMA is "scheduled to halt the CPU
            // on a get cycle during the 2nd APU cycle after the write
            // (that is, the 3rd or 4th CPU cycle)".
            self.dmc_load_halt_from = Some(first_get(self.cycles + 3));
        } else {
            match self.dmc_dma {
                Some(s) if s.kind == DmcDmaKind::Reload => {
                    let x_apu = self.cycles >> 1;
                    let s_apu = s.halt_from >> 1;
                    if x_apu + 1 >= s_apu {
                        // "When sample playback is stopped during the
                        // APU cycle before a reload DMA would schedule
                        // […] the DMA starts, but is aborted after a
                        // single cycle." (A stop on the schedule's own
                        // APU cycle is not pinned for explicit stops;
                        // the abort is the closest documented shape.)
                        self.dmc_dma = Some(ScheduledDmcDma {
                            kind: DmcDmaKind::Aborted,
                            ..s
                        });
                    } else {
                        // Stopped earlier: the memory reader only
                        // fetches while "bytes remaining is not zero"
                        // — no DMA at all.
                        self.dmc_dma = None;
                        self.apu.dmc_cancel_in_flight();
                    }
                }
                Some(s) if s.kind == DmcDmaKind::Load => {
                    self.dmc_dma = None;
                    self.apu.dmc_cancel_in_flight();
                }
                _ => {}
            }
            self.apu.dmc_set_enabled(false);
            self.dmc_load_halt_from = None;
        }
    }

    /// Service a scheduled DMC DMA whose halt lands on the current
    /// cycle boundary. `read_cycle` is whether the CPU cycle about to
    /// execute is a read — halts only succeed on read cycles.
    fn service_dmc_dma(&mut self, read_cycle: bool) {
        let Some(s) = self.dmc_dma else { return };
        let now = self.cycles;
        let due = match s.kind {
            DmcDmaKind::Aborted => now >= s.halt_from,
            _ => read_cycle && now >= s.halt_from,
        };
        if !due {
            return;
        }
        self.dmc_dma = None;
        match s.kind {
            DmcDmaKind::Aborted => {
                // §Bugs: "aborted after a single cycle" — but "if the
                // halt is delayed due to a write cycle, the aborted
                // DMA doesn't occur at all".
                if now == s.halt_from && read_cycle {
                    self.stall_ticks(1);
                }
                self.apu.dmc_cancel_in_flight();
            }
            _ => {
                // Halt on this cycle: halt + dummy + get for a
                // get-cycle halt (3), plus an alignment cycle for a
                // put-cycle halt (4).
                let stall = if now & 1 == 0 { 3 } else { 4 };
                self.stall_ticks(stall);
                let byte = self.read(s.addr);
                if s.kind == DmcDmaKind::Unexpected {
                    self.apu.dmc_supply_unexpected_byte(byte);
                } else {
                    self.apu.dmc_supply_byte(byte);
                }
            }
        }
    }

    /// `$4014` OAM DMA window, entered on its halt cycle — the CPU
    /// halt is real even though the NSF machine has no PPU to receive
    /// the 256 bytes.
    ///
    /// `docs/audio/nsf/apu-dma-wiki.html` §"OAM DMA": the DMA "halts
    /// the CPU, performs an optional alignment cycle, and then gets
    /// and puts 256 times, taking 513 or 514 cycles" — 513 when the
    /// halt lands on a put (the first read starts on the next get),
    /// 514 when it lands on a get (an alignment cycle is spent before
    /// the first read). Game rips frequently keep their engine's
    /// sprite-DMA write in the PLAY routine, so this ~513-cycle bite
    /// out of the frame is real wall-clock time on hardware.
    ///
    /// §"DMC DMA during OAM DMA": "When accesses collide, DMC DMA is
    /// allowed to run and OAM DMA is paused". The cycle walk below
    /// reproduces the doc's three costs exactly: 2 extra cycles in
    /// the common case ("1 cycle for the DMC DMA get and then 1 cycle
    /// for OAM DMA to align back to a get"), 1 when the DMC halt
    /// lands on the second-to-last OAM put (the dummy + alignment
    /// overlap OAM's final pair and no realign is needed), and 3 on
    /// the last put (dummy + alignment + get all extend the window).
    ///
    /// The 256 OAM source reads are not replayed through the bus (no
    /// PPU, and replaying them could spuriously trigger read-side
    /// register effects the real DMA would only cause for exotic
    /// source pages); only the CPU-time cost is modelled, with the
    /// APU + NSF2 timer running on through the halt.
    fn run_oam_window(&mut self) {
        let start = self.cycles;
        // Halt cycle.
        self.tick_machine(1);
        if start & 1 == 0 {
            // Halt landed on a get: alignment before the first read.
            self.tick_machine(1);
        }
        let mut remaining: u32 = 512;
        while remaining > 0 {
            let dmc_due = match self.dmc_dma {
                Some(s) => {
                    s.kind != DmcDmaKind::Aborted
                        && self.cycles >= s.halt_from
                        && self.cycles & 1 == 1
                }
                None => false,
            };
            if dmc_due {
                let s = self.dmc_dma.take().expect("checked above");
                // DMC halt overlaps this OAM put; its dummy and
                // alignment cycles overlap the next OAM pair (when
                // one remains).
                self.tick_machine(1);
                remaining -= 1;
                self.tick_machine(1);
                remaining = remaining.saturating_sub(1);
                self.tick_machine(1);
                remaining = remaining.saturating_sub(1);
                // The DMC get takes precedence for its sample read.
                let byte = self.read(s.addr);
                if s.kind == DmcDmaKind::Unexpected {
                    self.apu.dmc_supply_unexpected_byte(byte);
                } else {
                    self.apu.dmc_supply_byte(byte);
                }
                self.tick_machine(1);
                if remaining > 0 {
                    // OAM re-aligns back to a get.
                    self.tick_machine(1);
                }
                continue;
            }
            self.tick_machine(1);
            remaining -= 1;
        }
        let total = (self.cycles - start) as u32;
        self.pending_dma_stall = self.pending_dma_stall.saturating_add(total);
    }

    /// Insert `n` DMA-stolen cycles: the APU + NSF2 timer run on
    /// through the stall and the total is accumulated for
    /// [`NesBus::take_dma_stall`].
    fn stall_ticks(&mut self, n: u32) {
        self.pending_dma_stall = self.pending_dma_stall.saturating_add(n);
        self.tick_machine(n);
    }

    /// Advance machine time by `n` CPU cycles. While DMC DMA activity
    /// is possible the walk is cycle-exact (arm events must be
    /// observed on their true cycles); otherwise ticks run in cheap
    /// 8-cycle chunks exactly as before the sub-instruction engine.
    fn tick_machine(&mut self, n: u32) {
        let mut rem = n;
        while rem > 0 {
            if self.apu.dmc_activity_possible() {
                self.apu.tick_cpu_cycles(1);
                self.nsf2_timer.tick(1);
                self.cycles = self.cycles.wrapping_add(1);
                rem -= 1;
                self.observe_dmc_events();
            } else {
                let k = rem.min(8);
                self.apu.tick_cpu_cycles(k);
                self.nsf2_timer.tick(k);
                self.cycles = self.cycles.wrapping_add(k as u64);
                rem -= k;
            }
        }
    }

    /// Pick up DMC events that fired on the cycle just ticked: a
    /// newly armed fetch becomes a scheduled load/reload DMA, and an
    /// implicit sample end becomes the apu-dma-wiki §Bugs
    /// aborted-/unexpected-DMA outcome (NTSC-class CPUs only — "It is
    /// not known whether 2A07 CPUs are affected by these bugs").
    fn observe_dmc_events(&mut self) {
        let e = self.cycles.wrapping_sub(1); // the cycle just ticked
        if let Some((addr, is_load)) = self.apu.dmc_take_fetch() {
            let (kind, halt_from) = if is_load {
                let hf = self
                    .dmc_load_halt_from
                    .take()
                    .unwrap_or_else(|| first_get(e + 3));
                (DmcDmaKind::Load, hf.max(first_get(e)))
            } else {
                // Reloads "are scheduled to halt the CPU on a put
                // cycle" — the first put after the buffer emptied.
                (DmcDmaKind::Reload, first_put(e + 1))
            };
            self.dmc_dma = Some(ScheduledDmcDma {
                addr,
                kind,
                halt_from,
            });
        }
        if self.apu.dmc_take_implicit_stop() && !self.apu.is_pal() && self.dmc_dma.is_none() {
            self.dmc_dma = Some(if e & 1 == 0 {
                // Stop on the same APU cycle the reload would schedule
                // ("the 1st CPU cycle before the halt attempt"): the
                // RP2A03H unexpected DMA runs "from the same address".
                ScheduledDmcDma {
                    addr: self.apu.dmc_last_fetch_addr(),
                    kind: DmcDmaKind::Unexpected,
                    halt_from: e + 1,
                }
            } else {
                // Stop during the APU cycle before the schedule: the
                // DMA starts but "is aborted after a single cycle".
                ScheduledDmcDma {
                    addr: 0,
                    kind: DmcDmaKind::Aborted,
                    halt_from: e + 2,
                }
            });
        }
    }

    /// Drain the CPU cycles stolen by DMA (DMC sample-byte fetches +
    /// `$4014` OAM DMA) since the last call.
    /// [`crate::cpu::Cpu6502::step`] folds them into the executing
    /// instruction's cycle count so the caller's scheduling (PLAY
    /// cadence, samples-per-cycle budget) sees the stall.
    pub fn take_dma_stall(&mut self) -> u32 {
        std::mem::take(&mut self.pending_dma_stall)
    }

    /// Renamed: the accumulator covers OAM DMA too now.
    #[deprecated(note = "renamed to take_dma_stall (now also covers $4014 OAM DMA)")]
    pub fn take_dmc_stall(&mut self) -> u32 {
        self.take_dma_stall()
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
    fn reset_for_tune_scrubs_ram_and_reinits_registers() {
        // §"Initializing a tune": RAM $0000-$07FF + $6000-$7FFF are
        // cleared, $4015 gets $00 then $0F, $4017 gets $40.
        let mut bus = NesBus::new();
        let h = fake_header(0x8000, vec![0x60], [0u8; 8]);
        bus.configure_from_header(&h);
        // Dirty machine state as a previous song would leave it.
        bus.write(0x0123, 0xAB);
        bus.write(0x6123, 0xCD);
        bus.write(0x4015, 0x00); // channels all disabled
        bus.write(0x4017, 0x00); // 4-step, IRQ inhibit CLEAR
        bus.reset_for_tune(&h);
        assert_eq!(bus.read(0x0123), 0, "zero page/RAM cleared");
        assert_eq!(bus.read(0x6123), 0, "cart RAM cleared");
        // $4015 = $0F: a length-counter load must stick (loads only
        // reach the counter while the channel is enabled).
        bus.write(0x4003, 0x08);
        assert_eq!(
            bus.read(0x4015) & 0x01,
            0x01,
            "$4015=$0F must have re-enabled pulse 1"
        );
        // $4017 = $40 (IRQ inhibit set): a full 4-step pass must not
        // latch the frame IRQ flag.
        bus.tick_cycles(35_000);
        assert_eq!(
            bus.read(0x4015) & 0x40,
            0,
            "$4017=$40 must inhibit the frame IRQ"
        );
    }

    #[test]
    fn reset_for_tune_reloads_low_load_program_after_ram_clear() {
        // A non-bankswitched tune loaded below $8000 lives in the
        // $6000-$7FFF RAM the scrub clears; the documented sequence
        // clears RAM before placing the tune data, so the program
        // bytes must survive a reset (reloaded, not preserved).
        let mut bus = NesBus::new();
        let h = fake_header(0x6000, vec![0xAA, 0xBB], [0u8; 8]);
        bus.configure_from_header(&h);
        assert_eq!(bus.read(0x6000), 0xAA);
        bus.write(0x6000, 0x77); // previous song self-modifies
        bus.reset_for_tune(&h);
        assert_eq!(bus.read(0x6000), 0xAA, "program byte reloaded");
        assert_eq!(bus.read(0x6001), 0xBB);
        assert_eq!(bus.read(0x6002), 0x00, "rest of cart RAM cleared");
    }

    #[test]
    fn reset_for_tune_restores_header_bank_selection() {
        // "If the tune is bank switched, load the bank values from
        // $070-$077 into $5FF8-$5FFF."
        let mut bus = NesBus::new();
        // Two banks: bank 0 starts 0x11.., bank 1 starts 0x22...
        let mut prog = vec![0x11; BANK_SIZE];
        prog.extend(vec![0x22; BANK_SIZE]);
        let mut banks = [0u8; 8];
        banks[0] = 1; // $8000 window starts on bank 1
        let h = fake_header(0x8000, prog, banks);
        bus.configure_from_header(&h);
        assert_eq!(bus.read(0x8000), 0x22);
        bus.write(0x5FF8, 0x00); // song rebanks the window
        assert_eq!(bus.read(0x8000), 0x11);
        bus.reset_for_tune(&h);
        assert_eq!(
            bus.read(0x8000),
            0x22,
            "bank selection must return to the header's init values"
        );
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
    fn dmc_dma_fetch_stalls_are_accounted() {
        // §"Memory reader" of the DMC doc: every sample-byte fetch
        // stalls the CPU. The bus accrues the per-DMA-type stall (a
        // 3-cycle load DMA for the post-$4015 fetch, 4-cycle reload
        // DMAs after — apu-dma-wiki §"DMC DMA"), keeps the APU running
        // through the stall, and drains the total via take_dma_stall().
        let mut bus = NesBus::new();
        bus.write(0x4010, 0x4F); // loop flag, fastest rate (54 cy/bit)
        bus.write(0x4012, 0x00); // sample address $C000
        bus.write(0x4013, 0x00); // 1-byte sample
        bus.write(0x4015, 0x10); // enable DMC → arms the first fetch
        assert_eq!(bus.take_dma_stall(), 0, "no fetch before ticking");
        // §"DMC DMA": the load DMA halts "on a get cycle during the
        // 2nd APU cycle after the write (that is, the 3rd or 4th CPU
        // cycle)" — the write landed on cycle 0 (get), so the halt is
        // at cycle 4, not immediately.
        bus.tick_cycles(1);
        assert_eq!(bus.take_dma_stall(), 0, "no halt before its schedule");
        bus.tick_cycles(6);
        assert_eq!(
            bus.take_dma_stall(),
            crate::apu::DMC_DMA_LOAD_STALL_CYCLES,
            "the post-$4015 fetch is a 3-cycle load DMA"
        );
        assert_eq!(bus.take_dma_stall(), 0, "stall drains on take");
        // A looping 1-byte sample re-fetches once per 8-bit output
        // cycle (8 × 54 CPU cycles at rate $F); each refetch is a
        // 4-cycle reload DMA. Four output cycles' worth of ticking
        // must accrue at least three more fetch stalls, all reloads.
        bus.tick_cycles(54 * 8 * 4);
        let stall = bus.take_dma_stall();
        assert!(
            stall >= 3 * crate::apu::DMC_DMA_RELOAD_STALL_CYCLES,
            "looping sample must keep accruing reload stalls (got {stall})"
        );
        assert_eq!(
            stall % crate::apu::DMC_DMA_RELOAD_STALL_CYCLES,
            0,
            "every buffer-emptied refetch is a reload DMA"
        );
    }

    #[test]
    fn oam_dma_stall_is_513_or_514_by_write_parity() {
        // apu-dma-wiki §"OAM DMA": "All together, OAM DMA on its own
        // takes 513 or 514 cycles, depending on whether alignment is
        // needed" — no alignment for a get-half write, one alignment
        // cycle for a put-half write.
        let mut bus = NesBus::new();
        // Fresh bus: cycle 0 = get half of the first APU cycle. The
        // write occupies cycle 0; the halt lands on cycle 1 — a put —
        // so the first read starts on the next get with no alignment.
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES,
            "get-half $4014 write needs no alignment cycle"
        );
        // Move to a put-half cycle: the halt then lands on a get and
        // the doc's alignment cycle is spent before the first read.
        bus.tick_cycles(1);
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES + 1,
            "put-half $4014 write spends an alignment cycle"
        );
    }

    #[test]
    fn oam_dma_keeps_the_apu_running_and_overlaps_dmc_fetches() {
        // apu-dma-wiki §"DMC DMA during OAM DMA": "In the common case,
        // DMC DMA occurring during OAM DMA will cost only 2 cycles"
        // instead of its usual 4-cycle reload. A looping 1-byte sample
        // at the fastest rate empties its buffer once per 8 × 54 = 432
        // CPU cycles, so exactly one refetch lands inside the ~513-
        // cycle OAM window and stretches it by the 2-cycle overlap.
        let mut bus = NesBus::new();
        bus.write(0x4010, 0x4F); // loop flag, fastest rate (54 cy/bit)
        bus.write(0x4012, 0x00);
        bus.write(0x4013, 0x00); // 1-byte sample
        bus.write(0x4015, 0x10); // enable DMC
        bus.tick_cycles(7); // past the cycle-4 load-DMA halt
        assert_eq!(bus.take_dma_stall(), crate::apu::DMC_DMA_LOAD_STALL_CYCLES);
        // Clock now at 7 + 3 = 10 CPU cycles (a get half).
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES + DMC_DMA_DURING_OAM_STALL_CYCLES,
            "one in-window DMC refetch costs the 2-cycle overlap"
        );
        assert!(
            bus.apu.dmc_pending_fetch().is_none(),
            "the in-window refetch must have been serviced"
        );
    }

    // ------- Sub-instruction DMA timing (apu-dma-wiki §Behavior) -------

    /// Inject a scheduled reload DMA directly (unit-testing the halt
    /// placement without arranging the DMC timer phase).
    fn inject_reload(bus: &mut NesBus, halt_from: u64) {
        bus.dmc_dma = Some(ScheduledDmcDma {
            addr: 0xC000,
            kind: DmcDmaKind::Reload,
            halt_from,
        });
    }

    #[test]
    fn reload_halt_on_put_costs_four_on_get_costs_three() {
        // §"DMC DMA": "load DMAs take 3 cycles and reload DMAs take 4
        // unless the halt is delayed by an odd number of cycles."
        // Undelayed reload: halt on its scheduled put → 4 cycles.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 1);
        bus.tick_cycles(3);
        assert_eq!(bus.take_dma_stall(), 4, "put-cycle halt: 4-cycle reload");
        // Delayed by one write cycle: §Behavior "DMA can only halt on
        // CPU read cycles" — the halt slips to the following get and
        // the alignment cycle is saved.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 1);
        bus.begin_instruction();
        // 3-cycle instruction whose cycle 1 (the scheduled put) writes.
        bus.run_instruction(3, 0b010);
        assert_eq!(
            bus.take_dma_stall(),
            3,
            "halt delayed 1 cycle onto a get: the reload costs 3"
        );
    }

    #[test]
    fn reload_halt_delayed_two_cycles_stays_at_four() {
        // Two consecutive write cycles (the RMW shape): the delay is
        // even, parity is preserved, and the reload still costs 4.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 1);
        bus.begin_instruction();
        bus.run_instruction(4, 0b0110); // writes on cycles 1-2
        assert_eq!(bus.take_dma_stall(), 4, "even delay keeps the put halt");
    }

    #[test]
    fn load_dma_delayed_by_write_cycle_costs_four() {
        // §"DMC DMA": a load DMA normally halts on its scheduled get
        // (3 cycles); a write cycle there delays it onto a put and the
        // alignment cycle brings it to 4.
        let mut bus = NesBus::new();
        bus.write(0x4010, 0x0F);
        bus.write(0x4012, 0x00);
        bus.write(0x4013, 0x00);
        bus.write(0x4015, 0x10); // cycle-0 write → halt scheduled at 4
        bus.tick_cycles(4); // cycles 0-3: before the halt
        assert_eq!(bus.take_dma_stall(), 0);
        bus.begin_instruction();
        bus.run_instruction(3, 0b001); // cycle 4 is a write cycle
        assert_eq!(
            bus.take_dma_stall(),
            4,
            "get-halt delayed onto a put: load DMA costs 4"
        );
    }

    #[test]
    fn oam_halt_delayed_past_rmw_second_write() {
        // §"OAM DMA": "read-modify-write instructions such as INC
        // $4014 […] are able to perform a second write before the CPU
        // can be halted" — the halt slips past the instruction into
        // the next one's first (read) cycle.
        let mut bus = NesBus::new();
        bus.begin_instruction();
        bus.write(0x4014, 0x02); // the RMW's (first) write
        bus.run_instruction(6, 0b110000); // INC abs: writes on cycles 4-5
        assert_eq!(
            bus.take_dma_stall(),
            0,
            "no room for the halt inside the RMW instruction"
        );
        // Next CPU cycle (6 — a get) takes the halt; get-half halts
        // spend the alignment cycle (514 total).
        bus.tick_cycles(1);
        assert_eq!(bus.take_dma_stall(), OAM_DMA_BASE_STALL_CYCLES + 1);
    }

    #[test]
    fn dmc_during_oam_end_of_window_costs_one_or_three() {
        // §"DMC DMA during OAM DMA": "if DMC DMA occurs at the end of
        // OAM DMA, it can take 1 or 3 cycles" instead of the common 2.
        // Window for a cycle-0 (get) $4014 write: halt at 1 (put),
        // transfers on cycles 2..=513, last put = 513.
        // DMC halt on the second-to-last put (511): +1.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 511);
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES + 1,
            "second-to-last-put collision costs 1 extra cycle"
        );
        // DMC halt on the last put (513): +3.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 513);
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES + 3,
            "last-put collision costs 3 extra cycles"
        );
        // Mid-window control: the common 2-cycle overlap.
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 101);
        bus.write(0x4014, 0x02);
        assert_eq!(
            bus.take_dma_stall(),
            OAM_DMA_BASE_STALL_CYCLES + DMC_DMA_DURING_OAM_STALL_CYCLES,
            "mid-window collision costs the common 2 cycles"
        );
    }

    #[test]
    fn deferred_4017_write_keys_reset_delay_off_the_write_cycle() {
        // apu-frame-counter §"Side effects": the $4017 sequence reset
        // lands "after 3 or 4 CPU clock cycles" measured from the
        // WRITE cycle's CPU/APU phase. The write is a store's final
        // cycle, so shifting the store by a 3-cycle instruction moves
        // the write cycle by 3 and — because the delay flips 3↔4 with
        // the parity — the reset (and the whole 4-step IRQ schedule
        // behind it) by exactly 4 CPU cycles.
        fn first_frame_irq_cycle(with_prefix: bool) -> u64 {
            let mut bus = NesBus::new();
            let mut cpu = crate::cpu::Cpu6502::new();
            let mut prog: Vec<u8> = vec![0xA9, 0x00]; // LDA #$00
            if with_prefix {
                prog.extend([0xA5, 0x00]); // LDA $00 (3 cycles)
            }
            prog.extend([0x8D, 0x17, 0x40]); // STA $4017 (4-step, IRQ on)
            bus.ram[0x0200..0x0200 + prog.len()].copy_from_slice(&prog);
            cpu.pc = 0x0200;
            cpu.p = 0x24;
            let steps = if with_prefix { 3 } else { 2 };
            for _ in 0..steps {
                cpu.step(&mut bus);
            }
            for _ in 0..40_000u32 {
                if bus.irq_line() {
                    return bus.cycles;
                }
                bus.tick_cycles(1);
            }
            panic!("frame IRQ never asserted");
        }
        let base = first_frame_irq_cycle(false);
        let shifted = first_frame_irq_cycle(true);
        assert_eq!(
            shifted - base,
            4,
            "3-cycle-later write cycle flips the 3/4 delay: schedule moves by 4"
        );
    }

    // --------------- apu-dma-wiki §Bugs stop timing ---------------

    #[test]
    fn explicit_stop_in_apu_cycle_before_schedule_aborts_after_one_cycle() {
        // "When sample playback is stopped during the APU cycle before
        // a reload DMA would schedule […] the DMA starts, but is
        // aborted after a single cycle."
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 3); // halt attempt on the APU-1 put
        bus.write(0x4015, 0x00); // stop at cycle 0 — APU cycle 0
        bus.tick_cycles(5);
        assert_eq!(bus.take_dma_stall(), 1, "aborted DMA steals its halt cycle");
        assert!(
            !bus.apu.dmc_activity_possible(),
            "the aborted DMA never performs its read"
        );
    }

    #[test]
    fn explicit_stop_earlier_cancels_without_any_dma() {
        // A stop before the APU cycle preceding the schedule point
        // yields no DMA at all — the memory reader only fetches while
        // "bytes remaining is not zero".
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 5); // halt attempt on the APU-2 put
        bus.write(0x4015, 0x00); // stop at cycle 0 — two APU cycles early
        bus.tick_cycles(8);
        assert_eq!(bus.take_dma_stall(), 0, "stop-early: no DMA, no stall");
    }

    #[test]
    fn aborted_dma_skipped_when_halt_attempt_lands_on_write_cycle() {
        // "If the halt is delayed due to a write cycle, the aborted
        // DMA doesn't occur at all."
        let mut bus = NesBus::new();
        inject_reload(&mut bus, 3);
        bus.write(0x4015, 0x00); // abort window: DMA becomes a 1-cycle stub
        bus.begin_instruction();
        bus.run_instruction(5, 0b01000); // cycle 3 (the halt attempt) writes
        assert_eq!(bus.take_dma_stall(), 0, "write-delayed abort vanishes");
    }

    #[test]
    fn implicit_stop_triggers_unexpected_reload_from_same_address() {
        // §Bugs: "when playback is stopped implicitly on the same APU
        // cycle that a reload DMA would schedule […] an unexpected
        // reload DMA occurs from the same address. This extra byte
        // goes into the sample buffer". With this machine's pinned
        // power-up alignment the DMC output-cycle boundary always
        // lands on a get, selecting exactly this arm of the bug.
        let mut bus = NesBus::new();
        // A 1-byte non-looping sample at the fastest rate, enabled at
        // cycle 0: the load DMA halts at cycle 4, and the 8th
        // output-unit shift (cycle 7 × 54 = 378) moves the final byte
        // into the shift register — the implicit stop. The unexpected
        // DMA halts on the very next cycle (379, put).
        bus.write(0x4010, 0x0F);
        bus.write(0x4012, 0x00);
        bus.write(0x4013, 0x00);
        bus.write(0x4015, 0x10);
        bus.tick_cycles(385);
        assert_eq!(
            bus.take_dma_stall(),
            3 + 4,
            "3-cycle load + 4-cycle unexpected reload"
        );
        assert!(
            bus.apu.dmc_activity_possible(),
            "the unexpected byte sits in the sample buffer"
        );
    }

    #[test]
    fn implicit_stop_bugs_gated_off_on_pal() {
        // "It is not known whether 2A07 CPUs are affected by these
        // bugs" — the PAL machine skips them.
        let mut bus = NesBus::new();
        bus.apu.set_cpu_hz(1_662_607);
        bus.write(0x4010, 0x0F);
        bus.write(0x4012, 0x00);
        bus.write(0x4013, 0x00);
        bus.write(0x4015, 0x10);
        bus.tick_cycles(500);
        assert_eq!(
            bus.take_dma_stall(),
            3,
            "PAL: only the load DMA, no unexpected reload"
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
