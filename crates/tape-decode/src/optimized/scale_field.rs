use super::select_ranks_by_key;
use anyhow::{bail, Result};

// Full-field wow factors are large; fixed-point keeps the buffer at 4 bytes per
// sample without losing the precision needed by the level-adjust pass.
const WOW_FACTOR_SCALE: f64 = 100_000_000.0;

fn pack_wow_factor(value: f64) -> i32 {
    ((value - 1.0) * WOW_FACTOR_SCALE)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn unpack_wow_factor(value: f64) -> f64 {
    1.0 + value / WOW_FACTOR_SCALE
}

/// Tuning inputs shared by `scale_field` and every `scale_field_k` instantiation.
/// Bundled so the degree dispatch forwards a single value instead of repeating
/// the full argument list once per spline degree.
#[derive(Clone, Copy)]
pub(crate) struct ScaleFieldParams {
    pub eval_scale: f64,
    pub eval_count: usize,
    pub lineoffset: usize,
    pub outwidth: usize,
    pub wow_level_adjust_smoothing: f32,
    pub level_adjust_threshold: f64,
    pub cached_median_mad: Option<(f64, f64)>,
    /// Source samples added to every output sample's position: a positive
    /// shift resamples the channel that much later.
    pub shift: f64,
}

pub(crate) fn scale_field(
    buf: &[f32],
    lut: &SincLut,
    out_len: usize,
    expected_linelocs: &[f64],
    actual_linelocs: &[f64],
    k: usize,
    params: ScaleFieldParams,
) -> Result<(Vec<f32>, f64, f64)> {
    let (interp_t, interp_c) = make_interp_spline_scaled(expected_linelocs, actual_linelocs, k)?;
    let spline = (interp_t.as_slice(), interp_c.as_slice());
    Ok(match k {
        1 => scale_field_linear(buf, lut, out_len, spline, &params),
        2 => scale_field_k::<2>(buf, lut, out_len, spline, &params),
        3 => scale_field_k::<3>(buf, lut, out_len, spline, &params),
        _ => unreachable!("unsupported spline degree"),
    })
}

fn level_adjust_threshold_value(level_adjust_threshold: f64, mad: f64) -> f64 {
    if mad > 0.0 {
        level_adjust_threshold * mad
    } else {
        0.001 * WOW_FACTOR_SCALE
    }
}

fn level_adjusted(packed: i32, median: f64, threshold: f64) -> f64 {
    if (packed as f64 - median).abs() > threshold {
        unpack_wow_factor(median)
    } else {
        unpack_wow_factor(packed as f64)
    }
}

// Kaiser beta trades sharpness for ringing: a smaller value narrows the main
// lobe but attenuates the side lobes less.
const KAISER_BETA: f64 = 5.0;
const SINC_TAPS: usize = 16;
// Taps before and from the integer part of the source coordinate.
const SINC_TAPS_BEFORE: usize = SINC_TAPS / 2 - 1;
const SINC_TAPS_AFTER: usize = SINC_TAPS - SINC_TAPS_BEFORE;
// Enough phases for the interpolation between adjacent rows to be exact at f32
// resolution while the table stays cache resident.
const SINC_PHASES: usize = 1 << 12;

// One phase's weights; 64-byte aligned so a row is exactly one cache line.
#[repr(align(64))]
#[derive(Clone, Copy)]
struct SincRow([f32; SINC_TAPS]);

/// Windowed-sinc resampling weights tabulated per sub-sample phase, each row
/// normalized to unit sum. The extra row for phase 1.0 lets the lerp between
/// adjacent rows run without a bounds check on the upper row.
pub(crate) struct SincLut {
    rows: Vec<SincRow>,
}

impl SincLut {
    pub(crate) fn new() -> Self {
        let i0_beta = bessel_i0(KAISER_BETA);
        let half_width = (SINC_TAPS / 2) as f64;
        let row = |phase: f64| {
            let mut weights = [0.0f64; SINC_TAPS];
            for (tap, weight) in weights.iter_mut().enumerate() {
                // Signed distance from the output position to this tap.
                let x = tap as f64 - SINC_TAPS_BEFORE as f64 - phase;
                *weight = sinc(x) * kaiser(x, half_width, KAISER_BETA, i0_beta);
            }
            let sum: f64 = weights.iter().sum();
            SincRow(weights.map(|weight| (weight / sum) as f32))
        };
        // The kernel is even, so phase 1 - p is phase p with the taps reversed.
        let mut rows = vec![SincRow([0.0; SINC_TAPS]); SINC_PHASES + 1];
        for index in 0..=SINC_PHASES / 2 {
            let lower = row(index as f64 / SINC_PHASES as f64);
            let mut upper = lower.0;
            upper.reverse();
            rows[index] = lower;
            rows[SINC_PHASES - index] = SincRow(upper);
        }
        Self { rows }
    }

    /// Source coordinate range whose tap window stays inside a buffer of
    /// `len` samples.
    fn coord_bounds(len: usize) -> (f64, f64) {
        assert!(len >= SINC_TAPS);
        (SINC_TAPS_BEFORE as f64, (len - SINC_TAPS_AFTER) as f64)
    }

    /// Resample `buf` at `coord`, which must lie within `coord_bounds`.
    #[inline(always)]
    fn resample(&self, buf: &[f32], coord: f64) -> f32 {
        let coord_int = coord as usize;
        // The phase is split in f64 so the lower row index stays below
        // SINC_PHASES even when the fraction is within an f32 ulp of 1.
        let phase_pos = (coord - coord_int as f64) * SINC_PHASES as f64;
        let phase = phase_pos as usize;
        let alpha = (phase_pos - phase as f64) as f32;
        let start = coord_int - SINC_TAPS_BEFORE;
        let window = buf[start..start + SINC_TAPS].try_into().unwrap();
        sinc_dot(window, &self.rows[phase].0, &self.rows[phase + 1].0, alpha)
    }

    /// The tap window start, weight row and lerp fraction of each coordinate,
    /// split exactly as `resample` splits one.
    #[inline(always)]
    fn decompose(coords: &[f64; RESAMPLE_BATCH]) -> [(usize, usize, f32); RESAMPLE_BATCH] {
        coords.map(|coord| {
            let coord_int = coord as usize;
            let phase_pos = (coord - coord_int as f64) * SINC_PHASES as f64;
            let phase = phase_pos as usize;
            let alpha = (phase_pos - phase as f64) as f32;
            (coord_int - SINC_TAPS_BEFORE, phase, alpha)
        })
    }

    /// Resample `buf` at each coordinate, all within `coord_bounds`. With SIMD
    /// the batch's dot products are reduced together through one transpose
    /// tree, which pairs the partial sums in the order `sinc_dot` reduces a
    /// single window, so the outputs equal `resample`'s.
    #[inline(always)]
    fn resample_batch(
        &self,
        buf: &[f32],
        coords: &[f64; RESAMPLE_BATCH],
        out: &mut [f32; RESAMPLE_BATCH],
    ) {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
        {
            use std::arch::x86_64::*;
            let taps = Self::decompose(coords);
            // SAFETY: the windows and rows are checked slices of SINC_TAPS
            // floats; the stores cover `out` exactly.
            unsafe {
                let mut p = [_mm512_setzero_ps(); RESAMPLE_BATCH];
                for (product, &(start, phase, alpha)) in p.iter_mut().zip(&taps) {
                    let x = _mm512_loadu_ps(buf[start..start + SINC_TAPS].as_ptr());
                    let a = _mm512_loadu_ps(self.rows[phase].0.as_ptr());
                    let b = _mm512_loadu_ps(self.rows[phase + 1].0.as_ptr());
                    let w = _mm512_fmadd_ps(_mm512_sub_ps(b, a), _mm512_set1_ps(alpha), a);
                    *product = _mm512_mul_ps(x, w);
                }
                // Each stage halves the vector count by adding the two halves
                // (256, 128, 64 then 32 bits wide) of every output's partials,
                // interleaving two outputs per vector as it goes.
                let mut q = [_mm512_setzero_ps(); 8];
                for (k, q) in q.iter_mut().enumerate() {
                    let (a, b) = (p[2 * k], p[2 * k + 1]);
                    *q = _mm512_add_ps(
                        _mm512_shuffle_f32x4::<0x44>(a, b),
                        _mm512_shuffle_f32x4::<0xEE>(a, b),
                    );
                }
                let mut r = [_mm512_setzero_ps(); 4];
                for (k, r) in r.iter_mut().enumerate() {
                    let (a, b) = (q[2 * k], q[2 * k + 1]);
                    *r = _mm512_add_ps(
                        _mm512_shuffle_f32x4::<0x88>(a, b),
                        _mm512_shuffle_f32x4::<0xDD>(a, b),
                    );
                }
                let mut s = [_mm512_setzero_ps(); 2];
                for (k, s) in s.iter_mut().enumerate() {
                    let (a, b) = (r[2 * k], r[2 * k + 1]);
                    *s = _mm512_add_ps(
                        _mm512_shuffle_ps::<0x44>(a, b),
                        _mm512_shuffle_ps::<0xEE>(a, b),
                    );
                }
                let t = _mm512_add_ps(
                    _mm512_shuffle_ps::<0x88>(s[0], s[1]),
                    _mm512_shuffle_ps::<0xDD>(s[0], s[1]),
                );
                // Element 4m + c of `t` is output m + 4c.
                let order = _mm512_setr_epi32(0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15);
                _mm512_storeu_ps(out.as_mut_ptr(), _mm512_permutexvar_ps(order, t));
            }
        }
        #[cfg(all(
            target_arch = "x86_64",
            target_feature = "avx2",
            target_feature = "fma",
            not(target_feature = "avx512f")
        ))]
        {
            use std::arch::x86_64::*;
            let taps = Self::decompose(coords);
            // SAFETY: the windows and rows are checked slices of SINC_TAPS
            // floats, loaded at offsets 0 and 8; the stores cover `out` exactly.
            unsafe {
                for (half, out) in out.chunks_exact_mut(8).enumerate() {
                    let mut p = [_mm256_setzero_ps(); 8];
                    for (product, &(start, phase, alpha)) in
                        p.iter_mut().zip(&taps[half * 8..half * 8 + 8])
                    {
                        let window = buf[start..start + SINC_TAPS].as_ptr();
                        let (w0, w1) =
                            (self.rows[phase].0.as_ptr(), self.rows[phase + 1].0.as_ptr());
                        let alpha = _mm256_set1_ps(alpha);
                        let mut acc = _mm256_setzero_ps();
                        for offset in [0, 8] {
                            let x = _mm256_loadu_ps(window.add(offset));
                            let a = _mm256_loadu_ps(w0.add(offset));
                            let b = _mm256_loadu_ps(w1.add(offset));
                            let w = _mm256_fmadd_ps(_mm256_sub_ps(b, a), alpha, a);
                            acc = _mm256_add_ps(acc, _mm256_mul_ps(x, w));
                        }
                        *product = acc;
                    }
                    let mut q = [_mm256_setzero_ps(); 4];
                    for (k, q) in q.iter_mut().enumerate() {
                        let (a, b) = (p[2 * k], p[2 * k + 1]);
                        *q = _mm256_add_ps(
                            _mm256_permute2f128_ps::<0x20>(a, b),
                            _mm256_permute2f128_ps::<0x31>(a, b),
                        );
                    }
                    let mut s = [_mm256_setzero_ps(); 2];
                    for (k, s) in s.iter_mut().enumerate() {
                        let (a, b) = (q[2 * k], q[2 * k + 1]);
                        *s = _mm256_add_ps(
                            _mm256_shuffle_ps::<0x44>(a, b),
                            _mm256_shuffle_ps::<0xEE>(a, b),
                        );
                    }
                    let t = _mm256_add_ps(
                        _mm256_shuffle_ps::<0x88>(s[0], s[1]),
                        _mm256_shuffle_ps::<0xDD>(s[0], s[1]),
                    );
                    // Element 4l + c of `t` is output 2c + l.
                    let order = _mm256_setr_epi32(0, 4, 1, 5, 2, 6, 3, 7);
                    _mm256_storeu_ps(out.as_mut_ptr(), _mm256_permutevar8x32_ps(t, order));
                }
            }
        }
        #[cfg(not(any(
            all(target_arch = "x86_64", target_feature = "avx512f"),
            all(
                target_arch = "x86_64",
                target_feature = "avx2",
                target_feature = "fma"
            )
        )))]
        {
            for (out, &coord) in out.iter_mut().zip(coords) {
                *out = self.resample(buf, coord);
            }
        }
    }
}

