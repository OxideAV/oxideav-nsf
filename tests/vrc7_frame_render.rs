//! End-to-end VRC7 (OPLL / YM2413-derived) frame-render integration test.
//!
//! The OPLL operator core (`oxideav_nsf::opll`) and its register-level
//! VRC7 wrapper (`oxideav_nsf::expansion::Vrc7`) are exercised at the
//! unit level inside `src/`. This file drives the chip the way an NSF
//! player would — through the `$9010` (select) / `$9030` (data) write
//! ports — programs a single melody note, renders a full ~1/60 s
//! "frame" of operator samples, and verifies the produced PCM:
//!
//!   1. is *audible* (non-trivial peak amplitude after the attack), and
//!   2. oscillates at the *doc-predicted fundamental frequency*.
//!
//! The fundamental is computed directly from the silicon-measured §9
//! phase-step formula in
//! `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md`:
//!
//! ```text
//!   phase_step = ((fnum * mlTab[ML]) << block) >> 1     (VIB off)
//!   f_out      = phase_step / (1024 << 9) * 49716 Hz
//! ```
//!
//! where the phase counter is 10.9 fixed-point (9 fractional bits), so
//! one sine period spans `1024 << 9 = 524288` accumulator units
//! (`ym2413-phase-counter-andete-2015-03-16.txt` §"conclusion"), with
//! `mlTab = MUL_TIMES_TWO` (§3 MUL doubled) and the sample rate
//! `3.579545 MHz / 72 = 49716 Hz` (§8). The carrier of a key-on'd
//! channel runs at this fundamental (modulation only adds harmonics, it
//! does not move the fundamental), so a zero-crossing count over a known
//! span recovers `f_out` to within the counting resolution.

use oxideav_nsf::expansion::Vrc7;
use oxideav_nsf::opll::{
    MUL_TIMES_TWO, OPLL_SAMPLE_RATE_HZ, PHASE_ACC_FRAC_BITS, PHASE_STEPS_PER_PERIOD,
};

/// CPU cycles per OPLL operator sample: NES master clock 1.789773 MHz
/// over the 49716 Hz operator rate ≈ 35.9956. We tick one operator
/// sample at a time by feeding ~36 cycles and reading the freshly
/// latched output; the chip's own Q8 accumulator keeps the cadence
/// exact across the run, so 36 cycles/sample over a frame averages to
/// the true rate.
fn op_cycles() -> u32 {
    (1_789_773.0_f64 / OPLL_SAMPLE_RATE_HZ as f64).round() as u32
}

/// Write `value` into VRC7 internal register `r` via the documented
/// `$9010` select / `$9030` data ports (vrc7-audio-wiki §"Registers").
fn write_reg(chip: &mut Vrc7, r: u8, value: u8) {
    chip.write(0x9010, r);
    chip.write(0x9030, value);
}

/// Render `n` operator samples, returning the per-sample carrier-bus
/// output (the chip's normalised f32 mix). One sample == one `output()`
/// latch advance.
fn render(chip: &mut Vrc7, n: usize) -> Vec<f32> {
    let step = op_cycles();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        chip.tick(step);
        out.push(chip.output());
    }
    out
}

/// Predicted carrier fundamental (Hz) for a key-on'd channel from the
/// §9 phase-step formula, VIB disabled.
fn predicted_hz(fnum: u32, block: u32, mul: u8) -> f64 {
    let phase_step = ((fnum * MUL_TIMES_TWO[mul as usize & 0x0F] as u32) << block) >> 1;
    let modulus = (PHASE_STEPS_PER_PERIOD as u64) << PHASE_ACC_FRAC_BITS;
    phase_step as f64 / modulus as f64 * OPLL_SAMPLE_RATE_HZ as f64
}

/// Count upward zero-crossings (sign changes − to +) in a sample slice,
/// the simplest fundamental-frequency estimator for a quasi-periodic
/// signal centred near zero.
fn upward_crossings(samples: &[f32]) -> usize {
    let mut count = 0;
    let mut prev = 0.0f32;
    for &s in samples {
        if prev <= 0.0 && s > 0.0 {
            count += 1;
        }
        prev = s;
    }
    count
}

