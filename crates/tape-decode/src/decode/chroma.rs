use super::*;
use crate::spec::next_fast_len;
use realfft::RealFftPlanner;
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

/// Signed chroma to the unsigned 16-bit output.
fn encode_chroma(chroma: &[f32]) -> Vec<u16> {
    chroma
        .iter()
        .map(|&sample| (sample + SIGNED_SAMPLE_MAX) as i64 as u16)
        .collect()
}

/// Burst-normalizes the chroma in place and measures its noise floor.
/// Each line's gain is `burst_abs_ref` over its fitted burst amplitude,
/// capped at the field's median plus two MADs (so a line that lost its burst
/// to a dropout is not blown up), smoothed over the neighbouring lines and
/// ramped linearly across the line into the next line's gain. Lines the
/// color killer blanks are zeroed. The noise floor is the MAD-estimated
/// standard deviation of the `sync_tip_len` samples before each gained
/// line's successor, less four at either end, averaged over the field.
fn chroma_automatic_gain(
    chroma: &mut [f32],
    burst_abs_ref: f32,
    field: &DecodedField,
    phase_sequence: &[PhaseSequenceEntry],
    burst_start_line: i64,
    sync_tip_len: usize,
) -> f32 {
    const SMOOTHING_HALF_WINDOW: usize = 4;
    const CAP_MADS: f32 = 2.0;
    // Standard deviation of a normal distribution per MAD.
    const MAD_TO_SIGMA: f32 = 1.4826;
    const EDGE: usize = 4;

    let has_color = |entry: &PhaseSequenceEntry| entry.line_number as i64 >= burst_start_line;
    let raw_gains: Vec<f32> = phase_sequence
        .iter()
        .map(|entry| {
            let amplitude = entry.burst.amplitude;
            burst_abs_ref / if amplitude != 0.0 { amplitude } else { 1e-8 }
        })
        .collect();

    let mut valid_gains: Vec<f32> = phase_sequence
        .iter()
        .zip(&raw_gains)
        .filter(|(entry, _)| has_color(entry))
        .map(|(_, &gain)| gain)
        .collect();
    let gain_cap = if valid_gains.is_empty() {
        1.0
    } else {
        let median = median_from_values(&mut valid_gains);
        let mut deviations: Vec<f32> = valid_gains
            .iter()
            .map(|gain| (gain - median).abs())
            .collect();
        median + CAP_MADS * median_from_values(&mut deviations)
    };

    let smoothed_gains: Vec<f32> = (0..raw_gains.len())
        .map(|i| {
            let window = &raw_gains[i.saturating_sub(SMOOTHING_HALF_WINDOW)
                ..raw_gains.len().min(i + SMOOTHING_HALF_WINDOW + 1)];
            window.iter().map(|&gain| gain.min(gain_cap)).sum::<f32>() / window.len() as f32
        })
        .collect();

    let len = chroma.len();
    let line_start = |entry: &PhaseSequenceEntry| line_bounds(field, len, entry.line_number).0;
    let mut noise_sum = 0.0f32;
    let mut noise_lines = 0usize;
    for (i, entry) in phase_sequence.iter().enumerate() {
        let start = line_start(entry);
        let (end, gain_end) = match phase_sequence.get(i + 1) {
            Some(next) => (line_start(next), smoothed_gains[i + 1]),
            None => (len, smoothed_gains[i]),
        };
        if start >= end {
            continue;
        }
        if !has_color(entry) {
            chroma[start..end].fill(0.0);
            continue;
        }
        let gain_start = smoothed_gains[i];
        let gain_step = (gain_end - gain_start) / (end - start) as f32;
        for (j, sample) in chroma[start..end].iter_mut().enumerate() {
            *sample *= gain_start + j as f32 * gain_step;
        }

        let window_end = end.saturating_sub(EDGE);
        let window_start = (end + EDGE).saturating_sub(sync_tip_len).min(window_end);
        let window = &chroma[window_start..window_end];
        if window.is_empty() {
            continue;
        }
        let median = median_from_values(&mut window.to_vec());
        let mut deviations: Vec<f32> = window.iter().map(|&v| (v - median).abs()).collect();
        noise_sum += median_from_values(&mut deviations) * MAD_TO_SIGMA;
        noise_lines += 1;
    }

    if noise_lines == 0 {
        0.0
    } else {
        noise_sum / noise_lines as f32
    }
}