/// Outputs the batched resampler produces per call.
const RESAMPLE_BATCH: usize = 16;

/// `sum(window[t] * lerp(w0[t], w1[t], alpha))` over the taps. Explicit SIMD
/// where the build has it: the compiler bundles the equivalent array code only
/// two lanes wide, as the final add's two operands fix its vector width.
#[inline(always)]
fn sinc_dot(
    window: &[f32; SINC_TAPS],
    w0: &[f32; SINC_TAPS],
    w1: &[f32; SINC_TAPS],
    alpha: f32,
) -> f32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    {
        use std::arch::x86_64::*;
        // SAFETY: each load reads 16 floats from an array of that length.
        unsafe {
            let x = _mm512_loadu_ps(window.as_ptr());
            let a = _mm512_loadu_ps(w0.as_ptr());
            let b = _mm512_loadu_ps(w1.as_ptr());
            let w = _mm512_fmadd_ps(_mm512_sub_ps(b, a), _mm512_set1_ps(alpha), a);
            _mm512_reduce_add_ps(_mm512_mul_ps(x, w))
        }
    }
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx2",
        target_feature = "fma",
        not(target_feature = "avx512f")
    ))]
    {
        use std::arch::x86_64::*;
        // SAFETY: each load reads 8 floats at offset 0 or 8 of a 16-float array.
        unsafe {
            let alpha = _mm256_set1_ps(alpha);
            let mut acc = _mm256_setzero_ps();
            for offset in [0, 8] {
                let x = _mm256_loadu_ps(window.as_ptr().add(offset));
                let a = _mm256_loadu_ps(w0.as_ptr().add(offset));
                let b = _mm256_loadu_ps(w1.as_ptr().add(offset));
                let w = _mm256_fmadd_ps(_mm256_sub_ps(b, a), alpha, a);
                acc = _mm256_add_ps(acc, _mm256_mul_ps(x, w));
            }
            let quad = _mm_add_ps(_mm256_castps256_ps128(acc), _mm256_extractf128_ps(acc, 1));
            let pair = _mm_add_ps(quad, _mm_movehl_ps(quad, quad));
            _mm_cvtss_f32(_mm_add_ss(pair, _mm_shuffle_ps(pair, pair, 1)))
        }
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512f"),
        all(
            target_arch = "x86_64",
            target_feature = "avx2",
            target_feature = "fma"
        )
    )))]
    {
        let mut acc = [0.0f32; SINC_TAPS];
        for t in 0..SINC_TAPS {
            acc[t] = window[t] * (w1[t] - w0[t]).mul_add(alpha, w0[t]);
        }
        for t in 0..8 {
            acc[t] += acc[t + 8];
        }
        for t in 0..4 {
            acc[t] += acc[t + 4];
        }
        for t in 0..2 {
            acc[t] += acc[t + 2];
        }
        acc[0] + acc[1]
    }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x_pi = std::f64::consts::PI * x;
    x_pi.sin() / x_pi
}

