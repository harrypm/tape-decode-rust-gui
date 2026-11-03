//! SECAM method 1 colour-under restoration. The recorder passed the studio
//! chroma block through a divide-by-4 counter, so the rest carriers on tape
//! are foB/4 and foR/4 with the FM deviation divided by four as well, and
//! the block is restored by multiplying the carrier phase back up rather
//! than by mixing against a heterodyne. Carrier and deviation scale
//! together, so tape timebase error self-corrects and there is no
//! conversion LO to servo. The divider output has constant amplitude, so
//! the BT.470 bell pre-emphasis is regenerated from the restored
//! instantaneous frequency.

use std::f64::consts::{PI, TAU};
use std::ops::{Range, RangeInclusive};

use rustfft::num_complex::Complex64;

use super::chroma::{analytic_signal, phase_step};
use super::*;

/// Subcarrier rest frequencies (ITU-R BT.470).
const FOR_HZ: f32 = 4_406_250.0;
const FOB_HZ: f32 = 4_250_000.0;
/// Largest legal deviation below foB and above foR (BT.470-6 table 2 item
/// 2.12): the per-component maxima (D'B -350/+506 kHz, D'R -506/+350 kHz)
/// mirror across the carrier pair, so both components share the one
/// corridor foB - 350 kHz to foR + 350 kHz.
const MAX_DEVIATION_HZ: f32 = 350e3;
/// Centre of the HF pre-emphasis bell.
const BELL_F0_HZ: f32 = 4_286_000.0;
/// Legal subcarrier excursion; the bell gain is looked up inside it so that
/// noise and carrier switch transients are not boosted by the bell skirts.
const CARRIER_RANGE_HZ: RangeInclusive<f32> = 3.9e6..=4.756e6;
/// Taps of the moving average over the instantaneous frequency, so that
/// sample-level phase noise does not end up as amplitude noise through the
/// bell (which is smooth enough not to be blunted by it).
const INST_FREQ_SMOOTHING: usize = 9;
/// Envelope ceiling relative to the field median.
const ENVELOPE_CAP: f32 = 1.25;
/// First line with chroma output.
const STARTING_LINE: usize = 16;
/// Lines needed to fit the line alternation, and the fraction of them that
/// must agree with the fit before the blanking is regenerated from it.
const IDENT_MIN_LINES: usize = 32;
const IDENT_MIN_CONFIDENCE: f32 = 0.7;

/// Relative subcarrier HF pre-emphasis (bell) gain at `freq_hz`, unity at
/// the bell centre: |1 + j16F| / |1 + j1.26F| with F = f/f0 - f0/f.
fn bell_gain(freq_hz: f32) -> f32 {
    let f = freq_hz / BELL_F0_HZ;
    let bell_f = f - 1.0 / f;
    ((1.0 + (16.0 * bell_f).powi(2)) / (1.0 + (1.26 * bell_f).powi(2))).sqrt()
}

/// Wrap to (-pi, pi].
fn wrap_phase(phase: f64) -> f64 {
    phase.sin().atan2(phase.cos())
}

/// One restoration pass over a field.
struct Upconverted {
    /// The restored chroma block.
    signal: Vec<f32>,
    /// Smoothed restored-domain instantaneous frequency in Hz the bell was
    /// shaped with; reused for the line identification.
    inst_freq: Vec<f32>,
    /// Band-passed under-carrier envelope.
    envelope: Vec<f32>,
}

