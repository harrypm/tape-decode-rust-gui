use super::*;

/// Output sample range of the slice `loc`, a field line, its start in
/// microseconds into the line and its length in microseconds.
fn lineslice_tbc_bounds(spec: &DecoderSpec, loc: &[usize; 3]) -> (usize, usize) {
    let outlinelen = spec.sys_outlinelen as f64;
    let begin_px = outlinelen * (loc[0] as f64 - 1.0) + usectooutpx(spec, loc[1] as f64);
    let length_px = usectooutpx(spec, loc[2] as f64);
    (
        begin_px.round_ties_even().max(0.0) as usize,
        (begin_px + length_px).round_ties_even().max(0.0) as usize,
    )
}

fn clamped_slice<T>(values: &[T], start: usize, end: usize) -> &[T] {
    &values[start.min(values.len())..end.min(values.len())]
}

/// IRE values of the luma samples `start..end` of the field: the encoded
/// output's codes through the field's output scale, the raw export's hertz
/// through the format's nominal levels.
fn luma_slice_to_ire(
    field: &DecodedField,
    spec: &DecoderSpec,
    luma: &LumaOutput,
    start: usize,
    end: usize,
) -> Vec<f64> {
    match luma {
        LumaOutput::Encoded(values) => {
            let output_zero = spec.sys_output_zero;
            let out_scale = field.out_scale.expect("a valid field has its output scale");
            let vsync_ire = f64::from(spec.sys_vsync_ire);
            clamped_slice(values, start, end)
                .iter()
                .map(|&value| ((i64::from(value) - output_zero) as f64 / out_scale) + vsync_ire)
                .collect()
        }
        LumaOutput::Raw(values) => {
            let ire0 = f64::from(spec.sys_ire0);
            let hz_ire = f64::from(spec.sys_hz_ire);
            clamped_slice(values, start, end)
                .iter()
                .map(|&hz| (f64::from(hz) - ire0) / hz_ire)
                .collect()
        }
    }
}

// SNR (dB) of a luma slice relative to a 100 IRE reference, from the standard
// deviation of its IRE-converted samples. Returns 0 for a noiseless slice.
fn snr_at(
    field: &DecodedField,
    spec: &DecoderSpec,
    luma: &LumaOutput,
    start: usize,
    end: usize,
) -> f64 {
    let data = luma_slice_to_ire(field, spec, luma, start, end);
    let noise = rms(&data);
    if noise == 0.0 {
        0.0
    } else {
        20.0 * (100.0 / noise).log10()
    }
}

// VITS-derived white SNR / black PSNR for a decoded field's luma picture.
pub(crate) fn compute_vits_metrics(
    spec: &DecoderSpec,
    metrics_field: &DecodedField,
    metrics_luma: &LumaOutput,
) -> VitsMetrics {
    let mut wsnr_raw: Option<f64> = None;
    for wl in &spec.sys_ld_vits_whitelocs {
        let (start, end) = lineslice_tbc_bounds(spec, wl);
        let ire_values = luma_slice_to_ire(metrics_field, spec, metrics_luma, start, end);
        if inrange(mean(&ire_values), 90.0, 110.0) {
            wsnr_raw = Some(snr_at(metrics_field, spec, metrics_luma, start, end));
            break;
        }
    }
    let (black_start, black_end) = lineslice_tbc_bounds(spec, &spec.sys_blacksnr_slice);
    let bpsnr_raw = snr_at(metrics_field, spec, metrics_luma, black_start, black_end);
    let round_finite = |value: f64| {
        let rounded = roundfloat(value, 1);
        rounded.is_finite().then_some(rounded)
    };
    VitsMetrics {
        w_snr: wsnr_raw.and_then(round_finite),
        b_psnr: round_finite(bpsnr_raw),
    }
}