fn kaiser(x: f64, half_width: f64, beta: f64, i0_beta: f64) -> f64 {
    let r = x / half_width;
    if r.abs() > 1.0 {
        return 0.0;
    }
    bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta
}

// Modified Bessel function of the first kind, order zero, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let quarter_sq = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while term > sum * f64::EPSILON {
        term *= quarter_sq / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// One knot span of the degree-1 spline, with everything that is constant
/// across the span hoisted out: the value lerp endpoints and the (constant)
/// derivative.
#[derive(Clone, Copy)]
struct LinSpan {
    t0: f64,
    t1: f64,
    inv_h: f64,
    c0: f64,
    c1: f64,
    deriv: f64,
}

fn lin_span(t: &[f64], c: &[f64], s: usize) -> LinSpan {
    let t0 = t[s];
    let t1 = t[s + 1];
    let inv_h = 1.0 / (t1 - t0);
    let c0 = c[s - 1];
    let c1 = c[s];
    LinSpan {
        t0,
        t1,
        inv_h,
        c0,
        c1,
        deriv: (c1 - c0) * inv_h,
    }
}

/// First eval index in `[lo, hi]` whose sample position `idx * eval_scale`
/// reaches `bound`. The position is monotone in the index, so the fix-up walks
/// land on the exact crossover even when the seeding division rounds.
fn first_index_reaching(bound: f64, eval_scale: f64, lo: usize, hi: usize) -> usize {
    let mut idx = ((bound / eval_scale) as usize).clamp(lo, hi);
    while idx > lo && (idx - 1) as f64 * eval_scale >= bound {
        idx -= 1;
    }
    while idx < hi && (idx as f64) * eval_scale < bound {
        idx += 1;
    }
    idx
}