/// Program channel 0 with a custom (slot-0) patch tuned for a clean,
/// carrier-dominant tone: a strong carrier sine with minimal modulation
/// so the fundamental is unambiguous. Returns nothing; mutates `chip`.
///
/// Custom patch byte layout (vrc7-audio §"Custom Patch", mirrored in
/// `docs/audio/nsf/opll-ym2413/opll-ym2413-tables.md` §2):
///   $00 mod: AM VIB EGT KSR MUL  — MUL=1, all flags off
///   $01 car: AM VIB EGT KSR MUL  — MUL=1, sustained (EGT) so the tone
///                                  holds at full level through the frame
///   $02 KSL|TL — modulator total level high (≈ silence the modulator so
///                the carrier is a near-pure sine: TL is 6-bit, 0x3F max)
///   $03 KSL|DC|DM|FB — feedback 0
///   $04 mod AR|DR  $05 car AR|DR — fast attack, no decay
///   $06 mod SL|RR  $07 car SL|RR — no sustain decay, no release
fn program_pure_carrier(chip: &mut Vrc7, fnum: u16, block: u8, mul: u8) {
    // Custom patch (slot 0).
    write_reg(chip, 0x00, mul & 0x0F); // mod: MUL only
    write_reg(chip, 0x01, 0x20 | (mul & 0x0F)); // car: EGT(sustained)=1, MUL
    write_reg(chip, 0x02, 0x3F); // mod TL = max attenuation → quiet modulator
    write_reg(chip, 0x03, 0x00); // FB=0, no waveform distortion
    write_reg(chip, 0x04, 0xF0); // mod AR=F (instant), DR=0
    write_reg(chip, 0x05, 0xF0); // car AR=F (instant), DR=0
    write_reg(chip, 0x06, 0x00); // mod SL=0, RR=0
                                 // car SL=0, RR=F (fast release): the EGT-sustained carrier holds at
                                 // full level until key-off, then this release rate quiets it within
                                 // the rendered window.
    write_reg(chip, 0x07, 0x0F);

    // Channel 0: select patch 0 (custom), volume 0 (loudest). The
    // engine reloads the live OPLL channel only on a $3X instrument /
    // volume *change*, and a fresh chip already sits at instrument 0 /
    // volume 0 — so write a throwaway non-zero volume first to force the
    // reload, then the real value. This mirrors how an NSF program
    // programs the custom-patch bytes ($00-$07) *before* re-selecting
    // the instrument via $3X.
    write_reg(chip, 0x30, 0x01);
    write_reg(chip, 0x30, 0x00);
    // F-Number low byte.
    write_reg(chip, 0x10, (fnum & 0xFF) as u8);
    // $20: --S T BBB H  (S=0 sustain off, T=1 key-on, BBB=block, H=fnum bit8)
    let fnum_hi = ((fnum >> 8) & 0x01) as u8;
    let reg20 = 0x10 | ((block & 0x07) << 1) | fnum_hi;
    write_reg(chip, 0x20, reg20);
}

#[test]
fn vrc7_renders_audible_frame_through_register_ports() {
    let mut chip = Vrc7::new();
    chip.enabled = true;

    // A4-ish pitch: fnum=0x180, block=4, MUL=1.
    let (fnum, block, mul) = (0x180u16, 4u8, 1u8);
    program_pure_carrier(&mut chip, fnum, block, mul);

    // Render a full NTSC frame (~1/60 s ≈ 829 operator samples) after a
    // short settle so the attack has reached full level.
    let _settle = render(&mut chip, 256);
    let frame = render(&mut chip, (OPLL_SAMPLE_RATE_HZ as usize) / 60);

    let peak = frame.iter().cloned().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        peak > 0.01,
        "key-on'd VRC7 channel must render audible PCM, peak={peak}"
    );

    // The signal must actually swing (not a stuck DC level): both a
    // positive and a negative excursion within the frame.
    let max = frame.iter().cloned().fold(f32::MIN, f32::max);
    let min = frame.iter().cloned().fold(f32::MAX, f32::min);
    assert!(
        max > 0.0 && min < 0.0,
        "carrier sine must cross zero: min={min} max={max}"
    );
}