/// Chroma transient improvement: steepens the color transitions the
/// color-under band limit smeared, without disturbing the hue. Each sample
/// forms an (I, Q) pair with its predecessor, compared with the pairs
/// `radius` samples back and ahead; where the vector distance swept across
/// them clears the noise gate, the sample is pulled towards the nearer end
/// of the sweep by a sigmoid of its progress along it, so the transition
/// sharpens around its midpoint. Four passes, each reading the previous
/// one's result with a quarter of its mix. `width` sets the radius in
/// subcarrier cycles (at least one) and scales the gate, which starts from
/// the chroma noise standard deviation `noise_floor`.
fn chroma_transient_improvement(
    chroma: &mut [f32],
    line_len: usize,
    noise_floor: f32,
    width: u32,
    mix: f32,
) {
    const PASSES: usize = 4;
    const DECAY: f32 = 0.25;

    if line_len == 0 {
        return;
    }
    let radius = (width as usize * 4).max(4);
    let gate = noise_floor * (width as f32).sqrt();
    let end = line_len.saturating_sub(radius + 1);
    let mut source = vec![0.0f32; line_len];
    for line in chroma.chunks_exact_mut(line_len) {
        for pass in 0..PASSES {
            let mix = mix * DECAY.powi(pass as i32);
            source.copy_from_slice(line);
            for s in radius + 1..end {
                let (i_now, q_now) = (source[s], source[s - 1]);
                let (i_past, q_past) = (source[s - radius], source[s - radius - 1]);
                let (i_next, q_next) = (source[s + radius], source[s + radius - 1]);
                let dist_back = ((i_now - i_past).powi(2) + (q_now - q_past).powi(2)).sqrt();
                let dist_forward = ((i_next - i_now).powi(2) + (q_next - q_now).powi(2)).sqrt();
                let total = dist_back + dist_forward;
                if total <= gate {
                    continue;
                }
                let progress = dist_back / total;
                let (weight, from, to) = if progress < 0.5 {
                    (4.0 * progress * progress, i_past, i_now)
                } else {
                    let remaining = 1.0 - progress;
                    (1.0 - 4.0 * remaining * remaining, i_now, i_next)
                };
                let target = from + weight * (to - from);
                line[s] = i_now + mix * (target - i_now);
            }
        }
    }
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
    chroma: &mut [f32],
    field: &DecodedField,
    spec: &DecoderSpec,
    phase_sequence: &[PhaseSequenceEntry],
    target_phase: f32,
) {
    use std::f64::consts::FRAC_PI_2;

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
        for sample in &mut chroma[start..end] {
            *sample = (f64::from(*sample) * -phasor.re - dc) as f32;
            phasor *= phasor_step;
            phasor_step *= phasor_step_step;
        }
    }
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

/// Zero-phase band-pass of the upconverted chroma around the subcarrier, as
/// a super-Gaussian spectrum mask `exp(-((f - fsc) / bw)^4)` over the whole
/// field: 1.3 MHz wide below fsc for the chroma band, and above it as wide
/// as puts the heterodyne sum product, two color-under carriers up, 80 dB
/// down. The field is reflect-padded to a fast transform length so the
/// mask's ringing settles outside the picture.
pub(crate) struct ChromaFftFilter {
    r2c: Arc<dyn RealToComplex<f32>>,
    c2r: Arc<dyn ComplexToReal<f32>>,
    /// Gain per unique bin.
    mask: Vec<f32>,
}

impl ChromaFftFilter {
    const LOWER_BANDWIDTH_HZ: f64 = 1.3e6;
    const SUM_PRODUCT_ATTENUATION_DB: f64 = 80.0;
    const ORDER: i32 = 2;
    /// Least reflect padding on each side.
    const PAD: usize = 256;

    /// For fields of `len` samples at four samples per subcarrier cycle.
    pub(crate) fn new(len: usize, fsc_hz: f64, color_under_hz: f64) -> Self {
        let n = next_fast_len(len + 2 * Self::PAD);
        let exponent = 2 * Self::ORDER;
        // The upper width that puts the sum product at the attenuation:
        // exp(-(2 color_under / bw)^exponent) = 10^(-dB / 20).
        let attenuation = 10f64.powf(-Self::SUM_PRODUCT_ATTENUATION_DB / 20.0);
        let upper_bandwidth =
            2.0 * color_under_hz / (-attenuation.ln()).powf(1.0 / f64::from(exponent));
        let bin_hz = 4.0 * fsc_hz / n as f64;
        let mask = (0..=n / 2)
            .map(|bin| {
                let delta = bin as f64 * bin_hz - fsc_hz;
                let bandwidth = if delta <= 0.0 {
                    Self::LOWER_BANDWIDTH_HZ
                } else {
                    upper_bandwidth
                };
                (-(delta / bandwidth).powi(exponent)).exp() as f32
            })
            .collect();
        let mut planner = RealFftPlanner::<f32>::new();
        Self {
            r2c: planner.plan_fft_forward(n),
            c2r: planner.plan_fft_inverse(n),
            mask,
        }
    }