/// Value holding the given 0-based rank in a run-length encoded sorted list.
fn rank_value<T: Copy>(pairs: &[(T, u32)], rank: usize) -> T {
    let mut before = 0usize;
    for &(value, count) in pairs {
        let next = before + count as usize;
        if next > rank {
            return value;
        }
        before = next;
    }
    unreachable!("rank outside run-length counts")
}

/// Median (mean of the two middle order statistics for an even total) of a
/// run-length encoded multiset, matching `radix_median_by_key` exactly.
fn weighted_median<T: Copy + Ord>(
    pairs: &mut [(T, u32)],
    total: usize,
    decode: impl Fn(T) -> f64,
) -> f64 {
    assert!(total > 0);
    pairs.sort_unstable_by_key(|pair| pair.0);
    let upper_index = total / 2;
    let upper = decode(rank_value(pairs, upper_index));
    if !total.is_multiple_of(2) {
        return upper;
    }
    let lower = decode(rank_value(pairs, upper_index - 1));
    (lower + upper) / 2.0
}

/// Degree-1 (default) specialization of `scale_field_k`. The linear spline has
/// a constant derivative inside each knot span, so the wow factor, its packed
/// form, and the level-adjust threshold decision are all span-invariant: hoist
/// them and walk the output span by span. The median/MAD gather collapses the
/// same way, from one spline evaluation per output sample to one
/// `(value, count)` run per span fed to an exact weighted median.
fn scale_field_linear(
    buf: &[f32],
    lut: &SincLut,
    out_len: usize,
    spline: (&[f64], &[f64]),
    params: &ScaleFieldParams,
) -> (Vec<f32>, f64, f64) {
    const K: usize = 1;
    let (coord_lo, coord_hi) = SincLut::coord_bounds(buf.len());
    let ScaleFieldParams {
        eval_scale,
        eval_count,
        lineoffset,
        outwidth,
        wow_level_adjust_smoothing,
        level_adjust_threshold,
        cached_median_mad,
        shift,
    } = *params;
    let lineoffset_out_samples = outwidth * (lineoffset + 1);
    let required_eval_count = lineoffset_out_samples + out_len;
    assert!(required_eval_count <= eval_count);

    let (t, c) = spline;
    let nt = t.len() - K - 1;

    let (median, mad) = cached_median_mad.unwrap_or_else(|| {
        assert!(eval_count > 0);
        // Each span contributes one packed wow factor, repeated once per eval
        // index that falls inside it — the same multiset the per-sample walk
        // produced, so the medians are bit-identical.
        let mut pairs: Vec<(i32, u32)> = Vec::with_capacity(nt);
        let mut idx = 0usize;
        for span in K..nt {
            if idx >= eval_count {
                break;
            }
            let sp = lin_span(t, c, span);
            let end = if span + 1 < nt {
                first_index_reaching(sp.t1, eval_scale, idx, eval_count)
            } else {
                eval_count
            };
            if end > idx {
                pairs.push((pack_wow_factor(sp.deriv), (end - idx) as u32));
                idx = end;
            }
        }
        let median = weighted_median(&mut pairs, eval_count, |value| value as f64);
        let mut diff_pairs: Vec<(u32, u32)> = pairs
            .iter()
            .map(|&(value, count)| (wow_factor_abs_diff(value, median), count))
            .collect();
        let mad = weighted_median(&mut diff_pairs, eval_count, f64::from);
        (median, mad)
    });

    let threshold = level_adjust_threshold_value(level_adjust_threshold, mad);
    let smoothing_enabled = wow_level_adjust_smoothing > 0.0;
    let (alpha, one_minus_alpha) = if smoothing_enabled {
        let alpha = 1.0 / (f64::from(wow_level_adjust_smoothing) * outwidth as f64);
        (alpha, 1.0 - alpha)
    } else {
        (0.0, 0.0)
    };

    // Warmup smoothing over the lead-in samples. This walk is a tiny fraction
    // of the field, so it keeps the straightforward per-sample evaluation.
    let mut warmup_span = K;
    let initial = eval_spline_value_deriv_1(t, c, nt, &mut warmup_span, 0.0).1;
    let mut smoothed_adjust = level_adjusted(pack_wow_factor(initial), median, threshold);
    if smoothing_enabled && lineoffset_out_samples > 1 {
        for index in 1..lineoffset_out_samples {
            let deriv =
                eval_spline_value_deriv_1(t, c, nt, &mut warmup_span, index as f64 * eval_scale).1;
            smoothed_adjust = alpha.mul_add(
                level_adjusted(pack_wow_factor(deriv), median, threshold),
                one_minus_alpha * smoothed_adjust,
            );
        }
    }

    if outwidth == 0 {
        return (Vec::new(), median, mad);
    }

    let mut dsout = Vec::with_capacity(out_len);
    let mut span = K;
    let end_eval = lineoffset_out_samples + out_len;
    let mut idx = lineoffset_out_samples;
    while idx < end_eval {
        // Re-derive the span exactly the way the per-sample evaluation did,
        // then run every sample that stays inside it with the hoisted state.
        let x = idx as f64 * eval_scale;
        if x <= t[K] {
            span = K;
        } else if x >= t[nt] {
            span = nt - 1;
        } else {
            while span + 1 < nt && x >= t[span + 1] {
                span += 1;
            }
        }
        let sp = lin_span(t, c, span);
        let run_end = if span + 1 < nt {
            first_index_reaching(sp.t1, eval_scale, idx + 1, end_eval)
        } else {
            end_eval
        };
        let raw_adjust = level_adjusted(pack_wow_factor(sp.deriv), median, threshold);
        let alpha_raw = alpha * raw_adjust;
        // Clamp the source position into the interior so the tap window stays
        // in bounds. Valid fields never reach the edge, so this is a no-op for
        // normal output; it only catches degenerate line locations.
        // `.max().min()` (not `clamp`) also folds a NaN coordinate to the low
        // edge.
        let coord_at = |eval_index: usize| {
            let x = eval_index as f64 * eval_scale;
            let b0 = (sp.t1 - x) * sp.inv_h;
            let b1 = (x - sp.t0) * sp.inv_h;
            (b0.mul_add(sp.c0, b1 * sp.c1) + shift)
                .max(coord_lo)
                .min(coord_hi)
        };
        let next_adjust = |smoothed_adjust: &mut f64| {
            if smoothing_enabled {
                *smoothed_adjust = one_minus_alpha.mul_add(*smoothed_adjust, alpha_raw);
                *smoothed_adjust
            } else {
                raw_adjust
            }
        };
        let mut eval_index = idx;
        while run_end - eval_index >= RESAMPLE_BATCH {
            let coords: [f64; RESAMPLE_BATCH] = std::array::from_fn(|j| coord_at(eval_index + j));
            let adjusts: [f32; RESAMPLE_BATCH] =
                std::array::from_fn(|_| next_adjust(&mut smoothed_adjust) as f32);
            let mut samples = [0.0f32; RESAMPLE_BATCH];
            lut.resample_batch(buf, &coords, &mut samples);
            dsout.extend(adjusts.iter().zip(&samples).map(|(&a, &v)| a * v));
            eval_index += RESAMPLE_BATCH;
        }
        for eval_index in eval_index..run_end {
            let level_adjust = next_adjust(&mut smoothed_adjust);
            dsout.push(level_adjust as f32 * lut.resample(buf, coord_at(eval_index)));
        }
        idx = run_end;
    }
    (dsout, median, mad)
}