#[test]
fn vrc7_rendered_frame_matches_doc_predicted_fundamental() {
    let mut chip = Vrc7::new();
    chip.enabled = true;

    let (fnum, block, mul) = (0x180u16, 4u8, 1u8);
    program_pure_carrier(&mut chip, fnum, block, mul);

    // Settle through the attack, then capture a long window so the
    // zero-crossing estimate is stable.
    let _settle = render(&mut chip, 512);
    let n = 8192usize;
    let frame = render(&mut chip, n);

    let predicted = predicted_hz(fnum as u32, block as u32, mul);
    // Upward zero-crossings over `n` samples → cycles; convert to Hz.
    let cycles = upward_crossings(&frame) as f64;
    let measured = cycles * OPLL_SAMPLE_RATE_HZ as f64 / n as f64;

    // Zero-crossing resolution over an 8192-sample window is ≈6 Hz; a
    // ~700 Hz carrier should land well within 5% of the doc formula.
    let rel_err = (measured - predicted).abs() / predicted;
    assert!(
        rel_err < 0.05,
        "rendered fundamental {measured:.1} Hz must match §9 prediction \
         {predicted:.1} Hz (rel_err={rel_err:.4})"
    );
}

#[test]
fn vrc7_higher_block_renders_higher_pitch() {
    // Doubling the block doubles the phase-step → one octave up. Render
    // two notes an octave apart and confirm the measured crossing count
    // roughly doubles, end-to-end through the register ports.
    fn cycles_for(block: u8) -> usize {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        let (fnum, mul) = (0x180u16, 1u8);
        program_pure_carrier(&mut chip, fnum, block, mul);
        let _settle = render(&mut chip, 512);
        let frame = render(&mut chip, 8192);
        upward_crossings(&frame)
    }

    let low = cycles_for(3);
    let high = cycles_for(4);
    assert!(low > 0 && high > 0, "both notes must oscillate");
    let ratio = high as f64 / low as f64;
    assert!(
        (ratio - 2.0).abs() < 0.25,
        "block+1 must raise pitch ~1 octave: low={low} high={high} ratio={ratio:.3}"
    );
}

/// Program channel 0 with a custom patch whose carrier has a **slow,
/// measurable decay** toward a near-silent sustain level, so the
/// rendered envelope visibly ramps down over the frame. This exercises
/// the §7 global-counter decay path end-to-end through the register
/// ports (instant attack → slow decay → low sustain), rather than the
/// flat sustained tone used by `program_pure_carrier`.
fn program_decaying_carrier(chip: &mut Vrc7, fnum: u16, block: u8, mul: u8, dr: u8, sl: u8) {
    write_reg(chip, 0x00, mul & 0x0F); // mod: MUL only
    write_reg(chip, 0x01, 0x20 | (mul & 0x0F)); // car: EGT(sustained)=1, MUL
    write_reg(chip, 0x02, 0x3F); // mod TL = max → quiet modulator
    write_reg(chip, 0x03, 0x00); // FB=0
    write_reg(chip, 0x04, 0xF0); // mod AR=F, DR=0
    write_reg(chip, 0x05, 0xF0 | (dr & 0x0F)); // car AR=F (instant), DR=dr
    write_reg(chip, 0x06, 0x00); // mod SL=0, RR=0
    write_reg(chip, 0x07, (sl & 0x0F) << 4); // car SL=sl, RR=0
    write_reg(chip, 0x30, 0x01);
    write_reg(chip, 0x30, 0x00);
    write_reg(chip, 0x10, (fnum & 0xFF) as u8);
    let fnum_hi = ((fnum >> 8) & 0x01) as u8;
    let reg20 = 0x10 | ((block & 0x07) << 1) | fnum_hi;
    write_reg(chip, 0x20, reg20);
}

