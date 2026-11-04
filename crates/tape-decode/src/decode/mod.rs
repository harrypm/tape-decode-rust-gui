use std::ops::Range;
use std::sync::Arc;

use crate::optimized::{
    atan2_precise, boxcar_same, exp_fast, f32_from_total_order_key, f32_total_order_key,
    powf_fast_nonneg, scale_field, select_ranks_by_key, sosfilt_f32, sosfiltfilt_f32,
    sum_algebraic, unwrap_angles, ScaleFieldParams, SincLut,
};
use crate::request::{ColorSystem, FieldOrderAction, LineSystem, WowInterpolation};
use crate::spec::DecoderSpec;
use num_traits::Float;
use realfft::{ComplexToReal, RealToComplex};
use rustfft::num_complex::Complex32;
use rustfft::Fft;
use sci_rs::signal::filter::design::Sos;
use serde::{Deserialize, Serialize};

mod chroma;
mod demodblock;
mod dropouts;
mod field;
mod secam;
mod sync;
mod vits;

use chroma::{color_framed, decode_chroma};
pub(crate) use chroma::ChromaFftFilter;
use demodblock::decode_video_block;
use dropouts::detect_dropouts_rf;
use field::predecode_field_from_rawdecode;
use vits::compute_vits_metrics;

pub(crate) fn iretohz(ire0: f32, hz_ire: f32, ire: f32) -> f32 {
    ire0 + (hz_ire * ire)
}

/// Video levels of the demodulated signal: the carrier frequency of 0 IRE and
/// the deviation per IRE. The decoder starts from the profile's nominal values
/// and the `--ire0-adjust` modes retune them from each field's sync pulses.
#[derive(Clone, Copy)]
struct VideoLevels {
    ire0: f32,
    hz_ire: f32,
}

impl VideoLevels {
    fn nominal(spec: &DecoderSpec) -> Self {
        Self {
            ire0: spec.sys_ire0,
            hz_ire: spec.sys_hz_ire,
        }
    }

    fn iretohz(self, ire: f32) -> f32 {
        iretohz(self.ire0, self.hz_ire, ire)
    }
}

fn inrange<T: PartialOrd>(a: T, mi: T, ma: T) -> bool {
    a >= mi && a <= ma
}

fn hz_to_output_array(
    spec: &DecoderSpec,
    input: &[f32],
    ire0: f32,
    hz_ire: f32,
    out_scale: f64,
) -> Vec<u16> {
    let out_scale = out_scale as f32;
    let scale = out_scale / hz_ire;
    let offset = spec.sys_output_zero as f32 - spec.sys_vsync_ire * out_scale - ire0 * scale;
    input
        .iter()
        .map(|&sample| (sample * scale + offset + 0.5).clamp(0.0, 65535.0) as u16)
        .collect()
}

fn y_comb(input: &[f32], line_len: usize, limit: f32) -> Vec<f32> {
    let len = input.len();
    if len == 0 {
        return Vec::new();
    }

    // The two delayed taps wrap at i = len - shift and i = shift respectively,
    // so cutting the walk at both points leaves segments where each tap is one
    // contiguous slice: no per-sample modulo, and the loops vectorize.
    let shift = line_len % len;
    let mut output = Vec::with_capacity(len);
    let mut emit = |range: std::ops::Range<usize>| {
        let back_start = (range.start + shift) % len;
        let fwd_start = (range.start + len - shift) % len;
        let current = &input[range.clone()];
        let back = &input[back_start..back_start + current.len()];
        let fwd = &input[fwd_start..fwd_start + current.len()];
        output.extend(
            current
                .iter()
                .zip(back)
                .zip(fwd)
                .map(|((&current, &back), &fwd)| {
                    let diffb = current - back;
                    let difff = current - fwd;
                    let clipped = (diffb + difff).clamp(-limit, limit);
                    current - clipped / 2.0
                }),
        );
    };
    let cut0 = shift.min(len - shift);
    let cut1 = shift.max(len - shift);
    emit(0..cut0);
    emit(cut0..cut1);
    emit(cut1..len);
    output
}

fn roundfloat(fl: f64, places: i64) -> f64 {
    let scale = 10.0f64.powi(places as i32);
    (fl * scale).round_ties_even() / scale
}

fn resolve_slice_bound(len: usize, index: isize) -> usize {
    if index >= 0 {
        usize::try_from(index).unwrap_or(usize::MAX).min(len)
    } else {
        len.saturating_sub(index.unsigned_abs())
    }
}

fn mean_slice<T>(values: &[T]) -> f64
where
    T: Float,
{
    if values.is_empty() {
        return 0.0;
    }
    // Lane accumulators so the conversion and sum vectorize instead of running
    // as one serial carried add over field-sized inputs.
    const LANES: usize = 8;
    let mut acc = [0.0f64; LANES];
    let mut chunks = values.chunks_exact(LANES);
    for chunk in &mut chunks {
        for (lane, &value) in acc.iter_mut().zip(chunk) {
            *lane += value.to_f64().unwrap();
        }
    }
    let tail: f64 = chunks
        .remainder()
        .iter()
        .map(|&value| value.to_f64().unwrap())
        .sum();
    (acc.iter().sum::<f64>() + tail) / values.len() as f64
}

fn median_from_values<T: Float>(values: &mut [T]) -> T {
    if values.is_empty() {
        return T::nan();
    }
    // Quickselect (O(n) avg) instead of a full sort: we only need the middle
    // order statistic(s), and selecting the k-th element yields the same value
    // a full sort would place at index k for the same comparator.
    let cmp = |a: &T, b: &T| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Greater);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        let (left, &mut hi, _) = values.select_nth_unstable_by(mid, cmp);
        // Lower middle element is the maximum of the partition below `mid`,
        // ranked by the same comparator used for selection.
        let lo = *left.iter().max_by(|a, b| cmp(a, b)).unwrap();
        (lo + hi) / (T::one() + T::one())
    } else {
        let (_, &mut median, _) = values.select_nth_unstable_by(mid, cmp);
        median
    }
}

/// Median of `values` (the mean of the two middle values for an even
/// count), found by radix counting: for the field-sized inputs this serves,
/// cheaper than selecting in a copy.
fn median_radix(values: &[f32]) -> f32 {
    let upper = values.len() / 2;
    if !values.len().is_multiple_of(2) {
        let [median] = select_ranks_by_key(values, f32_total_order_key, [upper]);
        return f32_from_total_order_key(median);
    }
    let [lower, upper] = select_ranks_by_key(values, f32_total_order_key, [upper - 1, upper]);
    (f32_from_total_order_key(lower) + f32_from_total_order_key(upper)) / 2.0
}

fn fft_in_place_f32(buffer: &mut [Complex32], fft: &dyn Fft<f32>, inverse: bool) {
    assert_eq!(buffer.len(), fft.len());
    if buffer.is_empty() {
        return;
    }

    fft.process(buffer);

    if inverse {
        let inv_scale = 1.0 / buffer.len() as f32;
        for sample in buffer {
            *sample *= inv_scale;
        }
    }
}

fn fft_real_f32(input: &[f32], forward_fft: &dyn Fft<f32>) -> Vec<Complex32> {
    let mut output = input
        .iter()
        .map(|&sample| Complex32::new(sample, 0.0))
        .collect::<Vec<_>>();
    fft_in_place_f32(&mut output, forward_fft, false);
    output
}

fn ifft_complex_owned_f32(
    mut buffer: Vec<Complex32>,
    inverse_fft: &dyn Fft<f32>,
) -> Vec<Complex32> {
    fft_in_place_f32(&mut buffer, inverse_fft, true);
    buffer
}