/// One knot span of a degree-`K` spline as the polynomial of `u = x - t0`
/// that the spline reduces to inside it: `value[j]` and `deriv[j]` are the
/// coefficients of `u^j`. Taylor expanding the span's basis functions at its
/// start is exact, as they are polynomials there, and leaves each output
/// sample a short Horner evaluation instead of the basis recurrence.
#[derive(Clone, Copy)]
struct SpanPoly {
    t0: f64,
    t1: f64,
    value: [f64; 4],
    deriv: [f64; 4],
}

impl SpanPoly {
    fn new<const K: usize>(t: &[f64], c: &[f64], span: usize) -> Self {
        let t0 = t[span];
        let ders = bspline_ders_basis::<K>(t, span, t0, K);
        let cofs = &c[span - K..=span];
        let mut value = [0.0; 4];
        let mut deriv = [0.0; 4];
        let mut factorial = 1.0;
        for order in 0..=K {
            let d: f64 = ders[order].iter().zip(cofs).map(|(b, cj)| b * cj).sum();
            value[order] = d / factorial;
            if order > 0 {
                deriv[order - 1] = d / factorial * order as f64;
            }
            factorial *= (order + 1) as f64;
        }
        Self {
            t0,
            t1: t[span + 1],
            value,
            deriv,
        }
    }

    #[inline(always)]
    fn eval<const K: usize>(&self, x: f64) -> (f64, f64) {
        let u = x - self.t0;
        let mut value = self.value[K];
        let mut deriv = self.deriv[K - 1];
        let mut order = K;
        while order > 0 {
            order -= 1;
            value = value.mul_add(u, self.value[order]);
            if order > 0 {
                deriv = deriv.mul_add(u, self.deriv[order - 1]);
            }
        }
        (value, deriv)
    }
}

/// The knot span `x` evaluates in, continuing from `span`: the spline's edge
/// pieces extend past the first and last knots.
#[inline(always)]
fn advance_span<const K: usize>(t: &[f64], nt: usize, span: &mut usize, x: f64) {
    if x <= t[K] {
        *span = K;
    } else if x >= t[nt] {
        *span = nt - 1;
    } else {
        while *span + 1 < nt && x >= t[*span + 1] {
            *span += 1;
        }
    }
}

/// Walks the eval indices `[start, end)` span by span, handing each run of
/// consecutive indices inside one span to `run` with the span's polynomial.
fn for_each_span_run<const K: usize>(
    t: &[f64],
    c: &[f64],
    eval_scale: f64,
    start: usize,
    end: usize,
    mut run: impl FnMut(&SpanPoly, std::ops::Range<usize>),
) {
    let nt = t.len() - K - 1;
    let mut span = K;
    let mut idx = start;
    while idx < end {
        advance_span::<K>(t, nt, &mut span, idx as f64 * eval_scale);
        let poly = SpanPoly::new::<K>(t, c, span);
        let run_end = if span + 1 < nt {
            first_index_reaching(poly.t1, eval_scale, idx + 1, end)
        } else {
            end
        };
        run(&poly, idx..run_end);
        idx = run_end;
    }
}