/// Restore the chroma block from the colour-under `chroma` by multiplying
/// the under carrier's phase by `mult`, with the bell amplitude regenerated
/// from the restored instantaneous frequency. `rest_amplitude` is the peak
/// amplitude of the undeviated carrier.
fn upconvert(chroma: &[f32], spec: &DecoderSpec, mult: u32, rest_amplitude: f32) -> Upconverted {
    let under_filter = spec
        .secam_under_filter
        .as_ref()
        .expect("SECAM method 1 spec without its under band-pass");
    let fft = spec
        .secam_analytic_fft
        .as_ref()
        .expect("SECAM method 1 spec without its analytic transform");
    let filtered = sosfiltfilt_f32(under_filter, chroma);
    // Over the whole field, so short-window edge effects do not bias the
    // phase.
    let analytic = analytic_signal(&filtered, fft);
    let envelope: Vec<f32> = analytic
        .iter()
        .map(|z| z.re.mul_add(z.re, z.im * z.im).sqrt())
        .collect();
    let samp_rate = spec.tbc_sample_rate();

    // Under-carrier phase advance per sample, as the central difference of
    // the unwrapped phase (one-sided at the ends), clamped to the legal
    // corridor ahead of the multiplication: the studio limiter clipped the
    // colour difference to these bounds, so anything outside is noise or a
    // tape channel truncation transient, which the multiplication would
    // scale up and the downstream de-emphasis smear into streaks at
    // saturated transitions. Clipping the instantaneous frequency and
    // re-integrating keeps the carrier smooth, with the clipped spans as
    // constant-frequency stretches; the absolute phase is lost, which the FM
    // decoders downstream do not care about. The clamped steps are f64: the
    // corridor edge rounded to f32 would bias the thousands of clamped
    // samples of a field alike. The integral is kept reduced to a turn,
    // which every clamped step is well under.
    //
    // The restored-domain instantaneous frequency for the bell is the central
    // difference of the re-integrated phase, whose consecutive samples differ
    // by the clamped steps from the second one on. Both come out of one pass
    // over the analytic signal that carries the previous raw and clamped
    // steps, so no field-sized f64 buffer is needed.
    let step_per_hz = TAU / samp_rate;
    let corridor = f64::from(FOB_HZ - MAX_DEVIATION_HZ) / f64::from(mult) * step_per_hz
        ..=f64::from(FOR_HZ + MAX_DEVIATION_HZ) / f64::from(mult) * step_per_hz;
    let freq_scale = f64::from(mult) / step_per_hz;
    let n = analytic.len();
    assert!(n >= 2, "a field has more than one sample");
    let mut phase = Vec::with_capacity(n);
    let mut raw_freq = Vec::with_capacity(n);
    let mut integral = 0.0f64;
    let mut prev_raw = None;
    let mut prev_step = 0.0f64;
    for k in 0..n {
        let raw = analytic.get(k + 1).map(|&next| phase_step(analytic[k], next));
        let centered = match (prev_raw, raw) {
            (Some(before), Some(after)) => (before + after) / 2.0,
            (None, Some(after)) => after,
            (Some(before), None) => before,
            (None, None) => unreachable!(),
        };
        let step = f64::from(centered).clamp(*corridor.start(), *corridor.end());
        integral += f64::from(mult) * step;
        if integral >= TAU {
            integral -= TAU;
        }
        phase.push(integral as f32);
        match k {
            0 => {}
            1 => raw_freq.push((step * freq_scale) as f32),
            _ => raw_freq.push(((prev_step + step) / 2.0 * freq_scale) as f32),
        }
        prev_raw = raw;
        prev_step = step;
    }
    raw_freq.push((prev_step * freq_scale) as f32);
    let mut inst_freq = boxcar_same(&raw_freq, INST_FREQ_SMOOTHING);
    for freq in &mut inst_freq {
        *freq = freq.clamp(*CARRIER_RANGE_HZ.start(), *CARRIER_RANGE_HZ.end());
    }

    // Scale by the normalized envelope, capped just above nominal: where the
    // carrier is healthy this is about unity and the amplitude stays on the
    // bell; where it dips or disappears (dropouts, FM clicks, no colour) the
    // dip passes through to the output instead of being limited away, since
    // downstream SECAM decoders key their concealment off exactly those
    // collapses, and carrier-free noise does not become full-scale splatter.
    let env_median = median_radix(&envelope);
    let signal: Vec<f32> = envelope
        .iter()
        .zip(&inst_freq)
        .zip(&phase)
        .map(|((&amplitude, &freq), &phase)| {
            if env_median <= 0.0 {
                return 0.0;
            }
            let limited = (amplitude / env_median).min(ENVELOPE_CAP);
            rest_amplitude * bell_gain(freq) * limited * phase.cos()
        })
        .collect();
    Upconverted {
        signal,
        inst_freq,
        envelope,
    }
}

