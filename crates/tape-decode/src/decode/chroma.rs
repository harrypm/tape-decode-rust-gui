use super::*;
use rustfft::num_complex::Complex64;

const SIGNED_SAMPLE_MAX: f32 = 32767.0;
/// Output value of zero chroma.
const ZERO_LEVEL: u16 = SIGNED_SAMPLE_MAX as u16;
/// Burst phase in degrees the NTSC chroma is compensated to.
const NTSC_TARGET_BURST_PHASE: f32 = -33.0;

/// Mirror an output sample around the zero level.
pub(super) fn invert(sample: u16) -> u16 {
    (2 * ZERO_LEVEL).wrapping_sub(sample)
}

/// Whether the field's chroma follows the NTSC color-frame sequence: every
/// line is phase-compensated to a fixed burst phase, which the second color
/// frame carries inverted.
pub(super) fn color_framed(spec: &DecoderSpec, field: &DecodedField) -> bool {
    spec.rf_write_chroma
        && spec.color_system == ColorSystem::Ntsc
        && !spec.rf_disable_phase_correction
        && field.burst_start_line() != -1
}

/// Burst-normalizes the chroma and encodes it straight to the u16 output.
/// Each line's gain is `burst_abs_ref` over its fitted burst amplitude,
/// capped at the field's median plus two MADs (so a line that lost its burst
/// to a dropout is not blown up), smoothed over the neighbouring lines and
/// ramped linearly across the line into the next line's gain. Lines the
/// color killer blanks stay at the zero level.
fn chroma_automatic_gain(
    chroma: &[f32],
    burst_abs_ref: f32,
    field: &DecodedField,
    phase_sequence: &[PhaseSequenceEntry],
    burst_start_line: i64,
) -> Vec<u16> {
    const SMOOTHING_HALF_WINDOW: usize = 4;
    const CAP_MADS: f64 = 2.0;

    let burst_abs_ref = f64::from(burst_abs_ref);
    let has_color = |entry: &PhaseSequenceEntry| entry.line_number as i64 >= burst_start_line;
    let raw_gains: Vec<f64> = phase_sequence
        .iter()
        .map(|entry| {
            let amplitude = f64::from(entry.burst.amplitude);
            burst_abs_ref / if amplitude != 0.0 { amplitude } else { 1e-8 }
        })
        .collect();

    let mut valid_gains: Vec<f64> = phase_sequence
        .iter()
        .zip(&raw_gains)
        .filter(|(entry, _)| has_color(entry))
        .map(|(_, &gain)| gain)
        .collect();
    let gain_cap = if valid_gains.is_empty() {
        1.0
    } else {
        let median = median_from_values(&mut valid_gains);
        let mut deviations: Vec<f64> = valid_gains
            .iter()
            .map(|gain| (gain - median).abs())
            .collect();
        median + CAP_MADS * median_from_values(&mut deviations)
    };

    let smoothed_gains: Vec<f64> = (0..raw_gains.len())
        .map(|i| {
            let window = &raw_gains[i.saturating_sub(SMOOTHING_HALF_WINDOW)
                ..raw_gains.len().min(i + SMOOTHING_HALF_WINDOW + 1)];
            window.iter().map(|&gain| gain.min(gain_cap)).sum::<f64>() / window.len() as f64
        })
        .collect();

    let mut output = vec![ZERO_LEVEL; chroma.len()];
    let line_start = |entry: &PhaseSequenceEntry| line_bounds(field, chroma.len(), entry.line_number).0;
    for (i, entry) in phase_sequence.iter().enumerate() {
        if !has_color(entry) {
            continue;
        }
        let start = line_start(entry);
        let (end, gain_end) = match phase_sequence.get(i + 1) {
            Some(next) => (line_start(next), smoothed_gains[i + 1]),
            None => (chroma.len(), smoothed_gains[i]),
        };
        if start >= end {
            continue;
        }
        let gain_start = smoothed_gains[i];
        let gain_step = (gain_end - gain_start) / (end - start) as f64;
        for (j, (out, &sample)) in output[start..end]
            .iter_mut()
            .zip(&chroma[start..end])
            .enumerate()
        {
            let gain = (gain_start + j as f64 * gain_step) as f32;
            *out = (sample.mul_add(gain, SIGNED_SAMPLE_MAX) as i64) as u16;
        }
    }

    output
}