/// Peak absolute amplitude of a rendered window.
fn peak(samples: &[f32]) -> f32 {
    samples.iter().cloned().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// End-to-end §7 decay: a key-on'd channel with a slow carrier decay
/// rate and a quiet sustain level must render a tone whose amplitude
/// **ramps down** over successive windows (instant attack to full level,
/// then the §7 global-counter decay advance reduces the carrier's output
/// each EG step). This proves the §7 model — not a flat sustained level
/// — drives the audible decay through the full register-port pipeline.
#[test]
fn vrc7_rendered_decay_ramps_down_per_section_7_model() {
    let mut chip = Vrc7::new();
    chip.enabled = true;

    // Slow-ish decay (DR=4) toward a near-silent sustain (SL=13), so the
    // carrier falls steadily across the rendered window. fnum/block give
    // a clean mid tone; MUL=1.
    program_decaying_carrier(&mut chip, 0x180, 4, 1, 4, 13);

    // Let the instant attack reach full level, capture the early peak.
    let early = render(&mut chip, 1024);
    let early_peak = peak(&early);
    assert!(
        early_peak > 0.01,
        "attack must reach an audible level first: {early_peak}"
    );

    // Render several later windows; with a live §7 decay the peak must
    // fall monotonically (within crossing/quantisation noise) and end
    // well below the post-attack peak.
    let mut peaks = vec![early_peak];
    for _ in 0..6 {
        let w = render(&mut chip, 4096);
        peaks.push(peak(&w));
    }
    let last = *peaks.last().unwrap();
    assert!(
        last < early_peak * 0.7,
        "§7 decay must reduce the carrier amplitude over time: \
         early={early_peak} late={last} (peaks={peaks:?})"
    );
    // Non-increasing trend (allow tiny per-window jitter from the
    // zero-crossing-relative peak sampling).
    for pair in peaks.windows(2) {
        assert!(
            pair[1] <= pair[0] + 0.02,
            "decay must not get louder window-to-window: {pair:?}"
        );
    }
}

/// End-to-end §7 rate ordering: a **faster** carrier decay rate must
/// reach the quiet sustain floor sooner than a slower one, observable in
/// the rendered PCM. This confirms the §7 `eg_shift`/`eg_select` rate
/// mapping is wired into the audible path (not just a constant ramp).
#[test]
fn vrc7_faster_decay_rate_quiets_sooner() {
    fn amplitude_after(dr: u8, samples_in: usize) -> f32 {
        let mut chip = Vrc7::new();
        chip.enabled = true;
        program_decaying_carrier(&mut chip, 0x180, 4, 1, dr, 13);
        let _settle = render(&mut chip, 256); // reach full level
        let _mid = render(&mut chip, samples_in);
        peak(&render(&mut chip, 2048))
    }

    // After the same elapsed render span, a faster decay (DR=8) must be
    // quieter than a slow decay (DR=3).
    let span = 6000;
    let slow = amplitude_after(3, span);
    let fast = amplitude_after(8, span);
    assert!(
        fast < slow,
        "faster decay rate must be quieter at the same time: slow(DR=3)={slow} fast(DR=8)={fast}"
    );
}

#[test]
fn vrc7_key_off_silences_the_rendered_frame() {
    let mut chip = Vrc7::new();
    chip.enabled = true;

    let (fnum, block, mul) = (0x180u16, 4u8, 1u8);
    program_pure_carrier(&mut chip, fnum, block, mul);
    let _settle = render(&mut chip, 256);
    let on_peak = render(&mut chip, 1024)
        .iter()
        .cloned()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(on_peak > 0.01, "key-on frame must be audible: {on_peak}");

    // Key-off: clear the $20 key bit (T), keep block/fnum. The patch's
    // carrier RR=F (programmed in `program_pure_carrier`) makes the tail
    // die within the rendered window.
    let fnum_hi = ((fnum >> 8) & 0x01) as u8;
    let reg20 = ((block & 0x07) << 1) | fnum_hi; // T (key) bit 4 = 0
    write_reg(&mut chip, 0x20, reg20);

    // Render long enough for the fast release to reach silence.
    let off = render(&mut chip, 8192);
    let tail_peak = off[off.len() - 1024..]
        .iter()
        .cloned()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(
        tail_peak < on_peak * 0.25,
        "key-off + fast release must quiet the tail: on={on_peak} tail={tail_peak}"
    );
}