struct LineAlternation {
    dr_on_even: bool,
    /// Fraction of lines agreeing with the fit.
    confidence: f32,
}

/// Fit the field's D'R/D'B line alternation from each line's median restored
/// frequency over the active region: D'R lines sit in the top half of the
/// chroma block, D'B lines in the bottom. The sequence alternates strictly,
/// so the better of the two parities is fitted; a heavily saturated line can
/// land on the wrong side, the majority never does. `None` with too few
/// lines to fit.
fn fit_line_alternation(
    inst_freq: &[f32],
    outwidth: usize,
    first_line: usize,
    porch_end_px: usize,
) -> Option<LineAlternation> {
    let linesout = inst_freq.len() / outwidth;
    let n_lines = linesout
        .checked_sub(first_line)
        .filter(|&n| n >= IDENT_MIN_LINES)?;
    let active = porch_end_px + 30..outwidth - 40;
    let even_is_dr = (first_line..linesout)
        .filter(|&line| {
            let mut window = inst_freq[line * outwidth..][active.clone()].to_vec();
            let is_dr = median_from_values(&mut window) > (FOR_HZ + FOB_HZ) / 2.0;
            is_dr == (line % 2 == 0)
        })
        .count();
    let odd_is_dr = n_lines - even_is_dr;
    Some(LineAlternation {
        dr_on_even: even_is_dr >= odd_is_dr,
        confidence: even_is_dr.max(odd_is_dr) as f32 / n_lines as f32,
    })
}

/// Narrowband model of the colour-under carrier over a short window,
/// measured by correlation against a rotor at the line's rest frequency:
/// the two half-window correlations give the frequency offset from the
/// rotor, their sum the phase. Correlation projects out everything away
/// from the rotor, so it is usable on the raw chroma, where luma crosstalk
/// would bias a broadband measurement.
struct CarrierModel {
    freq: f64,
    /// Rotor phase advance per sample.
    rotor_step: f64,
    /// Phase advance per sample of the offset from the rotor.
    offset_step: f64,
    /// Phase at the window centre `t_mid`.
    phase_mid: f64,
    t_mid: f64,
}

impl CarrierModel {
    /// Carrier phase at absolute sample `t`.
    fn phase_at(&self, t: f64) -> f64 {
        self.rotor_step * t + self.phase_mid + self.offset_step * (t - self.t_mid)
    }
}

fn measure_carrier(
    chroma: &[f32],
    samp_rate: f64,
    start: usize,
    len: usize,
    f_rot: f64,
) -> Option<CarrierModel> {
    // Keep runaway estimates (no real carrier in the window) inside the
    // format's legal deviation.
    const MAX_OFFSET_HZ: f64 = 130e3;

    let window = chroma.get(start..start + len)?;
    let rotor_step = TAU * f_rot / samp_rate;
    let correlate = |samples: &[f32], t0: usize| -> Complex64 {
        // The rotor is walked from the window start rather than evaluated
        // per sample.
        let mut rotor = Complex64::from_polar(1.0, -rotor_step * t0 as f64);
        let advance = Complex64::from_polar(1.0, -rotor_step);
        let mut sum = Complex64::new(0.0, 0.0);
        for &x in samples {
            sum += rotor * f64::from(x);
            rotor *= advance;
        }
        sum
    };
    let half = len / 2;
    let z1 = correlate(&window[..half], start);
    let z2 = correlate(&window[half..], start + half);
    let zf = z1 + z2;
    if z1.norm() == 0.0 || z2.norm() == 0.0 || zf.norm() == 0.0 {
        return None;
    }
    let dphi = (z2 * z1.conj()).arg();
    let df = (dphi / (TAU * half as f64 / samp_rate)).clamp(-MAX_OFFSET_HZ, MAX_OFFSET_HZ);
    Some(CarrierModel {
        freq: f_rot + df,
        rotor_step,
        offset_step: TAU * df / samp_rate,
        phase_mid: zf.arg(),
        t_mid: start as f64 + (len - 1) as f64 / 2.0,
    })
}