/// Output sample range of a field line, clamped to the buffer.
fn line_bounds(field: &DecodedField, len: usize, line_number: usize) -> (usize, usize) {
    let lineoffset = field.lineoffset + 1;
    let linestart = (line_number as isize - lineoffset as isize) * field.outlinelen as isize;
    let lineend = linestart + field.outlinelen as isize;
    (
        resolve_slice_bound(len, linestart),
        resolve_slice_bound(len, lineend),
    )
}

fn upconvert_chroma(
    chroma: &[f32],
    field: &DecodedField,
    phase_sequence: &[PhaseSequenceEntry],
    chroma_heterodyne: &[Vec<f32>],
) -> Vec<f32> {
    let mut uphet = vec![0.0f32; chroma.len()];

    for entry in phase_sequence {
        let (start, end) = line_bounds(field, chroma.len(), entry.line_number);
        if start >= end {
            continue;
        }

        // Pair the line as slices so the loop carries no per-sample bounds
        // checks and vectorizes.
        let heterodyne_row = &chroma_heterodyne[entry.heterodyne_phase][start..end];
        for ((out, &sample), &het) in uphet[start..end]
            .iter_mut()
            .zip(&chroma[start..end])
            .zip(heterodyne_row)
        {
            *out = sample * het;
        }
    }

    uphet
}

/// Upconvert each line with its heterodyne rotated so that the line's burst
/// lands on `target_phase` (degrees), instead of one rotation for the field.
/// The heterodyne runs at the line's fitted subcarrier frequency and chirps
/// towards the next line's, so a slowly drifting carrier is tracked without
/// phase steps at the line boundaries.
fn upconvert_chroma_phase_comp(
    chroma: &[f32],
    field: &DecodedField,
    spec: &DecoderSpec,
    phase_sequence: &[PhaseSequenceEntry],
    target_phase: f32,
) -> Vec<f32> {
    use std::f64::consts::FRAC_PI_2;

    let mut uphet = vec![0.0f32; chroma.len()];
    let fsc_hz = spec.sys_fsc_mhz * 1e6;
    let color_under = spec.decoder_color_under_carrier;
    // Phase advance per output sample of the nominal heterodyne (the sum of
    // the subcarrier, a quarter turn per sample, and the color-under carrier).
    let nominal_step = FRAC_PI_2 * (1.0 + color_under / fsc_hz);
    // Each line's carrier scaled by its measured subcarrier error.
    let het_hz = |entry: &PhaseSequenceEntry| entry.burst.frequency / fsc_hz * color_under;

    for (idx, entry) in phase_sequence.iter().enumerate() {
        let (start, end) = line_bounds(field, chroma.len(), entry.line_number);
        if start >= end {
            continue;
        }

        let het_hz_current = het_hz(entry);
        let het_hz_next = phase_sequence.get(idx + 1).map_or(het_hz_current, het_hz);
        let theta_0 = nominal_step * start as f64
            + entry.heterodyne_phase as f64 * FRAC_PI_2
            + f64::from(target_phase + entry.burst.phase).to_radians();
        let step = FRAC_PI_2 * (1.0 + het_hz_current / fsc_hz);
        // Half the per-sample step change that reaches the next line's rate by
        // the end of the line: theta_k = theta_0 + step k + chirp k^2.
        let chirp = 0.5 * (het_hz_next - het_hz_current) * FRAC_PI_2 / (fsc_hz * field.outlinelen as f64);
        let dc = f64::from(entry.burst.dc);

        // Walk the phasor exp(i theta_k) instead of evaluating the cosine per
        // sample: the step between samples grows by a constant 2 chirp, so both
        // are complex multiplications.
        let mut phasor = Complex64::from_polar(1.0, theta_0);
        let mut phasor_step = Complex64::from_polar(1.0, step + chirp);
        let phasor_step_step = Complex64::from_polar(1.0, 2.0 * chirp);
        for (out, &sample) in uphet[start..end].iter_mut().zip(&chroma[start..end]) {
            *out = (f64::from(sample) * -phasor.re - dc) as f32;
            phasor *= phasor_step;
            phasor_step *= phasor_step_step;
        }
    }

    uphet
}

