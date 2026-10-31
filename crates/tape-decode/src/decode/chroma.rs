use super::*;

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

fn acc(
    chroma: &[f32],
    burst_abs_ref: f32,
    burststart: usize,
    burstend: usize,
    linelength: usize,
    lines: usize,
    burst_start_line: i64,
) -> Vec<u16> {
    const STARTING_LINE: usize = 16;
    assert!(lines > STARTING_LINE);

    // Burst-normalize each line and encode it straight to the u16 output.
    // Lines without chroma (the lead-in below STARTING_LINE, lines the color
    // killer blanks, any tail past `lines`) stay at the zero level.
    let mut output = vec![ZERO_LEVEL; chroma.len()];

    for linenumber in STARTING_LINE..lines {
        if (linenumber as i64) < burst_start_line {
            continue;
        }
        let linestart = linelength * linenumber;
        let lineend = linestart + linelength;
        let line = &chroma[linestart..lineend];
        let burst_abs_mean = rms(&line[burststart..burstend]);
        let scale = if burst_abs_mean != 0.0 {
            burst_abs_ref / burst_abs_mean as f32
        } else {
            1.0
        };
        for (out, &sample) in output[linestart..lineend].iter_mut().zip(line.iter()) {
            *out = (sample.mul_add(scale, SIGNED_SAMPLE_MAX) as i64) as u16;
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
fn upconvert_chroma_phase_comp(
    chroma: &[f32],
    field: &DecodedField,
    phase_sequence: &[PhaseSequenceEntry],
    chroma_heterodyne: &[Vec<f32>],
    target_phase: f32,
) -> Vec<f32> {
    let mut uphet = vec![0.0f32; chroma.len()];
    // The heterodyne rows are -cos(a) and sin(a) of the carrier phase, and
    // -cos(a + b) = -cos(a) cos(b) + sin(a) sin(b), so rotating a line by b is
    // a fixed mix of the two rows with no per-sample trigonometry.
    let (neg_cos_row, sin_row) = (&chroma_heterodyne[0], &chroma_heterodyne[1]);

    for entry in phase_sequence {
        let (start, end) = line_bounds(field, chroma.len(), entry.line_number);
        if start >= end {
            continue;
        }

        let rotation = entry.heterodyne_phase as f32 * std::f32::consts::FRAC_PI_2
            + (target_phase + entry.burst.phase).to_radians();
        let (sin_b, cos_b) = rotation.sin_cos();
        for (((out, &sample), &neg_cos), &sin) in uphet[start..end]
            .iter_mut()
            .zip(&chroma[start..end])
            .zip(&neg_cos_row[start..end])
            .zip(&sin_row[start..end])
        {
            *out = sample * neg_cos.mul_add(cos_b, sin * sin_b);
        }
    }

    uphet
}

fn burst_deemphasis(chroma: &mut [f32], field: &DecodedField, burstarea_end: usize) {
    let lineoffset = field.lineoffset + 1;
    for line in lineoffset..field.outlinecount + lineoffset {
        let linestart = (line - lineoffset) * field.outlinelen;
        let lineend = linestart + field.outlinelen;
        for sample in &mut chroma[linestart + burstarea_end + 5..lineend] {
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
) -> Result<Vec<u16>> {
    let burst_start_line = field.burst_start_line();
    if burst_start_line == -1 {
        return Ok(vec![ZERO_LEVEL; field.outlinecount * field.outlinelen]);
    }

    let mut chroma = tracked_chroma(field, spec, chroma_afc_state)?;
    let burstarea = padded_burst_area(spec);

    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    if is_ntsc {
        burst_deemphasis(&mut chroma, field, burstarea.1 as usize)
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
            phase_sequence,
            &spec.rf_chroma_heterodyne,
            NTSC_TARGET_BURST_PHASE,
        )
    } else {
        upconvert_chroma(
            &chroma,
            field,
            phase_sequence,
            active_chroma_heterodyne(spec, chroma_afc_state),
        )
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
    Ok(acc(
        &uphet,
        burst_abs_ref,
        burstarea.0 as usize,
        burstarea.1 as usize,
        field.outlinelen,
        field.outlinecount,
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
    let upconverted = process_chroma_internal(field, spec, chroma_afc_state)?;
    Ok(Some(upconverted))
}