/// Analytic-signal spectrum from the `n/2 + 1` unique bins: keep DC and
/// Nyquist, double the interior bins, zero the negative frequencies.
fn analytic_spectrum(half: &[Complex32], n: usize) -> Vec<Complex32> {
    assert_eq!(half.len(), (n / 2) + 1);
    let mut spectrum = Vec::with_capacity(n);
    spectrum.push(half[0]);
    spectrum.extend(half[1..half.len() - 1].iter().map(|&bin| bin * 2.0));
    spectrum.push(half[half.len() - 1]);
    spectrum.resize(n, Complex32::new(0.0, 0.0));
    spectrum
}

/// The color-under carrier in Hz nearest the nominal one in the field's
/// narrowband-filtered chroma `data`: the strict local maximum of the power
/// spectrum, clipped from below at a third of its peak so only the lines
/// above that stand out, fine-tuned to steps of a fraction of the line
/// frequency. `None` when nothing stands out of the spectrum.
fn cafc_fft_center_freq(spec: &DecoderSpec, data: &[f32]) -> Option<f64> {
    let sig_fft = fft_real_f32(data, spec.fft_field_forward_f32.as_ref());
    // Real input, so only the DC..Nyquist half of the spectrum is searched.
    // The squared magnitudes stay f32: they only feed a local-peak search over
    // well-separated spectral lines.
    let half_bins = data.len() / 2 + 1;
    let power: Vec<f32> = sig_fft[..half_bins]
        .iter()
        .map(|sample| sample.re.mul_add(sample.re, sample.im * sample.im))
        .collect();
    let max_power = power.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let clip_min = (f64::from(max_power) * DecoderSpec::CHROMA_AFC_POWER_THRESHOLD) as f32;
    // Flattening everything below the threshold leaves only peaks above it as
    // local maxima.
    let clipped_power: Vec<f32> = power
        .iter()
        .map(|&value| value.clamp(clip_min, max_power))
        .collect();

    let time_step = 1.0 / (spec.sys_fsc_mhz * 4.0 * 1e6);
    let scale = 1.0 / (data.len() as f64 * time_step);

    let mut peak_index = None;
    let mut peak_delta = f64::INFINITY;
    for index in 1..half_bins - 1 {
        if clipped_power[index] > clipped_power[index - 1]
            && clipped_power[index] > clipped_power[index + 1]
        {
            let delta = (index as f64 * scale - spec.decoder_color_under_carrier).abs();
            if delta < peak_delta {
                peak_delta = delta;
                peak_index = Some(index);
            }
        }
    }

    let peak_freq = peak_index? as f64 * scale;
    let fine_tune_threshold = spec.chroma_afc_fh() * spec.chroma_afc_fine_tune_fh_ratio;
    Some(fine_tune_frequency(
        peak_freq,
        spec.decoder_color_under_carrier,
        fine_tune_threshold,
    ))
}

fn fine_tune_frequency(freq: f64, color_under: f64, max_step: f64) -> f64 {
    let specs_distance = |frequency: f64| (frequency - color_under).abs();
    let mut tune_freq = freq;
    while specs_distance(tune_freq) >= max_step {
        tune_freq -= if tune_freq > color_under {
            max_step
        } else {
            -max_step
        };
    }

    let one_step_more = tune_freq + max_step;
    let one_step_less = tune_freq - max_step;

    if specs_distance(tune_freq) < specs_distance(one_step_less)
        && specs_distance(tune_freq) < specs_distance(one_step_more)
    {
        tune_freq
    } else if specs_distance(one_step_more) < specs_distance(one_step_less) {
        one_step_more
    } else {
        one_step_less
    }
}

/// The field's color-under carrier in Hz under chroma AFC, measured from its
/// TBC'd chroma as the spectral peak nearest the nominal carrier after the
/// narrowband chain, held within the band tolerance; the nominal carrier when
/// the spectrum has no peak.
fn chroma_afc_carrier(spec: &DecoderSpec, chroma: &[f32]) -> f64 {
    let nominal = spec.decoder_color_under_carrier;
    let mut sections = spec.chroma_afc_narrowband.iter();
    let first = sections
        .next()
        .expect("chroma AFC without its narrowband chain");
    let filtered = sections.fold(sosfiltfilt_f32(first, chroma), |data, sos| {
        sosfiltfilt_f32(sos, &data)
    });
    let Some(measured) = cafc_fft_center_freq(spec, &filtered) else {
        tracing::warn!(
            "no color-under carrier peak in the chroma spectrum, taking the nominal carrier"
        );
        return nominal;
    };
    let (min_f, max_f) = spec.chroma_afc_band_tolerance();
    let carrier = measured.clamp(nominal * min_f, nominal * max_f);
    if carrier != measured {
        tracing::warn!(clipped = carrier, measured, "Chroma PLL range clipped");
    }
    tracing::debug!(carrier, "chroma AFC carrier");
    carrier
}

/// The heterodyne that brings a color-under carrier of `carrier_hz` up to
/// the subcarrier, over the field.
fn color_under_heterodyne(spec: &DecoderSpec, carrier_hz: f64) -> Vec<Vec<f32>> {
    let het_wave_scale = (spec.sys_fsc_mhz + carrier_hz / 1e6) / (spec.sys_fsc_mhz * 4.0);
    gen_chroma_heterodyne(het_wave_scale, 0.0, spec.fieldlen())
}

/// The four quadrature phases of a heterodyne carrier of `het_wave_scale`
/// cycles per sample starting at `phase_drift` radians: `-cos`, `sin`,
/// `cos` and `-sin` over `field_len` samples.
pub(crate) fn gen_chroma_heterodyne(
    het_wave_scale: f64,
    phase_drift: f64,
    field_len: usize,
) -> Vec<Vec<f32>> {
    use std::f64::consts::TAU;
    // The carrier is walked as LANES rotating phasors a sample apart, each
    // advancing LANES samples per rotation, so the rotations vectorize
    // instead of forming one dependency chain. Every CHUNK samples the first
    // phasor is re-seeded from an exact sine and cosine of the reduced angle
    // (the others follow by rotation), so the f64 rounding of the rotations
    // never accumulates past that.
    const CHUNK: usize = 1024;
    const LANES: usize = 8;
    let angle_step = TAU * het_wave_scale;
    let rotation = |steps: usize| {
        let (sin, cos) = (angle_step * steps as f64).sin_cos();
        (cos, sin)
    };
    let rotate = |(cos, sin): (f64, f64), (rot_cos, rot_sin): (f64, f64)| {
        (
            cos.mul_add(rot_cos, -(sin * rot_sin)),
            sin.mul_add(rot_cos, cos * rot_sin),
        )
    };
    let lane_rotation = rotation(LANES);
    let one_rotation = rotation(1);

    let mut phases: [Vec<f32>; 4] = std::array::from_fn(|_| Vec::with_capacity(field_len));
    let mut index = 0;
    while index < field_len {
        let (sin, cos) = angle_step
            .mul_add(index as f64, phase_drift)
            .rem_euclid(TAU)
            .sin_cos();
        let mut cosines = [0.0f64; LANES];
        let mut sines = [0.0f64; LANES];
        let mut phasor = (cos, sin);
        for lane in 0..LANES {
            (cosines[lane], sines[lane]) = phasor;
            phasor = rotate(phasor, one_rotation);
        }
        let chunk_end = field_len.min(index + CHUNK);
        while index < chunk_end {
            let count = LANES.min(chunk_end - index);
            let cos32 = cosines.map(|c| c as f32);
            let sin32 = sines.map(|s| s as f32);
            phases[0].extend(cos32[..count].iter().map(|c| -c));
            phases[1].extend_from_slice(&sin32[..count]);
            phases[2].extend_from_slice(&cos32[..count]);
            phases[3].extend(sin32[..count].iter().map(|s| -s));
            for lane in 0..LANES {
                (cosines[lane], sines[lane]) = rotate((cosines[lane], sines[lane]), lane_rotation);
            }
            index += count;
        }
    }
    phases.into()
}