fn scale_field_k<const K: usize>(
    buf: &[f32],
    lut: &SincLut,
    out_len: usize,
    spline: (&[f64], &[f64]),
    params: &ScaleFieldParams,
) -> (Vec<f32>, f64, f64) {
    let (coord_lo, coord_hi) = SincLut::coord_bounds(buf.len());
    let ScaleFieldParams {
        eval_scale,
        eval_count,
        lineoffset,
        outwidth,
        wow_level_adjust_smoothing,
        level_adjust_threshold,
        cached_median_mad,
        shift,
    } = *params;
    let lineoffset_out_samples = outwidth * (lineoffset + 1);
    let required_eval_count = lineoffset_out_samples + out_len;
    assert!(required_eval_count <= eval_count);

    let (t, c) = spline;

    // The wow-factor median/MAD depend only on the line-location spline, so the
    // caller caches them across a field's channels. When not cached, evaluate
    // the spline derivative once per index to gather the distribution and take
    // the median and MAD.
    let (median, mad) = cached_median_mad.unwrap_or_else(|| {
        let mut wow_packed: Vec<i32> = Vec::with_capacity(eval_count);
        for_each_span_run::<K>(t, c, eval_scale, 0, eval_count, |poly, range| {
            wow_packed.extend(
                range.map(|index| pack_wow_factor(poly.eval::<K>(index as f64 * eval_scale).1)),
            );
        });
        let median = median_wow_factors(&wow_packed);
        (median, median_wow_factor_abs_diff(&wow_packed, median))
    });

    let threshold = level_adjust_threshold_value(level_adjust_threshold, mad);
    let adjusted = |value: i32| -> f64 { level_adjusted(value, median, threshold) };
    let smoothing_enabled = wow_level_adjust_smoothing > 0.0;
    let (alpha, one_minus_alpha) = if smoothing_enabled {
        let alpha = 1.0 / (f64::from(wow_level_adjust_smoothing) * outwidth as f64);
        (alpha, 1.0 - alpha)
    } else {
        (0.0, 0.0)
    };

    // Warmup smoothing over the lead-in samples.
    let mut smoothed_adjust = adjusted(pack_wow_factor(
        SpanPoly::new::<K>(t, c, K).eval::<K>(0.0).1,
    ));
    if smoothing_enabled {
        for_each_span_run::<K>(
            t,
            c,
            eval_scale,
            1,
            lineoffset_out_samples,
            |poly, range| {
                for index in range {
                    let raw_adjust =
                        adjusted(pack_wow_factor(poly.eval::<K>(index as f64 * eval_scale).1));
                    smoothed_adjust = alpha.mul_add(raw_adjust, one_minus_alpha * smoothed_adjust);
                }
            },
        );
    }

    if outwidth == 0 {
        return (Vec::new(), median, mad);
    }

    let mut dsout = Vec::with_capacity(out_len);
    let end_eval = lineoffset_out_samples + out_len;
    for_each_span_run::<K>(
        t,
        c,
        eval_scale,
        lineoffset_out_samples,
        end_eval,
        |poly, range| {
            // Clamp the source position into the interior so the tap window stays
            // in bounds. Valid fields never reach the edge, so this is a no-op for
            // normal output; it only catches degenerate line locations.
            // `.max().min()` (not `clamp`) also folds a NaN coordinate to the low
            // edge.
            let coord_adjust = |eval_index: usize, smoothed_adjust: &mut f64| {
                let (coord, deriv) = poly.eval::<K>(eval_index as f64 * eval_scale);
                let raw_adjust = adjusted(pack_wow_factor(deriv));
                let level_adjust = if smoothing_enabled {
                    *smoothed_adjust =
                        alpha.mul_add(raw_adjust, one_minus_alpha * *smoothed_adjust);
                    *smoothed_adjust
                } else {
                    raw_adjust
                };
                let coord = (coord + shift).max(coord_lo).min(coord_hi);
                (coord, level_adjust as f32)
            };
            let mut eval_index = range.start;
            while range.end - eval_index >= RESAMPLE_BATCH {
                let mut coords = [0.0f64; RESAMPLE_BATCH];
                let mut adjusts = [0.0f32; RESAMPLE_BATCH];
                for (j, (coord, adjust)) in coords.iter_mut().zip(&mut adjusts).enumerate() {
                    (*coord, *adjust) = coord_adjust(eval_index + j, &mut smoothed_adjust);
                }
                let mut samples = [0.0f32; RESAMPLE_BATCH];
                lut.resample_batch(buf, &coords, &mut samples);
                dsout.extend(adjusts.iter().zip(&samples).map(|(&a, &v)| a * v));
                eval_index += RESAMPLE_BATCH;
            }
            for eval_index in eval_index..range.end {
                let (coord, level_adjust) = coord_adjust(eval_index, &mut smoothed_adjust);
                dsout.push(level_adjust * lut.resample(buf, coord));
            }
        },
    );
    (dsout, median, mad)
}

const WOW_FACTOR_I32_ORDER_BIAS: u32 = 0x8000_0000;

