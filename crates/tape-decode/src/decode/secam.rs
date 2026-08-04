//! SECAM method 1 (standard quarter count-down) chroma restoration.
//!
//! Tapes recorded to IEC 60774-1 6.4.1 / annex E figure E1 carry the studio
//! SECAM chroma block band-passed around 4.32 MHz and counted down by 4, so the
//! rest carriers on tape are foB/4 and foR/4 with the FM deviations divided by
//! 4 as well. Restoration is a x4 phase multiplication of the colour-under
//! analytic signal rather than a heterodyne mix: carrier and deviation scale
//! back up together, so tape timebase error self-corrects and there is no
//! conversion LO to servo (unlike ME-SECAM).
//!
//! Ported from the Python implementation in vhs-decode (`vhsdecode/chroma.py`,
//! commit e5d5db5d).

use super::*;

use std::f64::consts::{PI, TAU};

/// Subcarrier rest frequencies and HF ("cloche"/bell) pre-emphasis constants
/// from ITU-R BT.470-6 table 2 / BT.1700: the subcarrier amplitude follows
/// `G = M0 * |1 + j16F| / |1 + j1.26F|` with `F = f/f0 - f0/f`.
const SECAM_FOR: f64 = 4_406_250.0;
const SECAM_FOB: f64 = 4_250_000.0;
const SECAM_BELL_F0: f64 = 4_286_000.0;

/// Minimum fraction of lines that must match the fitted alternation before the
/// fit is allowed to teach the parity flywheel.
const SECAM_IDENT_MIN_CONFIDENCE: f64 = 0.7;

/// Legal carrier excursion (BT.470). The bell gain lookup is clamped to this so
/// noise and carrier switch transients don't get boosted by the bell skirts.
/// Since the phase increments are held inside the same corridor (see
/// [`SECAM_MAX_DEVIATION`]) this now only catches the smoothing residue, but it
/// costs nothing and keeps the gain lookup bounded on its own terms.
const SECAM_FREQ_MIN: f64 = 3.9e6;
const SECAM_FREQ_MAX: f64 = 4.756e6;

/// Maximum per-component deviation towards the far side of the carrier pair
/// (BT.470-6 table 2 item 2.12, BT.1700 part C table 4 item 10e): D'B runs
/// -350/+506 kHz and D'R -506/+350 kHz, so the two mirror across the pair and
/// share the corridor foB - 350 kHz .. foR + 350 kHz.
const SECAM_MAX_DEVIATION: f64 = 350e3;

/// Lines below this carry no usable chroma (vertical interval / head switch).
const STARTING_LINE: usize = 16;

/// Relative SECAM subcarrier HF pre-emphasis (bell) gain at the given
/// instantaneous frequency, normalized to 1.0 at f0 (BT.470-6).
fn secam_bell_gain(freq_hz: f64) -> f64 {
    let f = freq_hz / SECAM_BELL_F0;
    let bell_f = f - 1.0 / f;
    let num = 1.0 + (16.0 * bell_f) * (16.0 * bell_f);
    let den = 1.0 + (1.26 * bell_f) * (1.26 * bell_f);
    (num / den).sqrt()
}

fn median_of(values: &[f32]) -> f32 {
    let mut scratch = values.to_vec();
    median_from_values(&mut scratch)
}

/// Raised-cosine ramp of `len` points rising from 0 (exclusive of 1).
fn raised_cosine(len: usize) -> Vec<f64> {
    (0..len)
        .map(|i| 0.5 - 0.5 * (PI * i as f64 / len as f64).cos())
        .collect()
}

/// Wrap an angle into (-pi, pi].
fn wrap_pi(angle: f64) -> f64 {
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped == -PI {
        PI
    } else {
        wrapped
    }
}

/// The restored chroma block plus the by-products later stages reuse.
struct RestoredChroma {
    /// Restored chroma block signal (studio frequencies, bell-shaped).
    restored: Vec<f32>,
    /// Smoothed restored-domain instantaneous frequency, in Hz. Reused for line
    /// identification.
    inst_freq: Vec<f32>,
    /// Band-passed under-carrier envelope, used by blanking regeneration for
    /// local amplitude matching.
    envelope: Vec<f32>,
}