fn rms<T>(samples: &[T]) -> f64
where
    T: Float,
{
    let len = samples.len() as f64;
    let mean = samples
        .iter()
        .map(|&sample| sample.to_f64().unwrap())
        .sum::<f64>()
        / len;
    let square_mean = samples
        .iter()
        .map(|&sample| {
            let centered = sample.to_f64().unwrap() - mean;
            centered * centered
        })
        .sum::<f64>()
        / len;
    square_mean.sqrt()
}

fn mean(samples: &[f64]) -> f64 {
    samples.iter().sum::<f64>() / samples.len() as f64
}

fn shift_chroma_and_remove_dc(mut output: Vec<f32>, move_by: isize) -> Vec<f32> {
    if output.is_empty() {
        return output;
    }

    roll(&mut output, move_by);

    let mean = sum_algebraic(&output) / output.len() as f32;
    for sample in output.iter_mut() {
        *sample -= mean;
    }

    output
}

/// Microseconds to input samples.
fn usectoinpx(spec: &DecoderSpec, x: f64) -> f64 {
    x * spec.freq
}

/// Microseconds to output samples.
fn usectooutpx(spec: &DecoderSpec, x: f64) -> f64 {
    x * spec.tbc_sample_rate() / 1e6
}

/// The 16-bit output code the demod frequency `input` is encoded as, at the
/// field output scale `out_scale`.
fn hz_to_output_scalar(spec: &DecoderSpec, input: f64, out_scale: f64) -> f64 {
    let mut reduced = (input - f64::from(spec.sys_ire0)) / f64::from(spec.sys_hz_ire);
    reduced -= f64::from(spec.sys_vsync_ire);
    (((reduced * out_scale) + spec.sys_output_zero as f64).clamp(0.0, 65535.0) + 0.5) as u16 as f64
}

fn sync_confidence_from_linelocs(field: &DecodedField) -> i64 {
    let linelocs = field
        .linelocs
        .as_ref()
        .expect("a valid field has line locations");
    let linecount = field.linecount.unwrap_or(0);
    let end = (field.lineoffset + linecount).min(linelocs.len());
    if end.saturating_sub(field.lineoffset) < 3 {
        return 100;
    }

    let mut lld2max = f64::NEG_INFINITY;
    for index in field.lineoffset..end - 2 {
        let lld2 = linelocs[index + 2] - (2.0 * linelocs[index + 1]) + linelocs[index];
        lld2max = lld2max.max(lld2);
    }

    if lld2max > 4.0 {
        45
    } else {
        100
    }
}

// Level of the `window` samples of each line of a picture of `outlinelen`
// samples per line: the mean of the middle third of the sorted per-line
// medians, which rejects lines hit by dropouts.
fn picture_line_level(picture_luma: &[f32], outlinelen: usize, window: &Range<usize>) -> f32 {
    let mut levels: Vec<f32> = picture_luma
        .chunks_exact(outlinelen)
        .map(|line| median_from_values(&mut line[window.clone()].to_vec()))
        .collect();
    levels.sort_by(f32::total_cmp);
    mean_slice(&levels[levels.len() / 3..(levels.len() * 2) / 3]) as f32
}

fn demod_slice_bounds(len: usize, start: i64, end: i64) -> Option<(usize, usize)> {
    let len = len as i64;
    let start = start.clamp(0, len) as usize;
    let end = end.clamp(0, len) as usize;
    (start < end).then_some((start, end))
}

fn demod_mean(data: &[f32], start: i64, end: i64) -> f32 {
    let Some((start, end)) = demod_slice_bounds(data.len(), start, end) else {
        return f32::NAN;
    };
    let slice = &data[start..end];
    slice.iter().sum::<f32>() / slice.len() as f32
}

/// Color burst of one line after the chroma heterodyne, fitted as
/// `amplitude * cos(2 pi fsc t - phase) + dc` against the reference
/// subcarrier, with `t` counted from the start of the field buffer.
#[derive(Clone, Copy)]
struct BurstMeasurement {
    /// Burst phase in degrees, [0, 360).
    phase: f32,
    amplitude: f32,
    /// Amplitude on the scale of the correlation sums (half the burst length
    /// times the amplitude), which the color killer threshold is set against.
    magnitude: f32,
    dc: f32,
}

#[derive(Clone, Copy)]
struct PhaseSequenceEntry {
    line_number: usize,
    /// Heterodyne phase row the line is upconverted with.
    heterodyne_phase: usize,
    burst: BurstMeasurement,
}

/// Field-wide result of locking the chroma heterodyne to the color burst.
#[derive(Clone)]
struct BurstLock {
    phase_sequence: Vec<PhaseSequenceEntry>,
    /// Color killer state, as reported in the `burstStartLine` metadata: -1
    /// when the whole field is blanked, 0 when the whole field has color,
    /// otherwise the line the burst switched on at.
    burst_start_line: i64,
}

pub const BLOCKSIZE: usize = 32 * 1024;
pub(crate) const BLOCKCUT: usize = 1024;
pub(crate) const BLOCKCUT_END: usize = 1024;
const DOD_MERGE_THRESHOLD: isize = 30;
const DOD_MIN_LENGTH: isize = 10;
const BADJ: f64 = -1.4;