/// Median (mean of the two middle order statistics for an even count) of
/// `packed` under the order-preserving `key`, with `decode` mapping an ordered
/// `u32` back to its real value.
fn radix_median_by_key<K, D>(packed: &[i32], key: K, decode: D) -> f64
where
    K: Fn(i32) -> u32 + Copy,
    D: Fn(u32) -> f64,
{
    let count = packed.len();
    assert!(count > 0);
    let upper_index = count / 2;
    if !count.is_multiple_of(2) {
        let [upper] = select_ranks_by_key(packed, key, [upper_index]);
        return decode(upper);
    }
    let [lower, upper] = select_ranks_by_key(packed, key, [upper_index - 1, upper_index]);
    (decode(lower) + decode(upper)) / 2.0
}

fn median_wow_factors(packed: &[i32]) -> f64 {
    radix_median_by_key(
        packed,
        |value| (value as u32) ^ WOW_FACTOR_I32_ORDER_BIAS,
        |ordered| ((ordered ^ WOW_FACTOR_I32_ORDER_BIAS) as i32) as f64,
    )
}

fn wow_factor_abs_diff(value: i32, median: f64) -> u32 {
    (value as f64 - median).abs().round() as u32
}

fn median_wow_factor_abs_diff(packed: &[i32], median: f64) -> f64 {
    radix_median_by_key(
        packed,
        |value| wow_factor_abs_diff(value, median),
        |ordered| ordered as f64,
    )
}

/// Value and derivative of the degree-1 spline at `x`, continuing from
/// `span`. Its two basis functions are straight lines, so this is a lerp.
fn eval_spline_value_deriv_1(
    t: &[f64],
    c: &[f64],
    nt: usize,
    span: &mut usize,
    x: f64,
) -> (f64, f64) {
    advance_span::<1>(t, nt, span, x);
    let s = *span;
    let inv_h = 1.0 / (t[s + 1] - t[s]);
    let b0 = (t[s + 1] - x) * inv_h;
    let b1 = (x - t[s]) * inv_h;
    let c0 = c[s - 1];
    let c1 = c[s];
    (b0 * c0 + b1 * c1, (c1 - c0) * inv_h)
}

/// Build an interpolating spline of degree `k` over (x, y) for the wow
/// interpolation modes (k=1 linear, k=2 quadratic, k=3 natural cubic).
fn make_interp_spline_scaled(x: &[f64], y: &[f64], k: usize) -> Result<(Vec<f64>, Vec<f64>)> {
    // Dispatch once on the runtime degree so the per-point inner loops are
    // monomorphized over a compile-time `K` (collapses the general NURBS recurrence,
    // especially for the common linear k=1 case).
    match k {
        1 => make_interp_spline::<1>(x, y, false),
        2 => make_interp_spline::<2>(x, y, false),
        3 => make_interp_spline::<3>(x, y, true),
        _ => bail!("unsupported spline degree"),
    }
}

/// Construct an interpolating B-spline (knots + coefficients) for degree `k`.
/// `natural` selects the zero-second-derivative boundary condition at both
/// ends, used for the cubic case; otherwise not-a-knot knots are used.
fn make_interp_spline<const K: usize>(
    x: &[f64],
    y: &[f64],
    natural: bool,
) -> Result<(Vec<f64>, Vec<f64>)> {
    let n = x.len();
    if n != y.len() {
        bail!("make_interp_spline: x and y length mismatch");
    }
    if n < 2 {
        bail!("make_interp_spline: need at least two points");
    }

    // special-case k=1: t = [x0, x, x_{-1}], c = y (Lyche and Morken, Eq.(2.16)).
    if K == 1 {
        let mut t = Vec::with_capacity(n + 2);
        t.push(x[0]);
        t.extend_from_slice(x);
        t.push(x[n - 1]);
        return Ok((t, y.to_vec()));
    }

    // Construct the knot vector.
    let t: Vec<f64> = if natural {
        // _augknt(x, k): k copies of x0, x, k copies of x_{-1}.
        let mut t = Vec::with_capacity(n + 2 * K);
        for _ in 0..K {
            t.push(x[0]);
        }
        t.extend_from_slice(x);
        for _ in 0..K {
            t.push(x[n - 1]);
        }
        t
    } else {
        // _not_a_knot(x, k).
        let interior: Vec<f64> = if K % 2 == 1 {
            let k2 = K.div_ceil(2);
            x[k2..n - k2].to_vec()
        } else {
            let k2 = K / 2;
            let mids: Vec<f64> = (0..n - 1).map(|i| (x[i + 1] + x[i]) / 2.0).collect();
            mids[k2..mids.len() - k2].to_vec()
        };
        let mut t = Vec::with_capacity(2 * (K + 1) + interior.len());
        for _ in 0..=K {
            t.push(x[0]);
        }
        t.extend_from_slice(&interior);
        for _ in 0..=K {
            t.push(x[n - 1]);
        }
        t
    };

    let nt = t.len() - K - 1;
    let (nleft, nright) = if natural {
        (1usize, 1usize)
    } else {
        (0usize, 0usize)
    };
    if nt != n + nleft + nright {
        bail!("make_interp_spline: knot/condition count mismatch");
    }

    // Build the collocation matrix with boundary derivative rows.
    let mut a = vec![vec![0.0; nt]; nt];
    let mut rhs = vec![0.0; nt];

    // Left boundary derivative rows (natural: 2nd derivative == 0 at x[0]).
    if nleft > 0 {
        let span = bspline_find_span(&t, K, nt, x[0]);
        let ders = bspline_ders_basis::<K>(&t, span, x[0], 2);
        for j in 0..=K {
            a[0][span - K + j] = ders[2][j];
        }
        rhs[0] = 0.0;
    }

    // Collocation rows: spline value at each data point equals y.
    for i in 0..n {
        let span = bspline_find_span(&t, K, nt, x[i]);
        let ders = bspline_ders_basis::<K>(&t, span, x[i], 0);
        let row = nleft + i;
        for j in 0..=K {
            a[row][span - K + j] = ders[0][j];
        }
        rhs[row] = y[i];
    }

    // Right boundary derivative rows.
    if nright > 0 {
        let span = bspline_find_span(&t, K, nt, x[n - 1]);
        let ders = bspline_ders_basis::<K>(&t, span, x[n - 1], 2);
        let row = nt - nright;
        for j in 0..=K {
            a[row][span - K + j] = ders[2][j];
        }
        rhs[row] = 0.0;
    }

    let c = solve_dense(a, rhs)?;
    Ok((t, c))
}