/// Restore the studio SECAM chroma block from a method 1 colour-under signal by
/// multiplying the carrier phase back up.
///
/// The divider outputs a constant-amplitude signal, so the BT.470 bell
/// pre-emphasis is regenerated here from the restored instantaneous frequency to
/// put the amplitude envelope back on spec for downstream SECAM decoders.
#[allow(clippy::too_many_arguments)]
fn upconvert_secam_method1(
    chroma: &[f32],
    forward_fft: &dyn Fft<f32>,
    inverse_fft: &dyn Fft<f32>,
    samp_rate: f64,
    under_bpf: &[Sos<f32>],
    carrier_mult: f64,
    rest_amplitude: f64,
) -> RestoredChroma {
    let filtered = sosfiltfilt_f32(under_bpf, chroma);
    let len = filtered.len();

    // Analytic signal over the whole field so short-window edge effects don't
    // bias the phase.
    let analytic = hilbert_f32(&filtered, forward_fft, inverse_fft);

    let envelope: Vec<f32> = analytic.iter().map(|z| z.norm()).collect();

    // Per-sample wrapped phase increments, taken from the product with the
    // conjugate of the previous sample rather than from a difference of two
    // atan2 results: the carrier advances ~0.38 rad per sample, comfortably
    // inside (-pi, pi], and this keeps the unwrap exact without accumulating an
    // f32 phase ramp whose resolution would swamp the deviation.
    let mut increments = vec![0.0f64; len];
    for i in 1..len {
        let product = analytic[i] * analytic[i - 1].conj();
        increments[i] = (product.im as f64).atan2(product.re as f64);
    }
    if len > 1 {
        increments[0] = increments[1];
    }

    // Clamp the under-carrier deviation to SECAM's legal corridor before the
    // multiplication scales it up. The studio or broadcast limiter already
    // bounded the pre-corrected colour difference to these limits, so anything
    // outside the corridor here is noise or a channel-truncation transient from
    // the colour-under recording chain: the tape channel cuts the FM sidebands
    // of pre-emphasized edges, and the resulting instantaneous-frequency
    // excursions would be multiplied by `carrier_mult` and then smeared into
    // streaks by every downstream SECAM decoder's de-emphasis. This is the one
    // stage where they are still small enough to remove cleanly, and clipping
    // the frequency (rather than the signal) keeps the carrier smooth: a
    // clipped span just becomes a constant-frequency stretch.
    //
    // The increments are the phase derivative already, so unlike the Python
    // reference there is nothing to reintegrate afterwards - the phase
    // accumulator below walks these same values, and a legal signal comes
    // through bit-identical (Python's gradient/cumsum round trip is a 2-tap
    // average, so it perturbs the phase even where nothing clips). Clipping the
    // raw backward difference rather than Python's central difference also
    // clips harder on brief transients: averaging neighbouring increments
    // spreads a one-sample spike over two, which lets anything up to ~20% over
    // the ceiling through untouched and halves the clip's bite beyond that. The
    // two converge as 1/N over an N-sample excursion.
    let increment_scale = TAU / (carrier_mult * samp_rate);
    let increment_min = (SECAM_FOB - SECAM_MAX_DEVIATION) * increment_scale;
    let increment_max = (SECAM_FOR + SECAM_MAX_DEVIATION) * increment_scale;
    for increment in &mut increments {
        *increment = increment.clamp(increment_min, increment_max);
    }

    // Restored instantaneous frequency for the bell shaping. Central difference
    // plus a short moving average keeps sample-level phase noise from ending up
    // as amplitude noise; the bell curve itself is smooth so this doesn't blunt
    // legitimate deviation.
    let freq_scale = carrier_mult * samp_rate / TAU;
    let mut raw_freq = vec![0.0f64; len];
    for i in 0..len {
        let central = if i == 0 {
            increments[(1).min(len - 1)]
        } else if i + 1 >= len {
            increments[i]
        } else {
            (increments[i] + increments[i + 1]) / 2.0
        };
        raw_freq[i] = central * freq_scale;
    }

    const SMOOTH_LEN: usize = 9;
    let half_smooth = SMOOTH_LEN / 2;
    let mut inst_freq = vec![0.0f32; len];
    // Running sum over the centered window, with partial windows at the field
    // edges (numpy's 'same' convolution zero-pads there, which would pull the
    // first and last few samples of the vertical interval toward 0 Hz).
    let mut window_sum = 0.0f64;
    let mut window_len = 0usize;
    for i in 0..len {
        if i == 0 {
            for &value in raw_freq.iter().take(half_smooth + 1) {
                window_sum += value;
                window_len += 1;
            }
        } else {
            if let Some(&entering) = raw_freq.get(i + half_smooth) {
                window_sum += entering;
                window_len += 1;
            }
            if i > half_smooth {
                window_sum -= raw_freq[i - half_smooth - 1];
                window_len -= 1;
            }
        }
        inst_freq[i] =
            (window_sum / window_len as f64).clamp(SECAM_FREQ_MIN, SECAM_FREQ_MAX) as f32;
    }

    // Scale by the normalized under-carrier envelope (capped just above
    // nominal). Where the carrier is healthy this is ~unity, so the average
    // amplitude stays on the bell curve; where it dips or disappears (dropouts,
    // FM clicks, no colour) the dip is passed through to the output instead of
    // being hard-limited away. Downstream SECAM decoders key their click/dropout
    // concealment off exactly those envelope collapses, so preserving them
    // matters more than emulating the constant-amplitude divider chain of a real
    // deck - and it doubles as the squelch that keeps carrier-free noise from
    // becoming full-scale splatter.
    let env_med = median_of(&envelope) as f64;

    let mut restored = vec![0.0f32; len];
    // The phase is kept wrapped: cos(carrier_mult * phase) is unchanged by whole
    // turns as long as carrier_mult is an integer, and a bounded argument keeps
    // the cosine's range reduction exact over a whole field.
    let mut phase = if len > 0 {
        (analytic[0].im as f64).atan2(analytic[0].re as f64)
    } else {
        0.0
    };
    for i in 0..len {
        if i > 0 {
            phase = wrap_pi(phase + increments[i]);
        }
        let limited = if env_med > 0.0 {
            (envelope[i] as f64 / env_med).min(1.25)
        } else {
            0.0
        };
        let gain = secam_bell_gain(inst_freq[i] as f64);
        restored[i] = (rest_amplitude * gain * limited * (carrier_mult * phase).cos()) as f32;
    }

    RestoredChroma {
        restored,
        inst_freq,
        envelope,
    }
}

/// Fit the field's D'R/D'B line alternation from the active-region median
/// restored frequency of each line: D'R lines sit in the top half of the chroma
/// block, D'B in the bottom.
///
/// The sequence alternates strictly (BT.470), so fit the better of the two
/// possible parities; per-line deviation medians can land on the wrong side on
/// heavily saturated lines, the majority never does.
///
/// Returns `(dr_on_even, confidence)` where confidence is the fraction of lines
/// whose measured identity matches the fitted alternation, or `None` if there
/// are too few lines to fit.
fn fit_secam_line_alternation(
    inst_freq: &[f32],
    linesout: usize,
    outwidth: usize,
    first_line: usize,
    porch_end_px: usize,
) -> Option<(bool, f64)> {
    let n_lines = linesout.checked_sub(first_line)?;
    if n_lines < 32 {
        return None;
    }
    let active_start = porch_end_px + 30;
    let active_end = outwidth.checked_sub(40)?;
    if active_start >= active_end || linesout * outwidth > inst_freq.len() {
        return None;
    }

    let threshold = ((SECAM_FOR + SECAM_FOB) / 2.0) as f32;
    let mut scratch = vec![0.0f32; active_end - active_start];
    let mut even_is_dr = 0usize;
    for line_index in first_line..linesout {
        let base = line_index * outwidth;
        scratch.copy_from_slice(&inst_freq[base + active_start..base + active_end]);
        let is_dr = median_from_values(&mut scratch) > threshold;
        if is_dr == line_index.is_multiple_of(2) {
            even_is_dr += 1;
        }
    }

    let confidence = even_is_dr.max(n_lines - even_is_dr) as f64 / n_lines as f64;
    Some((even_is_dr * 2 >= n_lines, confidence))
}

/// Where a field's resolved parity came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParitySource {
    Measured,
    Flywheel,
    Unlocked,
}