#[derive(Clone)]
pub enum LumaOutput {
    Encoded(Vec<u16>),
    Raw(Vec<f32>),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct VitsMetrics {
    #[serde(rename = "wSNR", skip_serializing_if = "Option::is_none")]
    pub w_snr: Option<f64>,
    #[serde(rename = "bPSNR", skip_serializing_if = "Option::is_none")]
    pub b_psnr: Option<f64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DropOuts {
    pub field_line: Vec<usize>,
    pub startx: Vec<usize>,
    pub endx: Vec<usize>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldInfoEntry {
    pub is_first_field: bool,
    pub detected_first_field: bool,
    pub is_duplicate_field: bool,
    /// -1: color killer active for the whole field; 0: color for the whole
    /// field; n: color from field line n on.
    #[serde(default)]
    pub burst_start_line: i64,
    pub sync_conf: i64,
    /// 1-based position in the output, assigned when the field is placed in
    /// the sequence (see [`WriteableField::set_sequence_position`]).
    pub seq_no: usize,
    pub disk_loc: f32,
    pub file_loc: u64,
    #[serde(rename = "fieldPhaseID")]
    pub field_phase_id: i64,
    pub vits_metrics: VitsMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drop_outs: Option<DropOuts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_faults: Option<i64>,
}

#[derive(Clone)]
pub struct DecoderMetadata {
    pub system: &'static str,
    pub field_width: usize,
    pub sample_rate: f64,
    pub black_16b_ire: f64,
    pub white_16b_ire: f64,
    pub blanking_16b_ire: f64,
    pub field_height: usize,
    pub colour_burst_start: i64,
    pub colour_burst_end: i64,
    pub active_video_start: i64,
    pub active_video_end: i64,
}

#[derive(Clone)]
struct StackableMa<T> {
    window_average: usize,
    min_watermark: usize,
    stack: Vec<T>,
}

impl<T: Float + std::iter::Sum> StackableMa<T> {
    fn new(min_watermark: usize, window_average: usize) -> Self {
        Self {
            window_average,
            min_watermark,
            stack: Vec::new(),
        }
    }

    fn push(&mut self, value: T) {
        self.stack.push(value);
    }

    fn pull(&mut self) -> Option<T> {
        let keep_from = self.stack.len().saturating_sub(self.window_average);
        self.stack.drain(..keep_from);
        self.window_mean()
    }

    /// Mean of the newest `window_average` values (what `pull` trims to).
    fn window_mean(&self) -> Option<T> {
        let window = &self.stack[self.stack.len().saturating_sub(self.window_average)..];
        (!window.is_empty()).then(|| {
            let sum: T = window.iter().copied().sum();
            sum / T::from(window.len()).unwrap()
        })
    }

    fn has_values(&self) -> bool {
        self.stack.len() > self.min_watermark
    }
}

/// How far the ME-SECAM carrier servo may trim the conversion LO either
/// way, in Hz.
pub(crate) const SECAM_LO_TRIM_LIMIT_HZ: f64 = 10e3;

// Mutable state of the ME-SECAM chroma up-conversion: the carrier servo's
// smoothed rest carrier pair offset trims the conversion LO, and the
// heterodyne carries its phase on from field to field.
#[derive(Clone)]
struct ConversionLoState {
    servo_avg: StackableMa<f64>,
    /// Carrier phase at the start of the next field, in cycles.
    phase: f64,
}

impl ConversionLoState {
    fn new(spec: &DecoderSpec) -> Self {
        let mut servo_avg = StackableMa::new(2, 60);
        if let Some(seed) = spec.secam_lo_trim {
            // Pushed past the watermark so it applies from the first field;
            // the servo keeps adapting from it, or holds it with the servo off.
            for _ in 0..3 {
                servo_avg.push(seed);
            }
        }
        Self {
            servo_avg,
            phase: 0.0,
        }
    }

    /// The heterodyne of a field of `samples` output samples, with the LO
    /// trimmed by the servo's settled average, quantized to 10 Hz steps so
    /// measurement noise does not dither it. The carrier phase continues
    /// from where the previous field's ended, so a trim change only changes
    /// the frequency; a phase step in the restored FM carrier would be a
    /// chroma click to a SECAM decoder.
    fn heterodyne(&mut self, spec: &DecoderSpec, samples: usize) -> Vec<Vec<f32>> {
        let lo_trim = self
            .servo_avg
            .has_values()
            .then(|| self.servo_avg.pull())
            .flatten()
            .map_or(0.0, |avg| {
                ((avg / 10.0).round_ties_even() * 10.0)
                    .clamp(-SECAM_LO_TRIM_LIMIT_HZ, SECAM_LO_TRIM_LIMIT_HZ)
            });
        let conversion_lo = spec
            .decoder_chroma_conversion_lo
            .expect("conversion LO state without a conversion LO");
        let het_wave_scale = (conversion_lo + lo_trim) / spec.tbc_sample_rate();
        let heterodyne = gen_chroma_heterodyne(
            het_wave_scale,
            std::f64::consts::TAU * self.phase,
            spec.fieldlen(),
        );
        self.phase = (self.phase + het_wave_scale * samples as f64).rem_euclid(1.0);
        heterodyne
    }
}

fn roll<T>(values: &mut [T], shift: isize) {
    if values.is_empty() {
        return;
    }
    let len = values.len() as isize;
    let shift = shift.rem_euclid(len) as usize;
    values.rotate_right(shift);
}

#[derive(Clone)]
struct VideoChannels {
    demod: Vec<f32>,
    demod_05: Vec<f32>,
    demod_burst: Vec<f32>,
    envelope: Vec<f32>,
}

#[derive(Clone)]
struct FieldData {
    video: VideoChannels,
    startloc: u64,
    input_len: usize,
}

#[derive(Clone)]
struct PrevFieldState {
    burst_start_line: i64,
}

#[derive(Clone)]
struct InterFieldState {
    prev_first_hsync_readloc: i64,
    prev_first_hsync_loc: f64,
    prev_first_hsync_diff: f64,
    prev_first_field: i64,
    /// The chroma track phase the next field's burst lock starts from.
    track_phase: i64,
    compute_linelocs_issues: bool,
    /// The working video levels; with `--use-saved-levels` the next field is
    /// sliced between them instead of re-detecting its levels.
    levels: VideoLevels,
}

impl InterFieldState {
    fn new(spec: &DecoderSpec) -> Self {
        Self {
            prev_first_hsync_readloc: -1,
            prev_first_hsync_loc: -1.0,
            prev_first_hsync_diff: -1.0,
            prev_first_field: -1,
            track_phase: spec.track_phase.unwrap_or(0),
            compute_linelocs_issues: false,
            levels: VideoLevels::nominal(spec),
        }
    }
}

#[derive(Clone)]
struct DecodedField {
    data: FieldData,
    prevfield: Option<PrevFieldState>,
    readloc: u64,
    inlinelen: f64,
    outlinelen: usize,
    outlinecount: usize,
    wow_level_adjust_smoothing: f32,
    wow_interpolation_method: WowInterpolation,
    validpulses: Vec<sync::ValidPulseSample>,
    is_first_field: Option<bool>,
    vblank_next: Option<f64>,
    linelocs: Option<Vec<f64>>,
    lineoffset: usize,
    linecount: Option<usize>,
    out_scale: Option<f64>,
    field_phase_id: Option<i64>,
    valid: bool,
    /// The levels the field's line locations were refined with.
    levels: VideoLevels,
    burst_lock: Option<BurstLock>,
    /// The chroma track phase the burst lock read this field with, which
    /// selects the track IRE0 offset.
    track_phase: Option<i64>,
    /// The color-under carrier the chroma is heterodyned at, in Hz: measured
    /// from the field under chroma AFC, otherwise the format's nominal one.
    chroma_carrier: f64,
    /// The heterodyne at the measured carrier, under chroma AFC.
    chroma_heterodyne: Option<Vec<Vec<f32>>>,
    /// Cached `(median, mad)` of the field's wow-factor distribution. These
    /// depend only on the line-location spline, so they are identical for every
    /// channel; `downscale_raw_vec` fills this on the first call and reuses it.
    wow_analysis: Option<(f64, f64)>,
}

impl DecodedField {
    fn burst_start_line(&self) -> i64 {
        self.burst_lock
            .as_ref()
            .map_or(0, |lock| lock.burst_start_line)
    }

    /// Field line numbers of the output rows, in order.
    fn output_lines(&self) -> std::ops::Range<usize> {
        let first = self.lineoffset + 1;
        first..first + self.outlinecount
    }

    /// Output row of a field line.
    fn output_row(&self, line_number: usize) -> usize {
        line_number
            .checked_sub(self.lineoffset + 1)
            .expect("the line lies in the output rows")
    }

    /// Absolute input sample the first output row starts at: buffer sample
    /// `i` is input sample `startloc + BLOCKCUT + i`.
    fn output_start_sample(&self) -> f64 {
        let linelocs = self
            .linelocs
            .as_ref()
            .expect("a valid field has line locations");
        (self.data.startloc + BLOCKCUT as u64) as f64 + linelocs[self.output_lines().start]
    }

    /// The heterodyne the chroma is upconverted with: the field's own under
    /// chroma AFC, otherwise the format's nominal one.
    fn heterodyne<'a>(&'a self, spec: &'a DecoderSpec) -> &'a [Vec<f32>] {
        self.chroma_heterodyne
            .as_deref()
            .unwrap_or(&spec.rf_chroma_heterodyne)
    }
}

struct DecodeFieldResult {
    field: DecodedField,
    offset: f64,
}
#[derive(Clone)]
pub struct WriteableField {
    pub info: FieldInfoEntry,
    picture: Arc<WriteablePicture>,
    /// The signal-derived field phase ID, or `None` when `info.field_phase_id`
    /// follows the field's position in the output instead (see
    /// [`Self::set_sequence_position`]).
    field_phase_id_raw: Option<i64>,
    /// Set when the chroma was phase-compensated onto the NTSC color-frame
    /// sequence: the second color frame is then written with the chroma
    /// inverted.
    color_framed: bool,
}

struct WriteablePicture {
    luma: LumaOutput,
    chroma: Option<Vec<u16>>,
}

/// Whether the field at 1-based output position `seq_no` falls in the
/// second frame of the NTSC four-field color sequence. The output starts on
/// a first field and alternates, so its fields pair into frames from the
/// first one.
fn ntsc_second_color_frame(seq_no: usize) -> bool {
    seq_no.div_ceil(2).is_multiple_of(2)
}

/// fieldPhaseID of a field without a signal-derived one, from its written
/// order and output position: first fields odd, second fields even, 3 and 4
/// in the second color frame.
fn sequence_field_phase_id(first_field: bool, seq_no: usize) -> i64 {
    match (first_field, ntsc_second_color_frame(seq_no)) {
        (true, false) => 1,
        (false, false) => 2,
        (true, true) => 3,
        (false, true) => 4,
    }
}

impl WriteableField {
    fn new(
        info: FieldInfoEntry,
        luma: LumaOutput,
        chroma: Option<Vec<u16>>,
        field: &DecodedField,
        color_framed: bool,
    ) -> Self {
        Self {
            info,
            picture: Arc::new(WriteablePicture { luma, chroma }),
            field_phase_id_raw: field.field_phase_id,
            color_framed,
        }
    }

    /// Number the field for its position in the output and derive the
    /// metadata that follows from it: the decoder numbers each field as it
    /// emits it, and the multithreaded stitch renumbers a worker's fields
    /// onto the global sequence.
    pub fn set_sequence_position(&mut self, seq_no: usize) {
        self.info.seq_no = seq_no;
        self.info.field_phase_id = self
            .field_phase_id_raw
            .unwrap_or_else(|| sequence_field_phase_id(self.info.is_first_field, seq_no));
    }

    #[inline]
    pub fn luma(&self) -> &LumaOutput {
        &self.picture.luma
    }

    /// The chroma as decoded: color-framed fields carry the first color
    /// frame's phase regardless of their position.
    #[inline]
    pub fn chroma(&self) -> Option<&[u16]> {
        self.picture.chroma.as_deref()
    }

    /// The chroma as it is to be written: inverted on the second color frame.
    pub fn chroma_output(&self) -> Option<std::borrow::Cow<'_, [u16]>> {
        let chroma = self.chroma()?;
        Some(
            if self.color_framed && ntsc_second_color_frame(self.info.seq_no) {
                chroma.iter().copied().map(chroma::invert).collect()
            } else {
                std::borrow::Cow::Borrowed(chroma)
            },
        )
    }
}

#[derive(Clone, Copy)]
struct MetadataFieldState {
    out_scale: f64,
    outlinecount: usize,
}

/// One phase of the rational cubic resampler: the integer tap base for output
/// `i` and the four Catmull-Rom weights (the fractional offset narrows to f32
/// before the weight polynomial).
fn resample_phase_taps(input_rate: usize, output_rate: usize, i: usize) -> (isize, [f32; 4]) {
    let scale = input_rate as f64 / output_rate as f64;
    let pos = (i % output_rate) as f64 * scale;
    let idx_off = pos.floor();
    let f = (pos - idx_off) as f32;
    let f2 = f * f;
    let f3 = f2 * f;
    let weights = [
        -0.5 * f3 + f2 - 0.5 * f,
        1.5 * f3 - 2.5 * f2 + 1.0,
        -1.5 * f3 + 2.0 * f2 + 0.5 * f,
        0.5 * f3 - 0.5 * f2,
    ];
    let base = (i / output_rate * input_rate) as isize + idx_off as isize;
    (base, weights)
}

pub(crate) struct ChromaSepClass {
    ratio_den: usize,
    /// Offset of the first fused tap relative to the output index.
    win_lo: isize,
    /// Fused tap weights, one tap-offset row of `ratio_den` phase entries each.
    fused_taps: Vec<Vec<f32>>,
}

impl ChromaSepClass {
    pub(crate) fn new(fs: f64, fsc: f64) -> Self {
        let multiplier = 8usize;
        let delay = multiplier / 2;
        let fscx = (fsc * multiplier as f64 * 1e6) as usize;
        let (ratio_num, ratio_den) = limit_denominator(fscx as f64 / fs, 1000);

        // The chain this implements — cubic upsample to multiplier*fsc, the
        // half-cycle comb average, cubic downsample back — is linear and
        // periodic with the output phase `i % ratio_den`, so it composes into
        // one short FIR per phase over a contiguous input window. Probe each
        // phase far enough into an imagined infinite stream that no composed
        // index wraps, and accumulate the products of the stage weights per
        // input tap; the offsets relative to the output index are
        // phase-periodic by construction.
        let probe_cycle = delay + 2;
        let mut per_phase: Vec<std::collections::BTreeMap<isize, f64>> =
            Vec::with_capacity(ratio_den);
        for p in 0..ratio_den {
            let i = p + probe_cycle * ratio_den;
            let mut taps = std::collections::BTreeMap::new();
            let (down_base, down_w) = resample_phase_taps(ratio_num, ratio_den, i);
            for (k, &dw) in down_w.iter().enumerate() {
                let j = down_base - 1 + k as isize;
                for branch in [0, delay as isize] {
                    let (up_base, up_w) =
                        resample_phase_taps(ratio_den, ratio_num, (j - branch) as usize);
                    for (m, &uw) in up_w.iter().enumerate() {
                        let xi = up_base - 1 + m as isize - i as isize;
                        *taps.entry(xi).or_insert(0.0) += f64::from(dw) * 0.5 * f64::from(uw);
                    }
                }
            }
            per_phase.push(taps);
        }
        let win_lo = per_phase
            .iter()
            .filter_map(|taps| taps.keys().next().copied())
            .min()
            .unwrap_or(0);
        let win_hi = per_phase
            .iter()
            .filter_map(|taps| taps.keys().next_back().copied())
            .max()
            .unwrap_or(0);
        let width = (win_hi - win_lo + 1) as usize;
        let mut fused_taps = vec![vec![0.0f32; ratio_den]; width];
        for (p, taps) in per_phase.iter().enumerate() {
            for (&offset, &coeff) in taps {
                fused_taps[(offset - win_lo) as usize][p] = coeff as f32;
            }
        }
        Self {
            ratio_den,
            win_lo,
            fused_taps,
        }
    }

    // It resamples the luminance data to self.multiplier * fsc, applies the
    // comb filter, then resamples it back — evaluated as the precomposed
    // per-phase FIR. The few outputs whose window leaves the buffer (where
    // the old chain clamped or wrapped) use clamped taps; they sit well
    // inside the discarded block-cut margins.
    fn work(&self, luminance: &[f32]) -> Vec<f32> {
        let len = luminance.len();
        let den = self.ratio_den;
        let width = self.fused_taps.len();
        let win_lo = self.win_lo;

        let edge_at = |i: usize| -> f32 {
            let p = i % den;
            let mut acc = 0.0f32;
            for (u, taps) in self.fused_taps.iter().enumerate() {
                acc += taps[p] * sample_clamped(luminance, i as isize + win_lo + u as isize);
            }
            acc
        };

        let head_end = ((-win_lo).max(0) as usize).min(len);
        let tail_start = (len as isize - win_lo - width as isize + 1)
            .clamp(head_end as isize, len as isize) as usize;

        let mut out = Vec::with_capacity(len);
        out.extend((0..head_end).map(edge_at));
        // The interior walks whole phase cycles; every tap row pairs a
        // contiguous weight run with a contiguous input window, so the
        // accumulation passes vectorize with no per-sample indexing.
        let mut i = head_end;
        while i < tail_start {
            let p = i % den;
            let run = (tail_start - i).min(den - p);
            let x0 = (i as isize + win_lo) as usize;
            let start = out.len();
            out.extend(
                self.fused_taps[0][p..p + run]
                    .iter()
                    .zip(&luminance[x0..x0 + run])
                    .map(|(&w, &x)| w * x),
            );
            let dst = &mut out[start..start + run];
            for (u, taps) in self.fused_taps.iter().enumerate().skip(1) {
                let xs = &luminance[x0 + u..x0 + u + run];
                for ((acc, &w), &x) in dst.iter_mut().zip(&taps[p..p + run]).zip(xs) {
                    *acc += w * x;
                }
            }
            i += run;
        }
        out.extend((tail_start..len).map(edge_at));
        out
    }
}

fn limit_denominator(x: f64, max_den: usize) -> (usize, usize) {
    let mut best_num = 0usize;
    let mut best_den = 1usize;
    let mut best_err = f64::INFINITY;
    for den in 1..=max_den {
        let num = (x * den as f64).round() as usize;
        let err = (x - num as f64 / den as f64).abs();
        if err < best_err {
            best_err = err;
            best_num = num;
            best_den = den;
        }
    }
    (best_num, best_den)
}

fn sample_clamped(data: &[f32], idx: isize) -> f32 {
    data[idx.clamp(0, data.len() as isize - 1) as usize]
}

/// TBC of one channel of the field onto the output grid, resampled `shift`
/// input samples late. With a `line_window` only that many output samples
/// at the start of every line are resampled, the rest left zero.
fn downscale_raw_vec(
    field: &mut DecodedField,
    lut: &SincLut,
    use_burst_channel: bool,
    shift: f64,
    line_window: Option<usize>,
) -> Vec<f32> {
    let actual_linelocs = field
        .linelocs
        .as_deref()
        .expect("a valid field has line locations");
    let outwidth = field.outlinelen;
    let k = field.wow_interpolation_method.spline_degree();
    let out_len = field.outlinecount * outwidth;
    let expected_linelocs = (0..actual_linelocs.len())
        .map(|i| i as f64 * field.inlinelen)
        .collect::<Vec<_>>();
    let outscale = field.inlinelen / field.outlinelen as f64;
    let outsamples = field.outlinecount * field.outlinelen;
    let outline_offset = (field.lineoffset + 1) * field.outlinelen;
    let eval_count = outsamples + outline_offset;
    let (dsout, median, mad) = {
        let channel = if use_burst_channel {
            field.data.video.demod_burst.as_slice()
        } else {
            field.data.video.demod.as_slice()
        };
        scale_field(
            channel,
            lut,
            out_len,
            &expected_linelocs,
            actual_linelocs,
            k,
            ScaleFieldParams {
                eval_scale: outscale,
                eval_count,
                lineoffset: field.lineoffset,
                outwidth,
                wow_level_adjust_smoothing: field.wow_level_adjust_smoothing,
                level_adjust_threshold: 15.0,
                cached_median_mad: field.wow_analysis,
                shift,
                line_window: line_window.unwrap_or(outwidth),
            },
        )
    };
    field.wow_analysis = Some((median, mad));
    dsout
}

/// What the field-order check keeps of a decoded field.
#[derive(Clone, Copy)]
struct DecodedFieldOrder {
    detected_first_field: bool,
    file_loc: u64,
}

/// The decode so far as the field-order check sees it: the written
/// sequence, whose count numbers the output and whose last field the next
/// one must alternate with, and the last two decoded fields, whose detected
/// orders tell progressive material from a skipped field. A duplicate
/// written in place of a skipped field is no decoded field and stays out of
/// the latter.
#[derive(Default)]
struct OutputSequence {
    len: usize,
    /// The order of the last written field.
    last_written: Option<bool>,
    /// Newest first.
    decoded: [Option<DecodedFieldOrder>; 2],
}

impl OutputSequence {
    fn written(&mut self, is_first_field: bool) {
        self.last_written = Some(is_first_field);
        self.len += 1;
    }

    fn decoded(&mut self, field: DecodedFieldOrder) {
        self.decoded = [Some(field), self.decoded[0]];
    }
}

// Minimal state needed to progress a decode: the immutable spec, the running
// input offset, and the speculative-predecode/field-ordering memory.
pub struct Decoder {
    spec: Arc<DecoderSpec>,
    fdoffset: u64,
    fdoffset_frac: f64,
    inter_field_state: InterFieldState,
    conversion_lo: Option<ConversionLoState>,
    output: OutputSequence,
    metadata_field: Option<MetadataFieldState>,
    lastvalidfield: Vec<Option<WriteableField>>,
    pending_result: Option<DecodeFieldResult>,
    has_pending: bool,
    duplicate_prev_field: bool,
    /// Set while the detected field order runs one field out of step with
    /// the written sequence (a skipped field under `FieldOrderAction::None`):
    /// the written order is then the detected one inverted.
    order_inverted: bool,
}

impl Decoder {
    pub fn new(spec: Arc<DecoderSpec>, fdoffset: u64) -> Self {
        let inter_field_state = InterFieldState::new(&spec);
        let conversion_lo = spec
            .chroma_phase_continuous()
            .then(|| ConversionLoState::new(&spec));
        Self {
            spec,
            fdoffset,
            fdoffset_frac: 0.0,
            inter_field_state,
            conversion_lo,
            output: OutputSequence::default(),
            metadata_field: None,
            lastvalidfield: vec![None, None],
            pending_result: None,
            has_pending: false,
            duplicate_prev_field: true,
            order_inverted: false,
        }
    }

    /// Decode as many fields as the window `data` allows. `data` is a window
    /// of the input samples starting at absolute sample offset `data_start`;
    /// `final_chunk` is set only when it reaches the true end of input.
    /// Returns the absolute offset before which input is no longer needed
    /// (so the caller can drop or skip past it) together with the fields
    /// produced. When more input is needed mid-field, decoding pauses with
    /// state intact so a later call with an extended window resumes it
    /// bit-identically.
    ///
    /// The window must start at or before the first sample the decoder
    /// reads: the offset the previous call returned, or for the first call
    /// the one a call with an empty window reports.
    pub fn decode(
        &mut self,
        data: &[f32],
        data_start: u64,
        final_chunk: bool,
    ) -> (u64, Vec<WriteableField>) {
        let data_start = data_start as usize;
        let readlen = self.spec.readlen();
        let blocksize = BLOCKSIZE;
        let usable_blocksize = self.spec.usable_blocksize();
        let bytes_per_field = self.spec.bytes_per_field();

        let mut output: Vec<WriteableField> = Vec::new();

        let mut done = false;
        while !done {
            let mut field_done = false;
            let mut picture_luma: Option<LumaOutput> = None;
            let mut picture_chroma: Option<Vec<u16>> = None;
            let mut field: Option<DecodedField> = None;

            while !field_done {
                let (decoded_field, decoded_offset) = if !self.has_pending {
                    (None, Some(0.0))
                } else if let Some(pending) = self.pending_result.take() {
                    (Some(pending.field), Some(pending.offset))
                } else {
                    (None, None)
                };

                let scheduled_prevfield = decoded_field
                    .as_ref()
                    .filter(|field_obj| field_obj.valid)
                    .map(|field_obj| PrevFieldState {
                        burst_start_line: field_obj.burst_start_line(),
                    });
                let pending_frac = self.fdoffset_frac + decoded_offset.unwrap_or(0.0);
                let scheduled_readloc_value = (self.fdoffset as i64 - BLOCKCUT as i64
                    + pending_frac.floor() as i64)
                    .max(0) as u64;
                let readloc_block = scheduled_readloc_value as usize / blocksize;
                let numblocks = (readlen / blocksize) + 2;
                let block_begin = readloc_block * blocksize;
                let block_end = block_begin + (numblocks * blocksize);
                let requested_begin = block_begin / usable_blocksize;
                let requested_end = (block_end / usable_blocksize) + 1;

                // The field needs blocks [requested_begin, requested_end), each a
                // BLOCKSIZE window at absolute offset `b * usable_blocksize`. If the
                // current window does not cover all of them and more input may still
                // arrive, pause here: hand the pending field back untouched and
                // report `requested_begin`'s block as the earliest offset still
                // needed. The blocks already in hand are not decoded now, as the
                // resumed call decodes the whole set again.
                let needed_end = (requested_end - 1) * usable_blocksize + BLOCKSIZE;
                if needed_end > data_start + data.len() && !final_chunk {
                    if let Some(field) = decoded_field {
                        self.pending_result = Some(DecodeFieldResult {
                            field,
                            offset: decoded_offset.unwrap_or(0.0),
                        });
                    }
                    return ((requested_begin * usable_blocksize) as u64, output);
                }

                // Every block contributes exactly `usable_blocksize` samples per
                // channel, so the field buffers are sized up front and each
                // block appends straight into them.
                let field_capacity = (requested_end - requested_begin) * usable_blocksize;
                let mut video = VideoChannels {
                    demod: Vec::with_capacity(field_capacity),
                    demod_05: Vec::with_capacity(field_capacity),
                    demod_burst: Vec::with_capacity(field_capacity),
                    envelope: Vec::with_capacity(field_capacity),
                };
                let mut completed_blocks = true;
                assert!(
                    data_start <= requested_begin * usable_blocksize,
                    "the data window starts at sample {data_start}, past the first needed block at {}",
                    requested_begin * usable_blocksize
                );
                for b in requested_begin..requested_end {
                    // Only decode a full BLOCKSIZE window; a short tail at the true
                    // end of input leaves the sequence incomplete and ends decoding.
                    let start = b * usable_blocksize - data_start;
                    let Some(rawdata) = data.get(start..start + BLOCKSIZE) else {
                        completed_blocks = false;
                        break;
                    };
                    decode_video_block(rawdata, &self.spec, &mut video);
                }
                self.pending_result = if completed_blocks {
                    let rawdecode = FieldData {
                        startloc: ((block_begin / usable_blocksize) * usable_blocksize) as u64,
                        input_len: video.demod.len(),
                        video,
                    };
                    Some(predecode_field_from_rawdecode(
                        rawdecode,
                        &self.spec,
                        scheduled_prevfield,
                        &mut self.inter_field_state,
                        scheduled_readloc_value,
                    ))
                } else {
                    None
                };
                self.has_pending = true;

                if decoded_field.is_some() {
                    let advanced = self.fdoffset_frac + decoded_offset.unwrap_or(0.0);
                    let whole = advanced.floor();
                    self.fdoffset = self.fdoffset.saturating_add_signed(whole as i64);
                    self.fdoffset_frac = advanced - whole;
                }

                let decoded_was_none = decoded_field.is_none();
                field = decoded_field;

                if let Some(mut field_obj) = field.take() {
                    if field_obj.valid {
                        // Predecode populated the wow-analysis cache before
                        // the burst refinement of the line locations, so it is
                        // stale now. Drop it; the luma pass below recomputes
                        // with the final ones and the chroma pass then reuses
                        // that.
                        field_obj.wow_analysis = None;
                        let mut luma = downscale_raw_vec(
                            &mut field_obj,
                            &self.spec.sinc_lut,
                            false,
                            0.0,
                            None,
                        );
                        if self.spec.rf_y_comb != 0.0 {
                            luma = y_comb(&luma, field_obj.outlinelen, self.spec.rf_y_comb);
                        }

                        if self.spec.rf_export_raw_tbc {
                            picture_luma = Some(LumaOutput::Raw(luma));
                        } else {
                            let mut ire0 = self.spec.sys_ire0;
                            let mut hz_ire = self.spec.sys_hz_ire;
                            let adjust = self.spec.rf_ire0_adjust;
                            if luma.len() == field_obj.outlinecount * field_obj.outlinelen {
                                let windows = &self.spec.ire0_adjust_windows;
                                if adjust.backporch {
                                    ire0 = picture_line_level(
                                        &luma,
                                        field_obj.outlinelen,
                                        &windows.back_porch,
                                    );
                                    tracing::debug!(ire0, "calculated ire0");
                                }
                                if adjust.hsync {
                                    // The sync tip sits vsync_ire below ire0, so
                                    // its measured distance rescales hz_ire.
                                    let hsync_level = picture_line_level(
                                        &luma,
                                        field_obj.outlinelen,
                                        &windows.sync_tip,
                                    );
                                    hz_ire = (ire0 - hsync_level) / -self.spec.sys_vsync_ire;
                                    tracing::debug!(hz_ire, "calculated hz_ire");
                                    // A dropout or sync loss can put the same
                                    // level in both windows.
                                    if !hz_ire.is_finite() || hz_ire == 0.0 {
                                        tracing::warn!(
                                            ire0,
                                            hsync_level,
                                            "degenerate hz_ire from the hsync level, keeping the working one"
                                        );
                                        hz_ire = field_obj.levels.hz_ire;
                                    }
                                }
                            }
                            if let Some(track_phase) = field_obj.track_phase {
                                ire0 += self.spec.sys_track_ire0_offset[track_phase as usize];
                            }
                            picture_luma = Some(LumaOutput::Encoded(hz_to_output_array(
                                &self.spec,
                                &luma,
                                ire0,
                                hz_ire,
                                field_obj.out_scale.unwrap(),
                            )));
                        }
                        self.metadata_field = Some(MetadataFieldState {
                            out_scale: field_obj.out_scale.unwrap(),
                            outlinecount: field_obj.outlinecount,
                        });

                        picture_chroma =
                            decode_chroma(&mut field_obj, &self.spec, self.conversion_lo.as_mut());

                        field_done = true;
                    }
                    field = Some(field_obj);
                }
                if decoded_was_none && decoded_offset.is_none() {
                    field = None;
                    break;
                }
            }

            let Some(field_obj) = field else {
                done = true;
                continue;
            };

            let detected_first_field = field_obj.is_first_field.unwrap_or(false);
            let file_loc = field_obj.output_start_sample().round() as u64;
            // The output starts on a first field.
            if self.output.len != 0 || detected_first_field {
                // How many fields an invalid stretch lost is unknown, so the
                // inversion established before it no longer holds.
                if field_obj.prevfield.is_none() {
                    self.order_inverted = false;
                }
                let mut is_first_field = detected_first_field ^ self.order_inverted;
                // A field of the other order written before this one.
                let mut duplicate: Option<WriteableField> = None;
                let mut sync_conf = sync_confidence_from_linelocs(&field_obj);
                let disk_loc = roundfloat(file_loc as f64 / bytes_per_field as f64, 1);

                let color_framed = color_framed(&self.spec, &field_obj);
                let mut write_field = true;

                // Dropout detection.
                let mut drop_outs: Option<DropOuts> = None;
                if self.spec.do_dod {
                    let (dropout_lines, dropout_starts, dropout_ends) = detect_dropouts_rf(
                        &self.spec,
                        &field_obj,
                        DOD_MERGE_THRESHOLD,
                        DOD_MIN_LENGTH,
                    );
                    if !dropout_lines.is_empty() {
                        drop_outs = Some(DropOuts {
                            field_line: dropout_lines,
                            startx: dropout_starts,
                            endx: dropout_ends,
                        });
                    }
                }

                // Decode-fault bitmap.
                let mut decode_faults = 0i64;
                let vits_metrics = compute_vits_metrics(
                    &self.spec,
                    &field_obj,
                    picture_luma.as_ref().expect("a valid field has its luma"),
                );

                // Interlaced video requires alternating fields. Handle cases where
                // fields repeat (recording breaks, progressive content, etc.).
                if self.output.last_written == Some(is_first_field) {
                    let [previous, before_previous] = self.output.decoded;
                    let previous = previous.expect("a written field was decoded");
                    // The spacing of this and the previous decoded field's
                    // first output lines, in fields.
                    let distance_from_previous_field =
                        (file_loc as f64 - previous.file_loc as f64) / bytes_per_field as f64;
                    // Three consecutive fields detected with one order are
                    // progressive material, written alternating.
                    if previous.detected_first_field == detected_first_field
                        && before_previous
                            .is_some_and(|field| field.detected_first_field == detected_first_field)
                        && inrange(distance_from_previous_field, 0.9, 1.1)
                    {
                        tracing::warn!("Detected progressive video content..., manually flipping the field order to compensate");
                        decode_faults |= 1;
                        sync_conf = 10;
                        is_first_field = !is_first_field;
                    } else {
                        match self.spec.field_order_action {
                            FieldOrderAction::Duplicate => self.duplicate_prev_field = true,
                            FieldOrderAction::Drop => self.duplicate_prev_field = false,
                            FieldOrderAction::Detect => {
                                if distance_from_previous_field > 1.1 {
                                    self.duplicate_prev_field = true;
                                } else if distance_from_previous_field < 0.9 {
                                    self.duplicate_prev_field = false;
                                } else {
                                    self.duplicate_prev_field = !self.duplicate_prev_field;
                                }
                            }
                            FieldOrderAction::None => {}
                        }
                        // Every same-order outcome marks a skipped-field fault and
                        // zeroes confidence; the branches differ only in the remedy.
                        decode_faults |= 4;
                        sync_conf = 0;
                        if self.spec.field_order_action == FieldOrderAction::None {
                            tracing::warn!("Possibly skipped field (Two fields with same isFirstField in a row), inverting the detected field order to compensate");
                            self.order_inverted = !self.order_inverted;
                            is_first_field = !is_first_field;
                        } else if self.duplicate_prev_field {
                            duplicate = self.lastvalidfield[usize::from(!is_first_field)].clone();
                            if duplicate.is_some() {
                                tracing::warn!("Possibly skipped field (Two fields with same isFirstField in a row), duplicating the last field of the other order to compensate...");
                            } else {
                                tracing::warn!("Possibly skipped field (Two fields with same isFirstField in a row), no field of the other order to duplicate, writing the field as it is");
                            }
                        } else {
                            tracing::warn!("Possibly skipped field (Two fields with same isFirstField in a row), dropping the last field to compensate...");
                            write_field = false;
                        }
                    }
                }

                let info = FieldInfoEntry {
                    is_first_field,
                    detected_first_field,
                    is_duplicate_field: false,
                    burst_start_line: field_obj.burst_start_line(),
                    sync_conf,
                    // Numbered when emitted.
                    seq_no: 0,
                    disk_loc: disk_loc as f32,
                    file_loc,
                    field_phase_id: 0,
                    vits_metrics,
                    drop_outs,
                    decode_faults: (decode_faults != 0).then_some(decode_faults),
                };

                if write_field {
                    let dataset = WriteableField::new(
                        info,
                        picture_luma.take().expect("a valid field has its luma"),
                        picture_chroma.take(),
                        &field_obj,
                        color_framed,
                    );
                    // Kept by written order: the duplicate written before a
                    // repeated field is the latest field of the other order.
                    self.lastvalidfield[usize::from(is_first_field)] = Some(dataset.clone());
                    if let Some(mut other) = duplicate {
                        other.info.is_duplicate_field = true;
                        self.emit(&mut output, other);
                    }
                    self.emit(&mut output, dataset);
                }
            }
            self.output.decoded(DecodedFieldOrder {
                detected_first_field,
                file_loc,
            });
        }

        (self.fdoffset, output)
    }

    /// Place `field` at the next position of the output sequence.
    fn emit(&mut self, output: &mut Vec<WriteableField>, mut field: WriteableField) {
        field.set_sequence_position(self.output.len + 1);
        self.output.written(field.info.is_first_field);
        output.push(field);
    }

    /// The ME-SECAM carrier servo's current estimate of the conversion LO
    /// trim in Hz (the unquantized average of its recent rest carrier pair
    /// measurements), or `None` without any. A calibration decode reads this
    /// to seed the real decode.
    pub fn secam_lo_trim(&self) -> Option<f64> {
        self.conversion_lo
            .as_ref()
            .and_then(|lo| lo.servo_avg.window_mean())
    }

    pub fn metadata(&self) -> Option<DecoderMetadata> {
        let spec: &DecoderSpec = &self.spec;
        let metadata_field = self.metadata_field?;
        let code_of_ire = |ire: f32| {
            hz_to_output_scalar(
                spec,
                f64::from(iretohz(spec.sys_ire0, spec.sys_hz_ire, ire)),
                metadata_field.out_scale,
            )
        };
        // The levels are set in the 16-bit output's codes, where the level
        // adjustment widens black and white; the raw export, written in
        // hertz, has them mapped back through IRE to hertz.
        let level = |code: f64| {
            if spec.rf_export_raw_tbc {
                let ire = (code - spec.sys_output_zero as f64) / metadata_field.out_scale
                    + f64::from(spec.sys_vsync_ire);
                f64::from(spec.sys_ire0) + f64::from(spec.sys_hz_ire) * ire
            } else {
                code
            }
        };
        let black = level(code_of_ire(spec.black_ire as f32) * (1.0 - spec.level_adjust as f64));
        let white = level(code_of_ire(100.0) * (1.0 + spec.level_adjust as f64));
        let blanking = level(code_of_ire(0.0));
        let to_sample = |us: f64| (usectooutpx(spec, us) + BADJ).round_ties_even() as i64;
        let system = match (spec.sys_frame_lines, spec.color_system) {
            (LineSystem::Line525, ColorSystem::Pal) => "PAL-M",
            (LineSystem::Line525, _) => "NTSC",
            _ => "PAL",
        };
        Some(DecoderMetadata {
            system,
            field_width: spec.sys_outlinelen,
            sample_rate: spec.tbc_sample_rate(),
            black_16b_ire: black,
            white_16b_ire: white,
            blanking_16b_ire: blanking,
            field_height: metadata_field.outlinecount,
            colour_burst_start: to_sample(spec.sys_color_burst_us[0]),
            colour_burst_end: to_sample(spec.sys_color_burst_us[1]),
            active_video_start: to_sample(spec.sys_active_video_us[0]),
            active_video_end: to_sample(spec.sys_active_video_us[1]),
        })
    }
}