fn burst_deemphasis(chroma: &mut [f32], field: &DecodedField, burstarea_end: usize) {
    let lineoffset = field.lineoffset + 1;
    for line in lineoffset..field.outlinecount + lineoffset {
        let linestart = (line - lineoffset) * field.outlinelen;
        let lineend = linestart + field.outlinelen;
        for sample in &mut chroma[linestart + burstarea_end + 4..lineend] {
            *sample *= 2.0;
        }
    }
}

fn comb_c(data: &mut [f32], line_len: usize, line_distance: usize) {
    let numlines = data.len() / line_len;
    if numlines <= 2 {
        return;
    }

    // The comb writes each line from its own (current) sample, the line
    // `line_distance` ahead, and the line `line_distance` behind. The current
    // and ahead lines are never overwritten before they are read (lines are
    // processed in increasing order), so they read straight from `data`. Only
    // the behind line may already be overwritten, so keep just the last
    // `line_distance` original lines in a small ring instead of copying the
    // whole field as the previous `data.to_vec()` did.
    let mut ring = vec![0.0f32; line_distance * line_len];
    for line_num in 16..numlines - 2 {
        let line_start = line_num * line_len;
        let advanced_start = (line_num + line_distance) * line_len;
        let slot = (line_num % line_distance) * line_len;
        let delayed = line_num - line_distance;
        // The first `line_distance` lines reach back before the processed range,
        // so those originals are still live in `data`; later ones come from the
        // ring slot they last wrote.
        let delayed_from_data = delayed < 16;
        let delayed_start = delayed * line_len;
        for offset in 0..line_len {
            let i = line_start + offset;
            let current = data[i];
            let advanced = data[advanced_start + offset];
            let delayed_sample = if delayed_from_data {
                data[delayed_start + offset]
            } else {
                ring[slot + offset]
            };
            ring[slot + offset] = current;
            data[i] = (current * 2.0 - advanced - delayed_sample) / 4.0;
        }
    }
}