    fn apply(&self, chroma: &[f32]) -> Vec<f32> {
        let n = self.r2c.len();
        let len = chroma.len();
        let pad_left = n.saturating_sub(len) / 2;
        let pad_right = n.saturating_sub(len) - pad_left;
        assert!(
            len + 2 * Self::PAD <= n && pad_right < len,
            "chroma of {len} samples does not fit the {n}-sample filter sized for the field"
        );
        let mut padded = Vec::with_capacity(n);
        padded.extend((1..=pad_left).rev().map(|i| chroma[i]));
        padded.extend_from_slice(chroma);
        padded.extend((0..pad_right).map(|i| chroma[len - 2 - i]));

        let mut spectrum = self.r2c.make_output_vec();
        self.r2c
            .process(&mut padded, &mut spectrum)
            .expect("r2c forward FFT failed");
        for (bin, &gain) in spectrum.iter_mut().zip(&self.mask) {
            *bin *= gain;
        }
        // The c2r transform rejects imaginary residue on the DC and Nyquist
        // bins.
        spectrum[0].im = 0.0;
        if n.is_multiple_of(2) {
            spectrum[n / 2].im = 0.0;
        }
        self.c2r
            .process(&mut spectrum, &mut padded)
            .expect("c2r inverse FFT failed");
        let scale = 1.0 / n as f32;
        padded[pad_left..pad_left + len]
            .iter()
            .map(|&sample| sample * scale)
            .collect()
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
        let restored = secam::process_chroma(field, spec, &chroma, mult);
        return Ok(encode_chroma(&restored));
    }
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    if is_ntsc {
        burst_deemphasis(&mut chroma, field, burst_area(spec).1)
    }

    let phase_sequence = &field
        .burst_lock
        .as_ref()
        .expect("valid field without a burst lock")
        .phase_sequence;
    let mut uphet = if color_framed(spec, field) {
        // Heterodynes with the nominal carrier even under chroma AFC, which
        // measured the burst phases against the tracked carrier; the
        // compensation is then off by the two carriers' phase difference.
        upconvert_chroma_phase_comp(
            &mut chroma,
            field,
            spec,
            phase_sequence,
            NTSC_TARGET_BURST_PHASE,
        );
        chroma
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

    // Keeps the difference product of the mix, at the subcarrier, and
    // rejects the sum product. ME-SECAM has no FFT mask: its restored block
    // sits at the conversion LO minus the colour-under carrier rather than
    // at fsc, so the fsc-anchored mask would ride ~106 kHz high on it and
    // lose the tight top edge that suppresses high-side FM splatter from
    // saturated transitions.
    uphet = match &spec.chroma_fft_filter {
        Some(filter) => filter.apply(&uphet),
        None => sosfiltfilt_f32(&spec.chroma_filter_final, &uphet),
    };

    if let Some(sos) = spec.chroma_filter_deemphasis.as_ref() {
        uphet = sosfilt_f32(sos, &uphet);
    }

    if !spec.rf_disable_comb {
        let line_distance = if is_ntsc { 1 } else { 2 };
        comb_c(&mut uphet, field.outlinelen, line_distance);
    }

    let burst_abs_ref = spec
        .sys_burst_abs_ref
        .expect("chroma output without burst_abs_ref");
    let sync_tip_len = (spec.sys_hsync_pulse_us * spec.sys_outfreq).floor() as usize;
    let noise_floor = chroma_automatic_gain(
        &mut uphet,
        burst_abs_ref,
        field,
        phase_sequence,
        burst_start_line,
        sync_tip_len,
    );

    tracing::debug!(noise_floor, "chroma noise floor");

    if spec.cti_mix != 0.0 {
        let first_line = ((field.lineoffset + 1) * field.outlinelen).min(uphet.len());
        chroma_transient_improvement(
            &mut uphet[first_line..],
            field.outlinelen,
            noise_floor,
            spec.cti_width,
            spec.cti_mix,
        );
    }

    Ok(encode_chroma(&uphet))
}

// The TBC'd chroma, with the carrier re-measured from it under chroma AFC.
fn tracked_chroma(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    chroma_afc_state: &mut ChromaAfcState,
) -> Result<Vec<f32>> {
    // Color-framed chroma is resampled fsc / (2 pi color_under) * 4 samples
    // late to undo the group delay of the color-under heterodyne filtering.
    let shift = if color_framed(spec, field) {
        spec.sys_fsc_mhz * 1e6 / (std::f64::consts::TAU * spec.decoder_color_under_carrier) * 4.0
    } else {
        0.0
    };
    let chroma_downscaled = downscale_raw_vec(field, &spec.sinc_lut, true, shift, None)?;
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
pub(super) fn phase_step(a: Complex32, b: Complex32) -> f32 {
    let product = b * a.conj();
    atan2_precise(product.im, product.re)
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
    const SEPARATION_RANGE: std::ops::RangeInclusive<f32> = 90e3..=230e3;

    let fft = spec.secam_analytic_fft.as_ref()?;
    let pair_center = spec.decoder_color_under_carrier as f32;
    let freq_scale = (spec.tbc_sample_rate() / std::f64::consts::TAU) as f32;
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
        let mut f_inst: Vec<f32> = window
            .windows(2)
            .map(|pair| phase_step(pair[0], pair[1]) * freq_scale)
            .collect();
        let mut env: Vec<f32> = window.iter().map(|sample| sample.norm()).collect();
        freqs.push(median_from_values(&mut f_inst));
        envs.push(median_from_values(&mut env));
    }
    if freqs.is_empty() {
        return None;
    }

    // Ignore lines whose porch carrier is too weak to measure (dropouts,
    // colour killed lines).
    let env_threshold = median_from_values(&mut envs.clone()) * 0.25;
    let (mut low, mut high): (Vec<f32>, Vec<f32>) = freqs
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
    Some(f64::from((low_carrier + high_carrier) / 2.0 - pair_center))
}