/// Samples the synthesized carrier is faded in over, ahead of the splice.
const FADE_LEN: usize = 8;
/// Raised-cosine ramp from the measured outgoing carrier to its rest
/// frequency.
const RAMP_LEN: usize = 20;
/// Carrier measurement window on either side of the blanking interval.
const MEAS_LEN: usize = 32;
/// Rest-to-rest frequency step, in samples from the next line's start: over
/// the sync tip.
const STEP_PX: Range<usize> = 8..40;
/// Phase closure bump: the outgoing and incoming carriers are independent
/// oscillator segments, so the synthesis must absorb a uniformly random
/// phase difference of up to +-pi (times the multiplier after the
/// restoration, so a step is not an option anywhere near the picture or the
/// porch). It goes into a cosine-tapered flat-top frequency excursion across
/// the sync region, sized to the error but capped inside the under
/// band-pass (BUMP_MAX_HZ around either rest carrier stays within its pass
/// band), and finished early enough (BUMP_END_PX into the next line) that
/// the band-pass ring stays out of the porch reference window.
const BUMP_END_PX: usize = 88;
const BUMP_MAX_HZ: f64 = 170e3;
const BUMP_TAPER: usize = 12;

/// Raised cosine rising from 0 towards 1 over `len` samples.
fn raised_cosine(len: usize) -> Vec<f64> {
    (0..len)
        .map(|k| 0.5 - 0.5 * (PI * k as f64 / len as f64).cos())
        .collect()
}

/// Phase at each sample of a frequency profile integrated from `phase0`,
/// and the phase one sample past its end.
fn integrate_phase(profile: &[f64], phase0: f64, samp_rate: f64) -> (Vec<f64>, f64) {
    let mut phase = Vec::with_capacity(profile.len());
    let mut acc = phase0;
    for &freq in profile {
        phase.push(acc);
        acc += TAU * freq / samp_rate;
    }
    (phase, acc)
}

/// Layout of one synthesized blanking interval, in samples from the splice
/// start; every line shares it.
struct BlankingLayout {
    /// Splice start to the next line's porch end.
    span: usize,
    /// Rest-to-rest frequency step.
    step: Range<usize>,
    /// Phase closure bump.
    bump: Range<usize>,
    /// Rest frequency hold after the bump, taking the bump's spill.
    hold: Range<usize>,
}

impl BlankingLayout {
    /// `None` when the interval is too short to fit the synthesis.
    fn new(outwidth: usize, blank_start_px: usize, porch_end_px: usize) -> Option<Self> {
        let next_line = outwidth.checked_sub(blank_start_px)?;
        let span = next_line + porch_end_px;
        let step0 = (next_line + STEP_PX.start)
            .max(RAMP_LEN)
            .min(span.checked_sub(RAMP_LEN + 96)?);
        // The bump starts no earlier than just before the next line (its
        // band-pass ring must stay out of the outgoing picture).
        let bump0 = (RAMP_LEN + 4).max(next_line.saturating_sub(16));
        let bump1 = (next_line + BUMP_END_PX).min(span.checked_sub(RAMP_LEN + 4)?);
        let hold = bump1..span - RAMP_LEN;
        if bump1.checked_sub(bump0)? < 2 * BUMP_TAPER + 8 || hold.len() < 24 {
            return None;
        }
        Some(Self {
            span,
            step: step0..step0 + STEP_PX.len(),
            bump: bump0..bump1,
            hold,
        })
    }
}