fn process_chroma_internal(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    chroma_afc_state: &mut ChromaAfcState,
    mut conversion_lo: Option<&mut ConversionLoState>,
) -> Result<Vec<u16>> {
    let burst_start_line = field.burst_start_line();
    if burst_start_line == -1 {
        return Ok(vec![ZERO_LEVEL; field.outlinecount * field.outlinelen]);
    }

    let mut chroma = tracked_chroma(field, spec, chroma_afc_state)?;
    if let Some(lo) = conversion_lo
        .as_deref_mut()
        .filter(|_| spec.rf_secam_carrier_servo)
    {
        // Measure the rest carrier pair on the late back porch, 3.7 to 0.3 us
        // before active video starts.
        let active_start_px = (spec.sys_active_video_us[0] * spec.sys_outfreq) as isize;
        let porch = (active_start_px - 65, active_start_px - 5);
        if let Some(offset) =
            secam_under_carrier_offset(spec, &chroma, field.outlinecount, field.outlinelen, porch)
        {
            lo.servo_avg.push(offset);
            tracing::debug!(offset_hz = offset, "SECAM carrier servo measurement");
        }
    }
    if let Some(mult) = spec.decoder_chroma_carrier_mult {
        let restored = secam::process_chroma(field, spec, &chroma, mult)?;
        return Ok(restored
            .iter()
            .map(|&sample| (sample + SIGNED_SAMPLE_MAX) as i64 as u16)
            .collect());
    }
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    if is_ntsc {
        burst_deemphasis(&mut chroma, field, burst_area(spec).1)
    }

    let phase_sequence = &field
        .burst_lock
        .as_ref()
        .context("missing burst lock")?
        .phase_sequence;
    let mut uphet = if color_framed(spec, field) {
        // Heterodynes with the nominal carrier even under chroma AFC, which
        // measured the burst phases against the tracked carrier; the
        // compensation is then off by the two carriers' phase difference.
        upconvert_chroma_phase_comp(
            &chroma,
            field,
            spec,
            phase_sequence,
            NTSC_TARGET_BURST_PHASE,
        )
    } else {
        let chroma_heterodyne = match conversion_lo {
            Some(lo) => lo.heterodyne(
                spec,
                field.field_number * (field.outlinecount * field.outlinelen) as i64,
            ),
            None => active_chroma_heterodyne(spec, chroma_afc_state),
        };
        upconvert_chroma(&chroma, field, phase_sequence, chroma_heterodyne)
    };

    uphet = sosfiltfilt_f32(&spec.chroma_filter_final, &uphet);

    if let Some(sos) = spec.chroma_filter_deemphasis.as_ref() {
        uphet = sosfilt_f32(sos, &uphet);
    }

    if !spec.rf_disable_comb {
        let line_distance = if is_ntsc { 1 } else { 2 };
        comb_c(&mut uphet, field.outlinelen, line_distance);
    }

    let burst_abs_ref = spec.sys_burst_abs_ref.context("missing burst_abs_ref")?;
    Ok(chroma_automatic_gain(
        &uphet,
        burst_abs_ref,
        field,
        phase_sequence,
        burst_start_line,
    ))
}

// The TBC'd chroma, with the carrier re-measured from it under chroma AFC.
fn tracked_chroma(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    chroma_afc_state: &mut ChromaAfcState,
) -> Result<Vec<f32>> {
    let chroma_downscaled = downscale_raw_vec(field, &spec.sinc_lut, None, None, None, true)?;
    if !spec.chroma_afc_enabled() {
        return Ok(chroma_downscaled);
    }
    let bandpass = chroma_afc_state.get_chroma_bandpass(spec)?;
    let chroma_len = chroma_downscaled.len();
    let chroma = demod_chroma_filt_array(
        &chroma_downscaled,
        spec,
        &bandpass,
        chroma_len,
        Some((10.0 * (spec.sys_outfreq / 40.0)) as isize),
    );
    chroma_afc_state.freq_offset(spec, &chroma, true)?;
    Ok(chroma)
}

pub(crate) fn decode_chroma(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    chroma_afc_state: &mut ChromaAfcState,
    conversion_lo: Option<&mut ConversionLoState>,
) -> Result<Option<Vec<u16>>> {
    if spec.color_system == ColorSystem::Monochrome {
        return Ok(None);
    }
    if !spec.chroma_output {
        // The burst lock of the following fields heterodynes with the tracked
        // carrier, so luma must not depend on whether chroma is written.
        if spec.chroma_afc_enabled() && field.burst_start_line() != -1 {
            tracked_chroma(field, spec, chroma_afc_state)?;
        }
        return Ok(None);
    }
    let upconverted = process_chroma_internal(field, spec, chroma_afc_state, conversion_lo)?;
    Ok(Some(upconverted))
}

/// Analytic signal of `data` (its Hilbert transform as the imaginary part)
/// through the zero-padded transform pair.
pub(super) fn analytic_signal(
    data: &[f32],
    (r2c, inverse): &(Arc<dyn RealToComplex<f32>>, Arc<dyn Fft<f32>>),
) -> Vec<Complex32> {
    let n = r2c.len();
    let mut padded = vec![0.0f32; n];
    padded[..data.len()].copy_from_slice(data);
    let mut half = r2c.make_output_vec();
    r2c.process(&mut padded, &mut half)
        .expect("r2c forward FFT failed");
    let mut spectrum = analytic_spectrum(&half, n);
    fft_in_place_f32(&mut spectrum, inverse.as_ref(), true);
    spectrum.truncate(data.len());
    spectrum
}