impl ParitySource {
    fn as_str(self) -> &'static str {
        match self {
            ParitySource::Measured => "measured",
            ParitySource::Flywheel => "flywheel",
            ParitySource::Unlocked => "unlocked",
        }
    }
}

/// Carry the fitted D'R/D'B alternation across fields.
///
/// Each TBC field is 312.5 line periods, so the alternation phase of consecutive
/// fields walks a strict 4-field cycle:
///
/// ```text
/// dr_on_even(n) = base ^ (((n + 1) >> 1) & 1)
/// ```
///
/// A single bit therefore locks the parity of every field in the recording.
/// Fields whose own alternation fit is confident teach `base`; fields whose
/// content can't be fitted (near-neutral pictures, noisy tape) inherit the
/// predicted parity instead of losing their blanking regeneration.
///
/// The lock requires `MIN_LOCK` agreeing confident fields, expires after
/// `MAX_AGE` fields without confirmation, and a confident contradiction resets
/// it - a dropped field upstream shifts the cycle phase, and re-learning is
/// cheaper than trusting a stale lock.
struct SecamParityFlywheel {
    index: i64,
    last_readloc: Option<u64>,
    base: Option<bool>,
    agree: usize,
    last_confirm: Option<i64>,
}

impl SecamParityFlywheel {
    const MIN_LOCK: usize = 4;
    const MAX_AGE: i64 = 32;

    fn new() -> Self {
        Self {
            index: -1,
            last_readloc: None,
            base: None,
            agree: 0,
            last_confirm: None,
        }
    }

    fn flip(index: i64) -> bool {
        (((index + 1) >> 1) & 1) == 1
    }

    /// Advance to the field identified by `readloc` and resolve its parity.
    fn resolve(&mut self, readloc: u64, fit: Option<(bool, f64)>) -> (Option<bool>, ParitySource) {
        if self.last_readloc != Some(readloc) {
            self.last_readloc = Some(readloc);
            self.index += 1;
        }
        let n = self.index;
        let flip = Self::flip(n);

        if let Some((dr_on_even, confidence)) = fit {
            if confidence >= SECAM_IDENT_MIN_CONFIDENCE {
                let base = dr_on_even ^ flip;
                if self.base == Some(base) {
                    self.agree += 1;
                } else {
                    self.base = Some(base);
                    self.agree = 1;
                }
                self.last_confirm = Some(n);
                return (Some(dr_on_even), ParitySource::Measured);
            }
        }

        if let (Some(base), Some(last_confirm)) = (self.base, self.last_confirm) {
            if self.agree >= Self::MIN_LOCK && n - last_confirm <= Self::MAX_AGE {
                return (Some(base ^ flip), ParitySource::Flywheel);
            }
        }
        (None, ParitySource::Unlocked)
    }
}

/// A narrowband frequency/phase estimate of the colour-under carrier.
struct CarrierFit {
    f_rot: f64,
    df: f64,
    phase_mid: f64,
    t_mid: f64,
    samp_rate: f64,
}

impl CarrierFit {
    /// The modelled carrier phase at absolute sample `t`.
    fn phase_at(&self, t: f64) -> f64 {
        TAU * (self.f_rot / self.samp_rate) * t
            + self.phase_mid
            + TAU * (self.df / self.samp_rate) * (t - self.t_mid)
    }
}

/// Narrowband frequency/phase estimate of the colour-under carrier over
/// `chroma[start..start + length]` by correlation against a rotor at `f_rot`.
///
/// Correlation projects out everything away from `f_rot`, so this stays usable
/// on the raw (pre-band-pass) chroma channel where luma crosstalk would bias a
/// broadband analytic-signal measurement. Two half-window correlations give the
/// frequency offset from the rotor; the pooled correlation gives the phase.
fn measure_under_carrier(
    chroma: &[f32],
    samp_rate: f64,
    start: usize,
    length: usize,
    f_rot: f64,
) -> Option<CarrierFit> {
    if length == 0 || start + length > chroma.len() {
        return None;
    }
    let half = length / 2;
    if half == 0 {
        return None;
    }

    let (mut z1_re, mut z1_im, mut z2_re, mut z2_im) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (i, &sample) in chroma[start..start + length].iter().enumerate() {
        let t = (start + i) as f64;
        let (sin, cos) = (-TAU * (f_rot / samp_rate) * t).sin_cos();
        let re = sample as f64 * cos;
        let im = sample as f64 * sin;
        if i < half {
            z1_re += re;
            z1_im += im;
        } else {
            z2_re += re;
            z2_im += im;
        }
    }

    let zf_re = z1_re + z2_re;
    let zf_im = z1_im + z2_im;
    if z1_re.hypot(z1_im) == 0.0 || z2_re.hypot(z2_im) == 0.0 || zf_re.hypot(zf_im) == 0.0 {
        return None;
    }

    // angle(z2 * conj(z1))
    let dphi = (z2_im * z1_re - z2_re * z1_im).atan2(z2_re * z1_re + z2_im * z1_im);
    // Keep runaway estimates (no real carrier in the window) inside the format's
    // legal deviation.
    let df = (dphi / (TAU * half as f64 / samp_rate)).clamp(-130e3, 130e3);

    Some(CarrierFit {
        f_rot,
        df,
        phase_mid: zf_im.atan2(zf_re),
        t_mid: start as f64 + (length - 1) as f64 / 2.0,
        samp_rate,
    })
}

/// Geometry of the synthesized blanking interval.
mod blanking {
    pub(super) const FADE_LEN: usize = 8;
    pub(super) const RAMP_LEN: usize = 20;
    pub(super) const MEAS_LEN: usize = 32;
    /// Rest-to-rest frequency step position within the NEXT line (px from its
    /// start): over the sync tip.
    pub(super) const STEP_START_PX: usize = 8;
    pub(super) const STEP_END_PX: usize = 40;
    pub(super) const BUMP_END_PX: usize = 88;
    pub(super) const BUMP_MAX_HZ: f64 = 170e3;
    pub(super) const BUMP_TAPER: usize = 12;
}