/// Evaluate the k+1 nonzero B-spline basis functions and their derivatives up to
/// order `d` at `x` in knot span `span`. Returns ders[order][j], j = 0..=k indexing
/// the basis functions B_{span-k+j}. Algorithm A2.3 from "The NURBS Book".
///
/// Degree `k` is bounded by 3 (cubic) in this codebase, so all scratch storage uses
/// fixed-size stack arrays of width 4.
fn bspline_ders_basis<const K: usize>(t: &[f64], span: usize, x: f64, d: usize) -> [[f64; 4]; 4] {
    let p = K;
    debug_assert!(p <= 3 && d <= p);
    let mut ndu = [[0.0f64; 4]; 4];
    let mut left = [0.0f64; 4];
    let mut right = [0.0f64; 4];
    ndu[0][0] = 1.0;
    for j in 1..=p {
        left[j] = x - t[span + 1 - j];
        right[j] = t[span + j] - x;
        let mut saved = 0.0;
        for r in 0..j {
            ndu[j][r] = right[r + 1] + left[j - r];
            let temp = ndu[r][j - 1] / ndu[j][r];
            ndu[r][j] = saved + right[r + 1] * temp;
            saved = left[j - r] * temp;
        }
        ndu[j][j] = saved;
    }

    let mut ders = [[0.0f64; 4]; 4];
    for j in 0..=p {
        ders[0][j] = ndu[j][p];
    }

    let mut a = [[0.0f64; 4]; 2];
    for r in 0..=p {
        let mut s1 = 0usize;
        let mut s2 = 1usize;
        a[0][0] = 1.0;
        for kk in 1..=d {
            let mut acc = 0.0;
            let rk = r as isize - kk as isize;
            let pk = p as isize - kk as isize;
            if r >= kk {
                a[s2][0] = a[s1][0] / ndu[(pk + 1) as usize][rk as usize];
                acc = a[s2][0] * ndu[rk as usize][pk as usize];
            }
            let j1 = if rk >= -1 { 1 } else { (-rk) as usize };
            let j2 = if (r as isize - 1) <= pk {
                kk - 1
            } else {
                p - r
            };
            for j in j1..=j2 {
                a[s2][j] =
                    (a[s1][j] - a[s1][j - 1]) / ndu[(pk + 1) as usize][(rk + j as isize) as usize];
                acc += a[s2][j] * ndu[(rk + j as isize) as usize][pk as usize];
            }
            if (r as isize) <= pk {
                a[s2][kk] = -a[s1][kk - 1] / ndu[(pk + 1) as usize][r];
                acc += a[s2][kk] * ndu[r][pk as usize];
            }
            ders[kk][r] = acc;
            std::mem::swap(&mut s1, &mut s2);
        }
    }

    // Multiply through by the correct factors (Eq. [2.9] in "The NURBS Book").
    let mut r = p;
    for kk in 1..=d {
        for j in 0..=p {
            ders[kk][j] *= r as f64;
        }
        r *= p - kk;
    }
    ders
}

/// Find the knot span index `i` such that t[i] <= x < t[i+1], clamped to the
/// valid evaluation range [k, nt-1] so out-of-bounds x uses the edge polynomial.
fn bspline_find_span(t: &[f64], k: usize, nt: usize, x: f64) -> usize {
    if x <= t[k] {
        return k;
    }
    if x >= t[nt] {
        return nt - 1;
    }
    let mut lo = k;
    let mut hi = nt;
    let mut mid = (lo + hi) / 2;
    while x < t[mid] || x >= t[mid + 1] {
        if x < t[mid] {
            hi = mid;
        } else {
            lo = mid;
        }
        mid = (lo + hi) / 2;
    }
    mid
}

/// Solve a dense linear system A x = b in place using Gaussian elimination with
/// partial pivoting. `a` is row-major n x n; returns the solution vector.
fn solve_dense(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Result<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        // Partial pivot.
        let mut pivot = col;
        let mut best = a[col][col].abs();
        for row in (col + 1)..n {
            let v = a[row][col].abs();
            if v > best {
                best = v;
                pivot = row;
            }
        }
        if best == 0.0 {
            bail!("Collocation matrix is singular.");
        }
        if pivot != col {
            a.swap(pivot, col);
            b.swap(pivot, col);
        }
        let inv = 1.0 / a[col][col];
        for row in (col + 1)..n {
            let factor = a[row][col] * inv;
            if factor != 0.0 {
                for c in col..n {
                    a[row][c] -= factor * a[col][c];
                }
                b[row] -= factor * b[col];
            }
        }
    }
    // Back-substitution.
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut sum = b[i];
        for c in (i + 1)..n {
            sum -= a[i][c] * x[c];
        }
        x[i] = sum / a[i][i];
    }
    Ok(x)
}