/// Replace each line's blanking interval (front porch, sync and back porch
/// in one run, from `blank_start_px` to the next line's `porch_end_px`) with
/// a synthesized undeviated rest carrier, phase-continuous with the active
/// video on both sides. What comes off tape there is the record divider's
/// settling transient (blanking edges and subcarrier phase reversals upset
/// it), which the zero-phase filtering here and downstream would smear
/// backwards into the last microseconds of the picture (a magenta band down
/// the right edge) and into the back porch decoders calibrate their
/// discriminator zeros from (a colour cast). This runs in the colour-under
/// domain ahead of the second restoration pass, so the filters never see
/// the transient.
///
/// The synthesized frequency ramps from the measured outgoing carrier to the
/// outgoing line's rest frequency over the front porch, steps to the
/// incoming line's rest frequency over the sync tip and holds it right
/// through the back porch, which is the decoders' discriminator zero
/// reference, up to the incoming picture; being zero colour difference, it
/// decodes the incoming line's colour turn-on strip as neutral rather than
/// as a per-line click. The phase is the profile's integral, with the random
/// phase difference between the two lines' carriers closed by the bump. The
/// write starts `FADE_LEN` samples early, faded in over the phase-matched
/// outgoing carrier before the transient sets in; the incoming side gets no
/// fade, the phase-closed synthesis hands over to the raw signal at the
/// porch end (a fade over the porch tail mixes transient back in next to the
/// porch measurement window, one over the picture leaves a seam that rings
/// down the left edge).
fn regenerate_blanking(
    chroma: &[f32],
    envelope: &[f32],
    samp_rate: f64,
    outwidth: usize,
    layout: &BlankingLayout,
    blank_start_px: usize,
    first_line: usize,
    dr_on_even: bool,
    mult: u32,
) -> Vec<f32> {
    let linesout = chroma.len() / outwidth;
    let rest_hz = |is_dr: bool| f64::from(if is_dr { FOR_HZ } else { FOB_HZ }) / f64::from(mult);
    let fade = raised_cosine(FADE_LEN);
    let ramp = raised_cosine(RAMP_LEN);
    let step = raised_cosine(layout.step.len());
    let taper = raised_cosine(BUMP_TAPER);
    let q = FADE_LEN;
    let mut cleaned = chroma.to_vec();

    for line in first_line..linesout.saturating_sub(1) {
        let line_is_dr = (line % 2 == 0) == dr_on_even;
        let (f_out_rest, f_in_rest) = (rest_hz(line_is_dr), rest_hz(!line_is_dr));
        let start = line * outwidth + blank_start_px;
        let end = start + layout.span;
        if start < 2 * MEAS_LEN + q || end + 2 * MEAS_LEN > chroma.len() {
            continue;
        }
        // The windows never overlap an earlier line's rewrite, so they read
        // the original chroma.
        let outgoing = measure_carrier(chroma, samp_rate, start - MEAS_LEN, MEAS_LEN, f_out_rest);
        let incoming = measure_carrier(chroma, samp_rate, end, MEAS_LEN, f_in_rest);
        let (Some(outgoing), Some(incoming)) = (outgoing, incoming) else {
            continue;
        };
        // Amplitudes from the band-passed envelope, as the narrowband
        // correlation under-reads a deviating FM carrier and an amplitude
        // step at the splice reads as a click downstream; measured a little
        // away from the splice, where the envelope is still inflated by the
        // band-pass smear of the adjacent transient.
        let amp_out = f64::from(median_from_values(
            &mut envelope[start - 2 * MEAS_LEN..start - MEAS_LEN].to_vec(),
        ));
        let amp_in = f64::from(median_from_values(
            &mut envelope[end + MEAS_LEN..end + 2 * MEAS_LEN].to_vec(),
        ));

        // Frequency profile over the write region, `q` samples ahead of
        // `start` to `end`.
        let mut profile = vec![f_in_rest; q + layout.span];
        profile[..q].fill(outgoing.freq);
        for (freq, &r) in profile[q..q + RAMP_LEN].iter_mut().zip(&ramp) {
            *freq = outgoing.freq + (f_out_rest - outgoing.freq) * r;
        }
        profile[q + RAMP_LEN..q + layout.step.start].fill(f_out_rest);
        for (freq, &r) in profile[q + layout.step.start..q + layout.step.end]
            .iter_mut()
            .zip(&step)
        {
            *freq = f_out_rest + (f_in_rest - f_out_rest) * r;
        }

        // Close the phase against the incoming carrier: the bump's area is
        // the absorbed phase; whatever exceeds its cap (degenerately short
        // blanking only) becomes a constant offset across the hold, never
        // more than a few kHz.
        let phase0 = outgoing.phase_at((start - q) as f64);
        let (_, phase_end) = integrate_phase(&profile, phase0, samp_rate);
        let err = wrap_phase(incoming.phase_at(end as f64) - phase_end);
        let bump_len = layout.bump.len();
        let bump_area = (bump_len - BUMP_TAPER) as f64;
        let bump_capacity = TAU * BUMP_MAX_HZ * bump_area / samp_rate;
        let err_bump = err.clamp(-bump_capacity, bump_capacity);
        let bump_amp = err_bump * samp_rate / (TAU * bump_area);
        for (k, freq) in profile[q + layout.bump.start..q + layout.bump.end]
            .iter_mut()
            .enumerate()
        {
            let shape = if k < BUMP_TAPER {
                taper[k]
            } else if k >= bump_len - BUMP_TAPER {
                taper[bump_len - 1 - k]
            } else {
                1.0
            };
            *freq += bump_amp * shape;
        }
        let spill = (err - err_bump) * samp_rate / (TAU * layout.hold.len() as f64);
        for freq in &mut profile[q + layout.hold.start..q + layout.hold.end] {
            *freq += spill;
        }
        let (phase, _) = integrate_phase(&profile, phase0, samp_rate);

        let amp_step = (amp_in - amp_out) / (profile.len() - 1) as f64;
        for (k, (sample, &phase)) in cleaned[start - q..end].iter_mut().zip(&phase).enumerate() {
            let synth = (amp_out + amp_step * k as f64) * phase.cos();
            let blend = fade.get(k).copied().unwrap_or(1.0);
            *sample = (f64::from(*sample) * (1.0 - blend) + synth * blend) as f32;
        }
    }
    cleaned
}