/// Replace each line's horizontal blanking interval - front porch, sync and back
/// porch in one continuous run - with a synthesized undeviated colour-under rest
/// carrier, phase-continuous with the active video on both sides.
///
/// On method 1 tapes the whole blanking interval carries the record chain's
/// divide-by-4 counter settling transient (blanking edges / SECAM subcarrier
/// phase reversals upset the divider), not the undeviated reference BT.470
/// promises. Two things go wrong if it is left in place:
///
/// - the zero-phase filters in this chain (the under-carrier band-pass here,
///   `chroma_filter_final` later) and the linear-phase cloche filters in
///   downstream SECAM decoders smear the end-of-line transient BACKWARDS into
///   the last ~2 us of active video, which demodulates as a magenta band down
///   the right edge of the picture (D'R deviates negative, D'B positive, so the
///   transient reads red on D'R lines and blue on D'B lines);
/// - decoders calibrate their discriminator zeros and line identification from
///   the back porch, and transient energy ringing into that window biases the
///   zeros, which shows up as a full-field colour cast.
///
/// This runs in the colour-under domain BEFORE the band-pass/analytic-signal
/// restoration pass, so the zero-phase filtering never sees the transient. One
/// continuous synthesis per blanking interval, phase-aligned to the outgoing
/// active carrier at its start and the incoming one at its end, with no interior
/// splices.
///
/// The synthesized frequency ramps from the measured outgoing carrier to the
/// outgoing line's rest frequency across the front porch, steps to the incoming
/// line's rest over the sync tip, and holds it through the back porch and the
/// fade-out; the phase is the integral of that profile, so it is continuous
/// throughout. The random phase difference between the two lines' carriers is
/// closed by a frequency bump over the sync region plus a small constant offset
/// across the hold.
#[allow(clippy::too_many_arguments)]
fn regenerate_secam_blanking(
    chroma: &[f32],
    envelope: &[f32],
    samp_rate: f64,
    linesout: usize,
    outwidth: usize,
    blank_start_px: usize,
    porch_end_px: usize,
    first_line: usize,
    dr_on_even: bool,
    carrier_mult: f64,
) -> Vec<f32> {
    use blanking::*;

    let mut cleaned = chroma.to_vec();
    let fade = raised_cosine(FADE_LEN);
    let ramp = raised_cosine(RAMP_LEN);
    let step_len = STEP_END_PX - STEP_START_PX;
    let mid_step = raised_cosine(step_len);
    let taper = raised_cosine(BUMP_TAPER);

    for linenumber in first_line..linesout.saturating_sub(1) {
        let line_is_dr = linenumber.is_multiple_of(2) == dr_on_even;
        let f_out_rest = if line_is_dr { SECAM_FOR } else { SECAM_FOB } / carrier_mult;
        let f_in_rest = if line_is_dr { SECAM_FOB } else { SECAM_FOR } / carrier_mult;
        let start = linenumber * outwidth + blank_start_px;
        let end = (linenumber + 1) * outwidth + porch_end_px;
        if end <= start {
            continue;
        }
        let span = end - start;
        if start < 2 * MEAS_LEN + FADE_LEN || end + 2 * MEAS_LEN > cleaned.len() {
            continue;
        }
        // Enough room for both ramps, the step and the phase-closure hold.
        if span < 2 * RAMP_LEN + 96 || porch_end_px >= span {
            continue;
        }

        let Some(out_meas) =
            measure_under_carrier(&cleaned, samp_rate, start - MEAS_LEN, MEAS_LEN, f_out_rest)
        else {
            continue;
        };
        let Some(in_meas) = measure_under_carrier(&cleaned, samp_rate, end, MEAS_LEN, f_in_rest)
        else {
            continue;
        };
        let f_out = out_meas.f_rot + out_meas.df;

        // Local amplitudes from the band-passed envelope: narrowband correlation
        // under-reads a deviating FM carrier, and an amplitude step at the
        // splice would read as a click downstream. Measured a little away from
        // the splice points, where the pass-1 envelope is still inflated by the
        // band-pass smear of the adjacent transient.
        let amp_out = median_of(&envelope[start - 2 * MEAS_LEN..start - MEAS_LEN]) as f64;
        let amp_in = median_of(&envelope[end + MEAS_LEN..end + 2 * MEAS_LEN]) as f64;

        // Frequency profile: measured outgoing -> outgoing rest (over the front
        // porch) -> incoming rest (step over the sync tip) -> incoming rest held
        // through the back porch, all raised-cosine.
        let next_line_p = span - porch_end_px; // px offset of the next line start
        let step0 = (next_line_p + STEP_START_PX)
            .max(RAMP_LEN)
            .min(span - RAMP_LEN - 96);
        let step1 = step0 + step_len;
        if step1 > span {
            continue;
        }

        // The write extends FADE_LEN beyond `start` on the outside, so the
        // fade-in sits OVER the phase-matched measured outgoing carrier (before
        // the transient sets in) instead of over raw transient next to the
        // picture. The incoming side gets NO fade at all: the synth ends
        // phase-closed against the incoming carrier model at `end`, and the raw
        // signal takes over with its natural continuity.
        let q = FADE_LEN; // profile offset of `start`
        let span_ext = span + FADE_LEN;
        let mut f_prof = vec![0.0f64; span_ext];
        f_prof[..q].fill(f_out);
        for (i, &value) in ramp.iter().enumerate() {
            f_prof[q + i] = f_out + (f_out_rest - f_out) * value;
        }
        f_prof[q + RAMP_LEN..q + step0].fill(f_out_rest);
        for (i, &value) in mid_step.iter().enumerate() {
            f_prof[q + step0 + i] = f_out_rest + (f_in_rest - f_out_rest) * value;
        }
        // Rest frequency holds right through the back porch AND the fade-out:
        // the porch is the decoders' discriminator-zero reference, and the
        // undeviated carrier is also zero colour difference, so the fade-out
        // region decodes as neutral instead of as a per-line click.
        f_prof[q + step1..].fill(f_in_rest);

        let phase_start = out_meas.phase_at(start as f64 - q as f64);
        let integrate = |profile: &[f64]| -> Vec<f64> {
            let mut phase = Vec::with_capacity(profile.len());
            let mut acc = 0.0f64;
            for &value in profile {
                phase.push(phase_start + TAU * acc / samp_rate);
                acc += value;
            }
            phase
        };

        let phase = integrate(&f_prof);
        let phase_at_end = phase[span_ext - 1] + TAU * f_prof[span_ext - 1] / samp_rate;
        let err = wrap_pi(in_meas.phase_at(end as f64) - phase_at_end);

        // Flat-top frequency excursion over the sync region: area = absorbed
        // phase. Starts no earlier than just before the next line (its band-pass
        // ring must stay out of the outgoing picture) and ends early enough that
        // the ring stays out of the porch reference window. Any spill past the
        // cap becomes a constant offset across the rest-frequency hold - never
        // more than a few kHz, too small to bias the per-field porch cluster
        // medians or flip a line identity label.
        let b0 = (RAMP_LEN + 4).max(next_line_p.saturating_sub(16));
        let b1 = (next_line_p + BUMP_END_PX).min(span - RAMP_LEN - 4);
        let hold_end = span - RAMP_LEN;
        if b1 <= b0 || b1 - b0 < 2 * BUMP_TAPER + 8 || b1 >= hold_end {
            continue;
        }
        let hold_len = hold_end - b1;
        if hold_len < 24 {
            continue;
        }

        let bump_len = b1 - b0;
        let bump_area = (bump_len - BUMP_TAPER) as f64; // amplitude * samples
        let bump_capacity = TAU * BUMP_MAX_HZ * bump_area / samp_rate;
        let err_bump = err.clamp(-bump_capacity, bump_capacity);
        let bump_amp = err_bump * samp_rate / (TAU * bump_area);
        for i in 0..bump_len {
            let shape = if i < BUMP_TAPER {
                taper[i]
            } else if i >= bump_len - BUMP_TAPER {
                taper[bump_len - 1 - i]
            } else {
                1.0
            };
            f_prof[q + b0 + i] += bump_amp * shape;
        }
        // Constant-offset spill over the rest-frequency hold (usually zero).
        let hold_offset = (err - err_bump) * samp_rate / (TAU * hold_len as f64);
        for value in &mut f_prof[q + b1..q + hold_end] {
            *value += hold_offset;
        }

        let phase = integrate(&f_prof);
        let amp_step = if span_ext > 1 {
            (amp_in - amp_out) / (span_ext - 1) as f64
        } else {
            0.0
        };
        for (i, &phase_value) in phase.iter().enumerate() {
            let synth = (amp_out + amp_step * i as f64) * phase_value.cos();
            let blend = if i < FADE_LEN { fade[i] } else { 1.0 };
            let target = &mut cleaned[start - q + i];
            *target = (*target as f64 * (1.0 - blend) + synth * blend) as f32;
        }
    }

    cleaned
}

