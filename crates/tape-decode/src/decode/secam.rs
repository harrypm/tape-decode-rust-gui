//! SECAM method 1 colour-under restoration. The recorder passed the studio
//! chroma block through a divide-by-4 counter, so the rest carriers on tape
//! are foB/4 and foR/4 with the FM deviation divided by four as well, and
//! the block is restored by multiplying the carrier phase back up rather
//! than by mixing against a heterodyne. Carrier and deviation scale
//! together, so tape timebase error self-corrects and there is no
//! conversion LO to servo. The divider output has constant amplitude, so
//! the BT.470 bell pre-emphasis is regenerated from the restored
//! instantaneous frequency.

use std::f64::consts::TAU;
use std::ops::RangeInclusive;

use rustfft::num_complex::Complex64;

use super::chroma::{analytic_signal, phase_step};
use super::*;

/// Centre of the HF pre-emphasis bell (ITU-R BT.470).
const BELL_F0_HZ: f64 = 4_286_000.0;
/// Legal subcarrier excursion; the bell gain is looked up inside it so that
/// noise and carrier switch transients are not boosted by the bell skirts.
const CARRIER_RANGE_HZ: RangeInclusive<f64> = 3.9e6..=4.756e6;
/// Taps of the moving average over the instantaneous frequency, so that
/// sample-level phase noise does not end up as amplitude noise through the
/// bell (which is smooth enough not to be blunted by it).
const INST_FREQ_SMOOTHING: usize = 9;
/// Envelope ceiling relative to the field median.
const ENVELOPE_CAP: f64 = 1.25;
/// First line with chroma output.
const STARTING_LINE: usize = 16;

/// Relative subcarrier HF pre-emphasis (bell) gain at `freq_hz`, unity at
/// the bell centre: |1 + j16F| / |1 + j1.26F| with F = f/f0 - f0/f.
fn bell_gain(freq_hz: f64) -> f64 {
    let f = freq_hz / BELL_F0_HZ;
    let bell_f = f - 1.0 / f;
    ((1.0 + (16.0 * bell_f).powi(2)) / (1.0 + (1.26 * bell_f).powi(2))).sqrt()
}

/// Moving average of `len` (odd) taps centred on each sample, zero-padded at
/// the ends.
fn moving_average(values: &[f64], len: usize) -> Vec<f64> {
    debug_assert!(len % 2 == 1);
    let half = len / 2;
    let mut prefix = Vec::with_capacity(values.len() + 1);
    prefix.push(0.0);
    for &value in values {
        prefix.push(prefix.last().unwrap() + value);
    }
    (0..values.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(values.len());
            (prefix[hi] - prefix[lo]) / len as f64
        })
        .collect()
}

/// Restore the chroma block from the colour-under `chroma` by multiplying
/// the under carrier's phase by `mult`, with the bell amplitude regenerated
/// from the restored instantaneous frequency. `rest_amplitude` is the peak
/// amplitude of the undeviated carrier.
fn upconvert(
    chroma: &[f32],
    spec: &DecoderSpec,
    mult: u32,
    rest_amplitude: f64,
) -> Result<Vec<f32>> {
    let under_filter = spec
        .secam_under_filter
        .as_ref()
        .context("missing SECAM under band-pass")?;
    let fft = spec
        .secam_analytic_fft
        .as_ref()
        .context("missing SECAM analytic transform")?;
    let filtered = sosfiltfilt_f32(under_filter, chroma);
    // Over the whole field, so short-window edge effects do not bias the
    // phase.
    let analytic = analytic_signal(&filtered, fft);
    let mut envelope: Vec<f32> = analytic.iter().map(|z| z.norm()).collect();

    // Restored-domain instantaneous frequency: central differences of the
    // phase (one-sided at the ends), smoothed, bounded to the legal
    // excursion.
    let freq_scale = f64::from(mult) * spec.tbc_sample_rate() / TAU;
    let steps: Vec<f64> = analytic
        .windows(2)
        .map(|pair| phase_step(pair[0], pair[1]))
        .collect();
    let raw_freq: Vec<f64> = (0..analytic.len())
        .map(|i| {
            let back = i.checked_sub(1).map(|j| steps[j]);
            let ahead = steps.get(i).copied();
            let step = match (back, ahead) {
                (Some(back), Some(ahead)) => (back + ahead) / 2.0,
                (Some(step), None) | (None, Some(step)) => step,
                (None, None) => 0.0,
            };
            step * freq_scale
        })
        .collect();
    let mut inst_freq = moving_average(&raw_freq, INST_FREQ_SMOOTHING);
    for freq in &mut inst_freq {
        *freq = freq.clamp(*CARRIER_RANGE_HZ.start(), *CARRIER_RANGE_HZ.end());
    }

    // Scale by the normalized envelope, capped just above nominal: where the
    // carrier is healthy this is about unity and the amplitude stays on the
    // bell; where it dips or disappears (dropouts, FM clicks, no colour) the
    // dip passes through to the output instead of being limited away, since
    // downstream SECAM decoders key their concealment off exactly those
    // collapses, and carrier-free noise does not become full-scale splatter.
    let env_median = f64::from(median_from_values(&mut envelope));
    Ok(analytic
        .iter()
        .zip(&inst_freq)
        .map(|(z, &freq)| {
            let (re, im) = (f64::from(z.re), f64::from(z.im));
            let amplitude = re.hypot(im);
            if env_median <= 0.0 || amplitude == 0.0 {
                return 0.0;
            }
            let limited = (amplitude / env_median).min(ENVELOPE_CAP);
            // cos(mult * phase) as the unit phasor raised to mult.
            let unit = Complex64::new(re / amplitude, im / amplitude);
            (rest_amplitude * bell_gain(freq) * limited * unit.powu(mult).re) as f32
        })
        .collect())
}

/// The restored chroma block of a field, band-passed to the block and zeroed
/// before the first line with colour. No per-line AGC: the amplitude
/// envelope is the bell's, and normalizing every line to its porch level
/// would flatten the intended foR/foB rest amplitude difference.
pub(super) fn process_chroma(
    field: &DecodedField,
    spec: &DecoderSpec,
    chroma: &[f32],
    mult: u32,
) -> Result<Vec<f32>> {
    // Peak amplitude putting the undeviated carrier near the porch RMS level
    // the other formats' chroma AGC normalizes to.
    let burst_abs_ref = spec.sys_burst_abs_ref.context("missing burst_abs_ref")?;
    let rest_amplitude = f64::from(burst_abs_ref) * 2f64.sqrt();
    let first_line = STARTING_LINE.max(field.burst_start_line().max(0) as usize);

    let restored = upconvert(chroma, spec, mult, rest_amplitude)?;
    let mut block = sosfiltfilt_f32(&spec.chroma_filter_final, &restored);
    let blanked = (first_line * field.outlinelen).min(block.len());
    block[..blanked].fill(0.0);
    Ok(block)
}