/// Phase advance from `a` to `b`, in (-pi, pi].
pub(super) fn phase_step(a: Complex32, b: Complex32) -> f64 {
    let (ar, ai) = (f64::from(a.re), f64::from(a.im));
    let (br, bi) = (f64::from(b.re), f64::from(b.im));
    // arg(b * conj(a))
    (bi * ar - br * ai).atan2(br * ar + bi * ai)
}

/// Offset in Hz of the ME-SECAM colour-under rest carrier pair from its
/// nominal centre, measured over `porch` (output samples from each line's
/// start) on the late back porch, where the subcarrier sits undeviated; the
/// early porch is still sweeping from the previous line's carrier switch.
/// The pair midpoint moves with the recording VCR's down-conversion crystal
/// error, which otherwise offsets both restored subcarriers. Single-field
/// accuracy is about +-100 Hz (ringing from the per-line carrier switch beats
/// across the short window), which averaging over fields washes out; the
/// crystal errors chased are in the kHz range. `None` without a reliable
/// pair.
fn secam_under_carrier_offset(
    spec: &DecoderSpec,
    chroma: &[f32],
    linesout: usize,
    outwidth: usize,
    porch: (isize, isize),
) -> Option<f64> {
    // Stay clear of the vertical interval and the head switch.
    const SKIP_LINES: usize = 20;
    const MIN_LINES_PER_CLUSTER: usize = 8;
    // The carriers are nominally 156.25 kHz apart; reject measurements where
    // the two clusters land somewhere else entirely.
    const SEPARATION_RANGE: std::ops::RangeInclusive<f64> = 90e3..=230e3;

    let fft = spec.secam_analytic_fft.as_ref()?;
    let pair_center = spec.decoder_color_under_carrier;
    let freq_scale = spec.tbc_sample_rate() / std::f64::consts::TAU;
    // Over the whole field, so the short per-line windows are free of
    // transform edge effects (a windowed transform of just the porch would
    // bias the frequency estimate by hundreds of Hz).
    let analytic = analytic_signal(chroma, fft);

    let mut freqs = Vec::new();
    let mut envs = Vec::new();
    for line in SKIP_LINES..linesout.saturating_sub(SKIP_LINES) {
        let line_start = (line * outwidth) as isize;
        let (start, end) = (line_start + porch.0, line_start + porch.1);
        if start < 0 || end > analytic.len() as isize {
            continue;
        }
        let window = &analytic[start as usize..end as usize];
        // Instantaneous frequency; the median rejects FM clicks and noise
        // spikes.
        let mut f_inst: Vec<f64> = window
            .windows(2)
            .map(|pair| phase_step(pair[0], pair[1]) * freq_scale)
            .collect();
        let mut env: Vec<f64> = window
            .iter()
            .map(|sample| f64::from(sample.norm()))
            .collect();
        freqs.push(median_from_values(&mut f_inst));
        envs.push(median_from_values(&mut env));
    }
    if freqs.is_empty() {
        return None;
    }

    // Ignore lines whose porch carrier is too weak to measure (dropouts,
    // colour killed lines).
    let env_threshold = median_from_values(&mut envs.clone()) * 0.25;
    let (mut low, mut high): (Vec<f64>, Vec<f64>) = freqs
        .iter()
        .zip(&envs)
        .filter(|&(_, &env)| env > env_threshold)
        .map(|(&freq, _)| freq)
        .partition(|&freq| freq < pair_center);
    if low.len() < MIN_LINES_PER_CLUSTER || high.len() < MIN_LINES_PER_CLUSTER {
        return None;
    }
    let low_carrier = median_from_values(&mut low);
    let high_carrier = median_from_values(&mut high);
    if !SEPARATION_RANGE.contains(&(high_carrier - low_carrier)) {
        return None;
    }
    Some((low_carrier + high_carrier) / 2.0 - pair_center)
}