/// Per-decode SECAM state carried across fields.
pub(crate) struct SecamState {
    flywheel: SecamParityFlywheel,
}

impl SecamState {
    pub(crate) fn new() -> Self {
        Self {
            flywheel: SecamParityFlywheel::new(),
        }
    }
}

/// SECAM method 1 chroma restoration: x4 phase multiplication instead of a
/// heterodyne mix, plus BT.470 bell amplitude regeneration.
pub(crate) fn process_chroma_secam_method1(
    field: &DecodedField,
    spec: &DecoderSpec,
    state: &mut SecamState,
    chroma: &[f32],
    burstarea: (isize, isize),
    carrier_mult: f64,
) -> Result<Vec<u16>> {
    let under_bpf = spec
        .chroma_filter_secam_under
        .as_ref()
        .context("missing SECAM under-carrier band-pass filter")?;
    let outwidth = field.outlinelen;
    let linesout = field.outlinecount;
    if linesout <= STARTING_LINE || chroma.len() < linesout * outwidth {
        bail!(
            "SECAM field too small to restore: {linesout} lines of {outwidth} against {} samples",
            chroma.len()
        );
    }

    // The restoration runs on the TBC output timebase (outlinelen samples per
    // line period), not on the nominal 4fsc rate.
    let samp_rate = spec.sys_outlinelen as f64 / (spec.sys_line_period * 1e-6);

    // Peak amplitude such that the undeviated carrier lands near the same porch
    // RMS level the other formats' chroma AGC normalizes to.
    let burst_abs_ref = spec.sys_burst_abs_ref.context("missing burst_abs_ref")? as f64;
    let rest_amplitude = burst_abs_ref * std::f64::consts::SQRT_2;

    let forward_fft = spec.fft_field_forward_f32.as_ref();
    let inverse_fft = spec.fft_field_inverse_f32.as_ref();
    let mut pass = upconvert_secam_method1(
        chroma,
        forward_fft,
        inverse_fft,
        samp_rate,
        under_bpf,
        carrier_mult,
        rest_amplitude,
    );

    // This port has no per-line colour-killer signal, so the first usable line is
    // simply the end of the vertical interval.
    let first_line = STARTING_LINE;
    let porch_end_px = (spec.sys_active_video_us[0] * spec.sys_outfreq) as usize;

    // Give downstream decoders the undeviated blanking-interval reference the
    // standard promises them; what comes off tape there is the record divider's
    // settling transient (see `regenerate_secam_blanking`). This is a two-pass
    // restore: the first pass above identifies the lines, then the blanking is
    // replaced in the colour-under domain and the restoration is run again on
    // the cleaned signal, so the zero-phase filtering never gets to smear the
    // transient into the picture or the porch reference.
    let fit = fit_secam_line_alternation(
        &pass.inst_freq,
        linesout,
        outwidth,
        first_line,
        porch_end_px,
    );
    let (dr_on_even, parity_source) = state.flywheel.resolve(field.readloc, fit);

    if let Some(dr_on_even) = dr_on_even {
        // The record chain's blanking-edge transient sets in slightly before the
        // nominal end of active video (the source's own blanking edge lands
        // inside the TBC active window), so the splice starts a little early.
        let blank_start_px = ((spec.sys_active_video_us[1] - 0.85) * spec.sys_outfreq) as usize;
        let cleaned = regenerate_secam_blanking(
            chroma,
            &pass.envelope,
            samp_rate,
            linesout,
            outwidth,
            blank_start_px,
            porch_end_px,
            first_line,
            dr_on_even,
            carrier_mult,
        );
        pass = upconvert_secam_method1(
            &cleaned,
            forward_fft,
            inverse_fft,
            samp_rate,
            under_bpf,
            carrier_mult,
            rest_amplitude,
        );
        tracing::debug!(
            "SECAM blanking reference regenerated ({}, fit confidence {})",
            parity_source.as_str(),
            fit.map_or_else(
                || "n/a".to_string(),
                |(_, confidence)| format!("{confidence:.02}")
            )
        );
    } else {
        tracing::debug!(
            "SECAM blanking left as-is (line ident confidence too low, no parity lock)"
        );
    }

    let mut uphet = pass.restored;
    uphet.truncate(linesout * outwidth);

    // Block-anchored final band-pass (same band as ME-SECAM).
    uphet = sosfiltfilt_f32(&spec.chroma_filter_final, &uphet);

    // No per-line chroma AGC here: the amplitude envelope was synthesised from
    // the BT.470 bell above, and normalizing every line to its porch level would
    // flatten the intended foR/foB rest amplitude difference. Just blank the
    // vertical interval and log the porch level like `acc` does for the other
    // formats.
    let blanked = (first_line * outwidth).min(uphet.len());
    uphet[..blanked].fill(0.0);

    let (burst_start, burst_end) = burstarea;
    if burst_start >= 0 && burst_end > burst_start && (burst_end as usize) <= outwidth {
        let mut porch_rms_total = 0.0f64;
        for linenumber in STARTING_LINE..linesout {
            let linestart = linenumber * outwidth;
            porch_rms_total +=
                rms(&uphet[linestart + burst_start as usize..linestart + burst_end as usize]);
        }
        tracing::debug!(
            "SECAM chroma porch level: {:.01}",
            porch_rms_total / (linesout - STARTING_LINE) as f64
        );
    }

    Ok(encode_chroma_u16(&uphet))
}