/// The restored chroma block of a field, band-passed to the block and zeroed
/// before the first line with colour. A first pass identifies the lines;
/// with a confident fit the blanking intervals are regenerated in the
/// colour-under domain and the restoration runs again on the cleaned
/// signal. No per-line AGC: the amplitude envelope is the bell's, and
/// normalizing every line to its porch level would flatten the intended
/// foR/foB rest amplitude difference.
pub(super) fn process_chroma(
    field: &DecodedField,
    spec: &DecoderSpec,
    chroma: &[f32],
    mult: u32,
) -> Vec<f32> {
    let outwidth = field.outlinelen;
    // Peak amplitude putting the undeviated carrier near the porch RMS level
    // the other formats' chroma AGC normalizes to.
    let burst_abs_ref = spec
        .sys_burst_abs_ref
        .expect("chroma output without burst_abs_ref");
    let rest_amplitude = burst_abs_ref * 2f32.sqrt();
    let first_line = match field.burst_start_line() {
        line if line > 0 => STARTING_LINE.max(field.output_row(line as usize)),
        _ => STARTING_LINE,
    };
    let porch_end_px = usectooutpx(spec, spec.sys_active_video_us[0]) as usize;
    // The record chain's blanking-edge transient sets in slightly before the
    // nominal end of active video (the source's own blanking edge lands
    // inside the TBC active window), so the splice starts a little early.
    let blank_start_px = usectooutpx(spec, spec.sys_active_video_us[1] - 0.85) as usize;

    let pass1 = upconvert(chroma, spec, mult, rest_amplitude);
    let fit = fit_line_alternation(&pass1.inst_freq, outwidth, first_line, porch_end_px)
        .filter(|fit| fit.confidence >= IDENT_MIN_CONFIDENCE);
    let layout = BlankingLayout::new(outwidth, blank_start_px, porch_end_px);
    let restored = match fit.zip(layout) {
        Some((fit, layout)) => {
            tracing::debug!(
                confidence = fit.confidence,
                dr_on_even = fit.dr_on_even,
                "SECAM blanking reference regenerated"
            );
            let cleaned = regenerate_blanking(
                chroma,
                &pass1.envelope,
                spec.tbc_sample_rate(),
                outwidth,
                &layout,
                blank_start_px,
                first_line,
                fit.dr_on_even,
                mult,
            );
            upconvert(&cleaned, spec, mult, rest_amplitude).signal
        }
        None => {
            tracing::debug!("SECAM blanking left as-is (line identification confidence too low)");
            pass1.signal
        }
    };

    let mut block = sosfiltfilt_f32(&spec.chroma_filter_final, &restored);
    let blanked = (first_line * outwidth).min(block.len());
    block[..blanked].fill(0.0);
    block
}