/// Encode restored chroma to the 16-bit output level convention, matching the
/// unity-scale case of `acc` in the heterodyne path: zero signal sits at 32767
/// and out-of-range samples wrap, as they do in the Python decoder's
/// `astype(np.uint16)`.
fn encode_chroma_u16(samples: &[f32]) -> Vec<u16> {
    const SIGNED_SAMPLE_MAX: f32 = 32767.0;
    samples
        .iter()
        .map(|&sample| ((sample + SIGNED_SAMPLE_MAX) as i64) as u16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::FftPlanner;

    /// TBC output rate for 625-line VHS: outlinelen samples per 64 us line.
    const TEST_SAMP_RATE: f64 = 1135.0 / 64e-6;
    const TEST_OUTWIDTH: usize = 1135;
    const TEST_PORCH_END_PX: usize = 186;
    const TEST_BLANK_START_PX: usize = 1093;
    const CARRIER_MULT: f64 = 4.0;

    fn under_bandpass() -> Vec<Sos<f32>> {
        let half = TEST_SAMP_RATE / 2.0;
        narrow_sos(
            &butter_sos(3, &[550e3 / half, 1300e3 / half], FilterBandType::Bandpass)
                .expect("under band-pass"),
        )
    }

    /// Run the restoration over a buffer, planning FFTs to match its length.
    fn restore(chroma: &[f32], rest_amplitude: f64) -> RestoredChroma {
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(chroma.len());
        let inverse = planner.plan_fft_inverse(chroma.len());
        upconvert_secam_method1(
            chroma,
            forward.as_ref(),
            inverse.as_ref(),
            TEST_SAMP_RATE,
            &under_bandpass(),
            CARRIER_MULT,
            rest_amplitude,
        )
    }

    /// Constant-amplitude colour-under tone at `under_freq`.
    fn under_tone(len: usize, under_freq: f64) -> Vec<f32> {
        (0..len)
            .map(|i| (TAU * under_freq * i as f64 / TEST_SAMP_RATE).cos() as f32)
            .collect()
    }

    #[test]
    fn x4_restores_carrier_and_deviation_together() {
        // A method 1 deck records foR/4 with the deviation divided by 4 as well;
        // playback has to bring both back up by the same factor.
        for deviation in [0.0, 100e3, -100e3, 200e3] {
            let restored_freq = SECAM_FOR + deviation;
            let pass = restore(&under_tone(16384, restored_freq / CARRIER_MULT), 1.0);
            // Skip the filter transients at both ends.
            let mut middle = pass.inst_freq[4096..12288].to_vec();
            let measured = median_from_values(&mut middle) as f64;
            assert!(
                (measured - restored_freq).abs() < 1e3,
                "deviation {deviation}: restored {measured} Hz, expected {restored_freq} Hz"
            );
        }
    }

    #[test]
    fn x4_restores_the_blue_rest_carrier_too() {
        let pass = restore(&under_tone(16384, SECAM_FOB / CARRIER_MULT), 1.0);
        let mut middle = pass.inst_freq[4096..12288].to_vec();
        let measured = median_from_values(&mut middle) as f64;
        assert!((measured - SECAM_FOB).abs() < 1e3, "measured {measured} Hz");
        // The two rest carriers must stay 156.25 kHz apart after restoration.
        let red = restore(&under_tone(16384, SECAM_FOR / CARRIER_MULT), 1.0);
        let mut red_middle = red.inst_freq[4096..12288].to_vec();
        let red_measured = median_from_values(&mut red_middle) as f64;
        assert!(((red_measured - measured) - (SECAM_FOR - SECAM_FOB)).abs() < 1e3);
    }

    #[test]
    fn deviation_outside_the_legal_corridor_is_clamped() {
        // Excursions past the BT.470 corridor are truncation transients or
        // noise, never signal, so the restoration pins them to the edge instead
        // of letting the x4 multiplication scale them up.
        for (restored_freq, expected) in [
            (4.9e6, SECAM_FOR + SECAM_MAX_DEVIATION),
            (3.7e6, SECAM_FOB - SECAM_MAX_DEVIATION),
        ] {
            let pass = restore(&under_tone(16384, restored_freq / CARRIER_MULT), 1.0);
            let mut middle = pass.inst_freq[4096..12288].to_vec();
            let measured = median_from_values(&mut middle) as f64;
            // The bell gain lookup clamps 250 Hz inside the corridor top, so
            // allow a kHz of slack rather than an exact edge match.
            assert!(
                (measured - expected).abs() < 1e3,
                "{restored_freq} Hz restored to {measured} Hz, expected the \
                 corridor edge at {expected} Hz"
            );
        }
    }

    #[test]
    fn legal_deviation_passes_through_unclamped() {
        // The corridor edges have to sit outside the deviation the standard
        // allows, or the clamp would eat real colour difference.
        for restored_freq in [SECAM_FOR + 200e3, SECAM_FOB - 200e3] {
            let pass = restore(&under_tone(16384, restored_freq / CARRIER_MULT), 1.0);
            let mut middle = pass.inst_freq[4096..12288].to_vec();
            let measured = median_from_values(&mut middle) as f64;
            assert!(
                (measured - restored_freq).abs() < 1e3,
                "{restored_freq} Hz came back as {measured} Hz"
            );
        }
    }

    #[test]
    fn bell_preemphasis_is_regenerated() {
        // The divider chain outputs constant amplitude; the restored signal has
        // to carry the BT.470 bell envelope again.
        const REST_AMPLITUDE: f64 = 7071.0;
        for restored_freq in [SECAM_FOR, SECAM_FOB, SECAM_FOR + 200e3] {
            let pass = restore(
                &under_tone(16384, restored_freq / CARRIER_MULT),
                REST_AMPLITUDE,
            );
            let peak = pass.restored[4096..12288]
                .iter()
                .fold(0.0f32, |acc, &value| acc.max(value.abs())) as f64;
            let expected = REST_AMPLITUDE * secam_bell_gain(restored_freq);
            assert!(
                (peak - expected).abs() / expected < 0.02,
                "at {restored_freq} Hz: peak {peak}, expected {expected}"
            );
        }
    }

    /// Colour-under field where active video carries each line's undeviated rest
    /// carrier and the whole blanking interval carries an off-frequency
    /// transient, standing in for the record divider's settling behaviour.
    fn field_with_blanking_transient(linesout: usize, dr_on_even: bool) -> Vec<f32> {
        let mut signal = vec![0.0f32; linesout * TEST_OUTWIDTH];
        let mut phase = 0.0f64;
        for (i, sample) in signal.iter_mut().enumerate() {
            let line = i / TEST_OUTWIDTH;
            let pos = i % TEST_OUTWIDTH;
            let line_is_dr = line.is_multiple_of(2) == dr_on_even;
            let rest = if line_is_dr { SECAM_FOR } else { SECAM_FOB } / CARRIER_MULT;
            let in_active = (TEST_PORCH_END_PX..TEST_BLANK_START_PX).contains(&pos);
            let freq = if in_active { rest } else { rest + 60e3 };
            *sample = phase.cos() as f32;
            phase += TAU * freq / TEST_SAMP_RATE;
        }
        signal
    }

    #[test]
    fn blanking_regeneration_restores_the_porch_reference() {
        let linesout = 8usize;
        let dr_on_even = true;
        let chroma = field_with_blanking_transient(linesout, dr_on_even);
        let envelope = vec![1.0f32; chroma.len()];

        let cleaned = regenerate_secam_blanking(
            &chroma,
            &envelope,
            TEST_SAMP_RATE,
            linesout,
            TEST_OUTWIDTH,
            TEST_BLANK_START_PX,
            TEST_PORCH_END_PX,
            1,
            dr_on_even,
            CARRIER_MULT,
        );

        // Decoders calibrate their discriminator zeros from roughly 65..5 px
        // before active video. That window must now sit on the incoming line's
        // rest carrier.
        for line in 3..linesout {
            let line_is_dr = line.is_multiple_of(2) == dr_on_even;
            let rest = if line_is_dr { SECAM_FOR } else { SECAM_FOB } / CARRIER_MULT;
            let window_start = line * TEST_OUTWIDTH + TEST_PORCH_END_PX - 62;
            let before = measure_under_carrier(&chroma, TEST_SAMP_RATE, window_start, 32, rest)
                .expect("porch fit before");
            let after = measure_under_carrier(&cleaned, TEST_SAMP_RATE, window_start, 32, rest)
                .expect("porch fit after");
            assert!(
                before.df.abs() > 40e3,
                "line {line}: transient should be present before regeneration ({} Hz)",
                before.df
            );
            assert!(
                after.df.abs() < 4e3,
                "line {line}: porch off rest by {} Hz after regeneration",
                after.df
            );
        }
    }

    #[test]
    fn blanking_regeneration_leaves_active_video_alone() {
        let linesout = 8usize;
        let chroma = field_with_blanking_transient(linesout, true);
        let envelope = vec![1.0f32; chroma.len()];
        let cleaned = regenerate_secam_blanking(
            &chroma,
            &envelope,
            TEST_SAMP_RATE,
            linesout,
            TEST_OUTWIDTH,
            TEST_BLANK_START_PX,
            TEST_PORCH_END_PX,
            1,
            true,
            CARRIER_MULT,
        );

        // The synthesis writes from FADE_LEN before `blank_start_px` up to
        // `porch_end_px` of the next line, and must not touch a sample of the
        // picture beyond that.
        for line in 2..linesout - 1 {
            let base = line * TEST_OUTWIDTH;
            for pos in TEST_PORCH_END_PX..TEST_BLANK_START_PX - blanking::FADE_LEN {
                assert_eq!(
                    chroma[base + pos],
                    cleaned[base + pos],
                    "line {line} pos {pos} inside active video was modified"
                );
            }
        }
    }

    #[test]
    fn blanking_regeneration_stays_phase_continuous() {
        let linesout = 8usize;
        let chroma = field_with_blanking_transient(linesout, true);
        let envelope = vec![1.0f32; chroma.len()];
        let cleaned = regenerate_secam_blanking(
            &chroma,
            &envelope,
            TEST_SAMP_RATE,
            linesout,
            TEST_OUTWIDTH,
            TEST_BLANK_START_PX,
            TEST_PORCH_END_PX,
            1,
            true,
            CARRIER_MULT,
        );

        // A splice discontinuity would show up as a sample-to-sample step far
        // larger than the carrier's own per-sample excursion. The under carrier
        // advances at most ~0.4 rad per sample, so a unit-amplitude tone never
        // steps by more than ~0.4.
        let start = 2 * TEST_OUTWIDTH;
        let max_step = cleaned[start..]
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 0.5, "largest sample step {max_step}");
    }

    #[test]
    fn chroma_encode_puts_zero_at_the_u16_midpoint() {
        assert_eq!(encode_chroma_u16(&[0.0]), vec![32767u16]);
        assert_eq!(encode_chroma_u16(&[1.0, -1.0]), vec![32768u16, 32766u16]);
        // Full-scale positive and negative rest carriers stay inside the range.
        assert_eq!(encode_chroma_u16(&[32767.0]), vec![65534u16]);
        assert_eq!(encode_chroma_u16(&[-32767.0]), vec![0u16]);
    }

    #[test]
    fn bell_gain_is_unity_at_reference() {
        assert!((secam_bell_gain(SECAM_BELL_F0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn bell_gain_boosts_away_from_reference() {
        // The bell rises on both sides of f0, and both rest carriers sit off it.
        assert!(secam_bell_gain(SECAM_FOR) > 1.0);
        assert!(secam_bell_gain(SECAM_FOB) > 1.0);
        // 16/1.26 is the asymptotic gain; the skirts stay below it.
        assert!(secam_bell_gain(SECAM_FREQ_MAX) < 16.0 / 1.26);
    }

    #[test]
    fn median_matches_numpy_semantics() {
        assert_eq!(median_of(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median_of(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert!(median_of(&[]).is_nan());
    }

    #[test]
    fn wrap_pi_folds_into_half_open_turn() {
        assert!((wrap_pi(0.5) - 0.5).abs() < 1e-12);
        assert!((wrap_pi(TAU + 0.5) - 0.5).abs() < 1e-12);
        assert!((wrap_pi(-TAU - 0.5) + 0.5).abs() < 1e-12);
        assert!((wrap_pi(PI).abs() - PI).abs() < 1e-12);
    }

    #[test]
    fn parity_flywheel_walks_the_four_field_cycle() {
        // dr_on_even(n) = base ^ (((n + 1) >> 1) & 1) with base = false gives
        // the FTTF/TFFT alternation seen on real method 1 tapes.
        let expected: Vec<bool> = (0..8).map(SecamParityFlywheel::flip).collect();
        assert_eq!(
            expected,
            vec![false, true, true, false, false, true, true, false]
        );
    }

    #[test]
    fn parity_flywheel_carries_unfittable_fields() {
        let mut flywheel = SecamParityFlywheel::new();
        // Four confident fields in a row teach and lock `base`.
        for index in 0..4u64 {
            let expected = SecamParityFlywheel::flip(index as i64);
            let (parity, source) = flywheel.resolve(index, Some((expected, 1.0)));
            assert_eq!(parity, Some(expected));
            assert_eq!(source, ParitySource::Measured);
        }
        // A field that can't be fitted now inherits the predicted parity.
        let (parity, source) = flywheel.resolve(4, None);
        assert_eq!(source, ParitySource::Flywheel);
        assert_eq!(parity, Some(SecamParityFlywheel::flip(4)));
    }

    #[test]
    fn parity_flywheel_stays_unlocked_before_min_lock() {
        let mut flywheel = SecamParityFlywheel::new();
        flywheel.resolve(0, Some((false, 1.0)));
        let (parity, source) = flywheel.resolve(1, None);
        assert_eq!(parity, None);
        assert_eq!(source, ParitySource::Unlocked);
    }

    #[test]
    fn parity_flywheel_ignores_low_confidence_fits() {
        let mut flywheel = SecamParityFlywheel::new();
        let (parity, source) = flywheel.resolve(0, Some((true, 0.5)));
        assert_eq!(parity, None);
        assert_eq!(source, ParitySource::Unlocked);
    }

    #[test]
    fn carrier_measurement_recovers_frequency_and_phase() {
        let samp_rate = 17_734_375.0;
        let f_true = 1_082_031.25 + 20_000.0;
        let phase0 = 0.7;
        let start = 1000usize;
        let length = 32usize;
        let signal: Vec<f32> = (0..start + length + 16)
            .map(|t| (TAU * f_true * t as f64 / samp_rate + phase0).cos() as f32)
            .collect();

        let fit = measure_under_carrier(&signal, samp_rate, start, length, 1_082_031.25)
            .expect("carrier fit");
        assert!(
            ((fit.f_rot + fit.df) - f_true).abs() < 1500.0,
            "measured {} vs {f_true}",
            fit.f_rot + fit.df
        );
        // The modelled phase must track the real carrier across the window.
        for t in [start, start + length / 2, start + length - 1] {
            let expected = TAU * f_true * t as f64 / samp_rate + phase0;
            assert!(wrap_pi(fit.phase_at(t as f64) - expected).abs() < 0.05);
        }
    }

    #[test]
    fn line_alternation_fit_reads_the_carrier_pair() {
        let outwidth = 1135usize;
        let linesout = 120usize;
        let first_line = 16usize;
        let porch_end_px = 186usize;
        let mut inst_freq = vec![0.0f32; linesout * outwidth];
        // D'R on even lines.
        for line in 0..linesout {
            let value = if line.is_multiple_of(2) {
                SECAM_FOR as f32
            } else {
                SECAM_FOB as f32
            };
            inst_freq[line * outwidth..(line + 1) * outwidth].fill(value);
        }

        let (dr_on_even, confidence) =
            fit_secam_line_alternation(&inst_freq, linesout, outwidth, first_line, porch_end_px)
                .expect("fit");
        assert!(dr_on_even);
        assert_eq!(confidence, 1.0);

        // Swapping the assignment flips the fit, still at full confidence.
        for line in 0..linesout {
            let value = if line.is_multiple_of(2) {
                SECAM_FOB as f32
            } else {
                SECAM_FOR as f32
            };
            inst_freq[line * outwidth..(line + 1) * outwidth].fill(value);
        }
        let (dr_on_even, confidence) =
            fit_secam_line_alternation(&inst_freq, linesout, outwidth, first_line, porch_end_px)
                .expect("fit");
        assert!(!dr_on_even);
        assert_eq!(confidence, 1.0);
    }

    #[test]
    fn line_alternation_fit_needs_enough_lines() {
        let outwidth = 1135usize;
        let inst_freq = vec![SECAM_FOR as f32; 40 * outwidth];
        assert!(fit_secam_line_alternation(&inst_freq, 40, outwidth, 16, 186).is_none());
    }
}
