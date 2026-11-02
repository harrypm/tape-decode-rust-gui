use super::sync::{refine_levels_from_vsync, try_get_pulses};
use super::*;
use std::f64::consts::{PI, TAU};

/// Measures the burst kept at output sample `burst_start` of the field
/// buffer, on the line starting at `line_start`, against the reference
/// subcarrier table.
fn demod_burst(
    burst: &[f32],
    burst_start: usize,
    line_start: usize,
    burst_wave: &[(f32, f32)],
    fsc_hz: f64,
) -> BurstMeasurement {
    let reference = burst_wave
        .get(burst_start..burst_start + burst.len())
        .expect("the reference subcarrier table spans the field");

    let mut i = 0.0f32;
    let mut q = 0.0f32;
    let mut sum = 0.0f32;
    for (&sample, &(sin, cos)) in burst.iter().zip(reference) {
        sum += sample;
        i += sample * cos;
        q += sample * sin;
    }

    let len = burst.len() as f64;
    let seed = BurstFit {
        amplitude: 2.0 * f64::from(i.hypot(q)) / len,
        phase: f64::from(q.atan2(i)),
        dc: f64::from(sum) / len,
        frequency: fsc_hz,
    };
    let fit = fit_burst(burst, burst_start, fsc_hz, seed);
    // The phase as a sample offset, four samples per cycle.
    let phase_samples = fit.phase.rem_euclid(TAU) * (2.0 / PI);
    BurstMeasurement {
        phase: (fit.phase.to_degrees().rem_euclid(360.0)) as f32,
        amplitude: fit.amplitude as f32,
        magnitude: (fit.amplitude * len / 2.0) as f32,
        frequency: fit.frequency,
        dc: fit.dc as f32,
        center: (burst_start - line_start) as f64 + (len - 1.0) / 2.0 + phase_samples,
        i,
        q,
    }
}

/// Parameters of the burst model `amplitude * cos(2 pi frequency t - phase) + dc`.
#[derive(Clone, Copy)]
struct BurstFit {
    amplitude: f64,
    /// Radians.
    phase: f64,
    dc: f64,
    /// Hz.
    frequency: f64,
}

/// Refines the correlation estimate `seed` by a Gauss-Newton fit of the burst
/// model, with `t` in seconds from the start of the field buffer (the
/// reference table's origin) at four samples per subcarrier cycle. The
/// frequency is pulled towards `fsc_hz` by a prior, so a noisy burst cannot
/// trade frequency against phase along the model's near-degenerate valley.
fn fit_burst(burst: &[f32], burst_start: usize, fsc_hz: f64, seed: BurstFit) -> BurstFit {
    const FREQUENCY_PRIOR_WEIGHT: f64 = 1e4;
    const REGULARIZATION: f64 = 1e-6;
    const MAX_ITERATIONS: usize = 32;
    const PRECISION: f64 = 1e-10;

    let mut fit = seed;
    let time_at = |k: usize| (k + burst_start) as f64 / (4.0 * fsc_hz);
    for _ in 0..MAX_ITERATIONS {
        // Normal equations (J^T J) delta = J^T r of the residual r = burst -
        // model, with the Jacobian columns in the parameter order
        // (amplitude, phase, dc, frequency).
        let mut jtj = [[0.0f64; 4]; 4];
        let mut jtr = [0.0f64; 4];
        // The model phase advances by a constant per sample, so its sine and
        // cosine are walked as a rotating phasor from the first sample's.
        let (mut sin, mut cos) = (TAU * fit.frequency * time_at(0) - fit.phase).sin_cos();
        let (step_sin, step_cos) = (TAU * fit.frequency / (4.0 * fsc_hz)).sin_cos();
        for (k, &sample) in burst.iter().enumerate() {
            let t = time_at(k);
            let residual = f64::from(sample) - (fit.amplitude * cos + fit.dc);
            let d_phase = fit.amplitude * sin;
            let jacobian = [cos, d_phase, 1.0, -TAU * t * d_phase];
            for ((jtj_row, jtr_row), &j_row) in jtj.iter_mut().zip(&mut jtr).zip(&jacobian) {
                for (cell, &j_col) in jtj_row.iter_mut().zip(&jacobian) {
                    *cell += j_row * j_col;
                }
                *jtr_row += j_row * residual;
            }
            (sin, cos) = (
                sin.mul_add(step_cos, cos * step_sin),
                cos.mul_add(step_cos, -(sin * step_sin)),
            );
        }
        jtj[3][3] += FREQUENCY_PRIOR_WEIGHT;
        jtr[3] += FREQUENCY_PRIOR_WEIGHT * (fsc_hz - fit.frequency);
        for (row, jtj_row) in jtj.iter_mut().enumerate() {
            jtj_row[row] += REGULARIZATION;
        }

        let Some(delta) = solve_4x4(jtj, jtr) else {
            break;
        };
        fit.amplitude += delta[0];
        fit.phase += delta[1];
        fit.dc += delta[2];
        fit.frequency += delta[3];
        if delta.iter().all(|step| step.abs() < PRECISION) {
            break;
        }
    }
    fit.phase = (fit.phase + PI).rem_euclid(TAU) - PI;
    fit
}

/// Gaussian elimination with partial pivoting; `None` when a pivot vanishes.
fn solve_4x4(a: [[f64; 4]; 4], b: [f64; 4]) -> Option<[f64; 4]> {
    let mut m = [[0.0f64; 5]; 4];
    for (row, a_row) in m.iter_mut().zip(&a) {
        row[..4].copy_from_slice(a_row);
    }
    for (row, &b_row) in m.iter_mut().zip(&b) {
        row[4] = b_row;
    }

    for i in 0..4 {
        let mut pivot = i;
        for row in i + 1..4 {
            if m[row][i].abs() > m[pivot][i].abs() {
                pivot = row;
            }
        }
        m.swap(i, pivot);
        if m[i][i].abs() < 1e-12 {
            return None;
        }
        let (upper, lower) = m.split_at_mut(i + 1);
        let pivot_row = &upper[i];
        for row in lower {
            let factor = row[i] / pivot_row[i];
            for col in i..5 {
                row[col] -= factor * pivot_row[col];
            }
        }
    }

    let mut x = [0.0f64; 4];
    for i in (0..4).rev() {
        let sum: f64 = (i + 1..4).map(|j| m[i][j] * x[j]).sum();
        x[i] = (m[i][4] - sum) / m[i][i];
    }
    Some(x)
}

fn calczc_do(
    data: &[f32],
    start_offset: isize,
    target: f32,
    count: isize,
    mut edge: i64,
) -> Result<f64> {
    // The crossing is interpolated from the sample before it, so the search
    // starts at index 1 at the earliest.
    let start = start_offset.max(1) as usize;
    if start >= data.len() {
        bail!("start_offset out of range");
    }

    if edge == 0 {
        edge = if data[start] < target { 1 } else { -1 };
    }

    if (edge == 1 && data[start] > target) || (edge == -1 && data[start] < target) {
        return Ok(f64::NAN);
    }

    let search_len = count + 1;
    let search_end = if search_len <= 0 {
        start
    } else {
        start.saturating_add(search_len as usize).min(data.len())
    };
    let Some(loc) = data[start..search_end]
        .iter()
        .position(|&sample| (edge == 1 && sample >= target) || (edge != 1 && sample <= target))
    else {
        return Ok(f64::NAN);
    };

    let x = start + loc;
    let a = data[x - 1] - target;
    let b = data[x] - target;
    let y = if b - a != 0.0 { -a / (-a + b) } else { 0.0 };

    Ok(x as f64 - 1.0 + f64::from(y))
}

fn signed_bounds_slice<T>(data: &[T], start: isize, end: isize) -> &[T] {
    let start = resolve_slice_bound(data.len(), start);
    let end = resolve_slice_bound(data.len(), end);
    data.get(start..end).unwrap_or_default()
}

fn slice_empty_or_out_of_range(values: &[f32], min: f32, max: f32) -> bool {
    if values.is_empty() {
        return true;
    }

    values.iter().any(|&value| value < min || value > max)
}

fn round_ties_even_to_isize(value: f64) -> isize {
    value.round_ties_even() as isize
}

fn refine_linelocs_hsync(
    spec: &DecoderSpec,
    initial_linelocs: &[f64],
    demod_05: &[f32],
    linebad: &mut [u8],
    normal_hsync_length: usize,
    levels: VideoLevels,
) -> Result<Vec<f64>> {
    let one_usec = spec.freq as usize;
    let is_pal = spec.sys_frame_lines != LineSystem::Line525;
    let zc_threshold = levels.iretohz(spec.sys_vsync_ire / 2.0);
    let ire_30 = levels.iretohz(30.0);
    let ire_n_65 = levels.iretohz(-65.0);
    let ire_110 = levels.iretohz(110.0);

    let mut linelocs_refined = initial_linelocs.to_vec();
    let mut refined_from_right_lineloc = -1.0;
    let mut prev_porch_level = -1.0f32;
    let one_usec_samples = spec.freq;
    let normal_hsync_samples = normal_hsync_length as f64;

    for i in 0..initial_linelocs.len() {
        if (3..=6).contains(&i) || (is_pal && (1..=2).contains(&i)) {
            linebad[i] = 1;
            continue;
        }

        let ll1 = round_ties_even_to_isize(initial_linelocs[i]) - one_usec as isize;
        let zc = calczc_do(demod_05, ll1, zc_threshold, (one_usec * 2) as isize, 0)?;

        let mut right_cross = f64::NAN;
        if !spec.rf_disable_right_hsync {
            right_cross = calczc_do(
                demod_05,
                ll1 + normal_hsync_length as isize - one_usec as isize,
                zc_threshold,
                (normal_hsync_length * 2) as isize,
                1,
            )?;
        }
        let mut right_cross_refined = false;

        if !zc.is_nan() && linebad[i] == 0 {
            linelocs_refined[i] = zc;

            let hsync_area = signed_bounds_slice(
                demod_05,
                round_ties_even_to_isize(zc - (one_usec_samples * 0.75)),
                round_ties_even_to_isize(zc + (one_usec_samples * 3.5)),
            );
            if slice_empty_or_out_of_range(hsync_area, ire_n_65, ire_110) {
                linebad[i] = 1;
                linelocs_refined[i] = initial_linelocs[i];
            } else {
                let porch_level = if prev_porch_level > 0.0 {
                    prev_porch_level
                } else {
                    mean_slice(signed_bounds_slice(
                        demod_05,
                        round_ties_even_to_isize(zc - one_usec_samples),
                        round_ties_even_to_isize(zc - (one_usec_samples * 0.5)),
                    )) as f32
                };
                let sync_level = mean_slice(signed_bounds_slice(
                    demod_05,
                    round_ties_even_to_isize(zc + one_usec_samples),
                    round_ties_even_to_isize(zc + (one_usec_samples * 2.5)),
                )) as f32;

                let zc2 = calczc_do(demod_05, ll1, (porch_level + sync_level) / 2.0, 400, 0)?;
                if !zc2.is_nan() && (zc2 - zc).abs() < (one_usec_samples / 2.0) {
                    linelocs_refined[i] = zc2;
                    prev_porch_level = porch_level;
                } else if prev_porch_level > 0.0 {
                    let zc2 =
                        calczc_do(demod_05, ll1, (prev_porch_level + sync_level) / 2.0, 400, 0)?;
                    if !zc2.is_nan() && (zc2 - zc).abs() < (one_usec_samples / 2.0) {
                        linelocs_refined[i] = zc2;
                    } else {
                        linebad[i] = 1;
                    }
                } else {
                    linebad[i] = 1;
                }
            }
        } else {
            linebad[i] = 1;
        }

        if !right_cross.is_nan() {
            let zc_fr = right_cross - normal_hsync_samples;
            let hsync_area = signed_bounds_slice(
                demod_05,
                round_ties_even_to_isize(zc_fr - (one_usec_samples * 0.75)),
                round_ties_even_to_isize(zc_fr + (one_usec_samples * 8.0)),
            );

            if !slice_empty_or_out_of_range(hsync_area, ire_n_65, ire_30) {
                let porch_level = mean_slice(signed_bounds_slice(
                    demod_05,
                    round_ties_even_to_isize(zc_fr + normal_hsync_samples + one_usec_samples),
                    round_ties_even_to_isize(
                        zc_fr + normal_hsync_samples + (one_usec_samples * 2.0),
                    ),
                )) as f32;

                let sync_level = mean_slice(signed_bounds_slice(
                    demod_05,
                    round_ties_even_to_isize(zc_fr + one_usec_samples),
                    round_ties_even_to_isize(zc_fr + (one_usec_samples * 2.5)),
                )) as f32;

                let zc2 = calczc_do(
                    demod_05,
                    ll1 + normal_hsync_length as isize - one_usec as isize,
                    (porch_level + sync_level) / 2.0,
                    400,
                    0,
                )?;

                if !zc2.is_nan() && (zc2 - right_cross).abs() < (one_usec_samples / 2.0) {
                    refined_from_right_lineloc =
                        right_cross - normal_hsync_samples + (2.25 * (spec.freq / 40.0));
                    if (refined_from_right_lineloc - linelocs_refined[i]).abs()
                        < (one_usec_samples * 2.0)
                    {
                        right_cross = zc2;
                        right_cross_refined = true;
                        prev_porch_level = porch_level;
                    }
                }
            }
        }

        if linebad[i] != 0 {
            linelocs_refined[i] = initial_linelocs[i];
        }

        if !right_cross.is_nan() && right_cross_refined {
            linebad[i] = 0;
            linelocs_refined[i] = refined_from_right_lineloc;
        }
    }

    Ok(linelocs_refined)
}

fn valid_pulses_to_linelocs(
    mut validpulses: Vec<f64>,
    reference_pulse: f64,
    reference_line: i64,
    meanlinelen: f64,
    proclines: usize,
) -> Vec<f64> {
    validpulses.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Greater));

    let mut line_locations = vec![0.0; proclines];
    let validpulses_len = validpulses.len();
    let max_allowed_distance_between_pulse_and_line = meanlinelen / 1.5;
    let reference_line = reference_line as f64;
    let mut current_pulse_index = 0usize;

    for (line_index, line_location) in line_locations.iter_mut().enumerate() {
        let expected_line_location =
            reference_pulse + meanlinelen * (line_index as f64 - reference_line);
        *line_location = expected_line_location;

        if current_pulse_index < validpulses_len {
            let mut current_distance_from_pulse_to_line =
                (validpulses[current_pulse_index] - expected_line_location).abs();
            let mut smallest_distance_observed_from_pulse_to_line =
                max_allowed_distance_between_pulse_and_line;
            let mut current_pulse_sample_location = -1.0;

            let mut pulse_search_index = current_pulse_index;
            loop {
                if current_distance_from_pulse_to_line
                    <= smallest_distance_observed_from_pulse_to_line
                {
                    smallest_distance_observed_from_pulse_to_line =
                        current_distance_from_pulse_to_line;
                    current_pulse_index = pulse_search_index;
                    current_pulse_sample_location = validpulses[pulse_search_index];
                }

                let Some(next_pulse) = validpulses.get(pulse_search_index + 1) else {
                    break;
                };
                let next_observed_distance_between_pulse_and_line =
                    (next_pulse - expected_line_location).abs();
                if next_observed_distance_between_pulse_and_line
                    > current_distance_from_pulse_to_line
                {
                    break;
                }

                current_distance_from_pulse_to_line = next_observed_distance_between_pulse_and_line;
                pulse_search_index += 1;
            }

            if current_pulse_sample_location != -1.0 {
                *line_location = current_pulse_sample_location;
                current_pulse_index += 1;
            }
        }
    }

    line_locations
}

fn rotated_phase(current_phase: usize, track_rotation: i64) -> usize {
    (current_phase as i64 + track_rotation).rem_euclid(4) as usize
}

#[allow(clippy::too_many_arguments)]
fn get_upconverted_burst(
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_filter: &[Sos<f32>],
    current_phase: usize,
    burst_area: (usize, usize),
    burst_wave: &[(f32, f32)],
    fsc_hz: f64,
    line_number: usize,
    lineoffset: usize,
    outwidth: usize,
) -> BurstMeasurement {
    // Upconvert and filter from the line start to as far past the burst as
    // the burst sits from the line start, so the filter transients fall
    // outside the kept window.
    let (burst_start, burst_end) = burst_area;
    let line_start = (line_number - lineoffset) * outwidth;
    let window_end = line_start + burst_end + burst_start;
    let upconverted = chroma[line_start..window_end]
        .iter()
        .zip(&chroma_heterodyne[current_phase][line_start..window_end])
        .map(|(&sample, &het)| sample * het)
        .collect::<Vec<f32>>();
    let filtered = sosfiltfilt_f32(chroma_filter, &upconverted);

    demod_burst(
        &filtered[burst_start..burst_end],
        line_start + burst_start,
        line_start,
        burst_wave,
        fsc_hz,
    )
}

fn get_phase_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_rotation_starting_index: Option<i64>,
    burst_area: (usize, usize),
    track_change_threshold: f32,
) -> (i64, Vec<PhaseSequenceEntry>) {
    const BURST_CHECK_SKIP_LINES: usize = 16;

    let chroma_rotation = spec.decoder_chroma_rotation.map(|v| (v[0], v[1]));
    let do_phase_rotation_check = spec.rf_detect_chroma_track_phase && chroma_rotation.is_some();
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    let lineoffset = field.lineoffset + 1;
    let last_line = field.outlinecount + lineoffset;
    let rotation_check_start_line = lineoffset + field.outlinecount - BURST_CHECK_SKIP_LINES;
    let starting_index = chroma_rotation_starting_index.unwrap_or(0);
    let mut chroma_rotation_index;
    let mut track_rotation;
    if let Some(rotation) = chroma_rotation {
        chroma_rotation_index = starting_index;
        track_rotation = match chroma_rotation_index {
            0 => rotation.0,
            1 => rotation.1,
            _ => unreachable!("track phase {chroma_rotation_index} is neither 0 nor 1"),
        };
    } else {
        chroma_rotation_index = 0;
        track_rotation = starting_index;
    }

    let upconvert = |phase: usize, linenumber: usize| {
        get_upconverted_burst(
            chroma,
            chroma_heterodyne,
            &spec.chroma_filter_final,
            phase,
            burst_area,
            &spec.rf_fsc_wave,
            spec.sys_fsc_mhz * 1e6,
            linenumber,
            lineoffset,
            field.outlinelen,
        )
    };

    let mut phase_sequence: Vec<PhaseSequenceEntry> =
        Vec::with_capacity(last_line.saturating_sub(lineoffset));
    let mut current_phase = 0usize;
    // The next line's burst, when the rotation check already measured it.
    let mut next: Option<(usize, BurstMeasurement)> = None;

    for linenumber in lineoffset..last_line {
        let current_burst = if let Some((phase, burst)) = next.take() {
            current_phase = phase;
            burst
        } else {
            current_phase = rotated_phase(current_phase, track_rotation);
            upconvert(current_phase, linenumber)
        };

        if do_phase_rotation_check
            && linenumber >= rotation_check_start_line
            && linenumber + 1 < last_line
        {
            let next_phase = rotated_phase(current_phase, track_rotation);
            let next_burst = upconvert(next_phase, linenumber + 1);

            // PAL-family bursts alternate between lines, so the next line is
            // compared against the one two back; NTSC against the current one.
            let comparison_phase = if is_ntsc {
                current_burst.phase
            } else {
                phase_sequence
                    .last()
                    .map_or(current_burst.phase, |entry| entry.burst.phase)
            };
            let phase_delta_quadrant =
                ((next_burst.phase - comparison_phase + 180.0).rem_euclid(360.0) - 180.0).abs();
            if phase_delta_quadrant > track_change_threshold {
                chroma_rotation_index = (chroma_rotation_index + 1).rem_euclid(2);
                if let Some(rotation) = chroma_rotation {
                    track_rotation = if chroma_rotation_index == 0 {
                        rotation.0
                    } else {
                        rotation.1
                    };
                }
            } else {
                next = Some((next_phase, next_burst));
            }
        }

        phase_sequence.push(PhaseSequenceEntry {
            line_number: linenumber,
            heterodyne_phase: current_phase,
            burst: current_burst,
        });
    }

    if chroma_rotation.is_some() && chroma_rotation_index == starting_index {
        chroma_rotation_index = (chroma_rotation_index + 1).rem_euclid(2);
    }

    (chroma_rotation_index, phase_sequence)
}

fn get_phase_rotation_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_rotation_index: Option<i64>,
    burst_area: (usize, usize),
    prev_burst_start_line: i64,
) -> (i64, BurstLock) {
    const TRACK_CHANGE_THRESHOLD: f32 = 90.0;
    const BURST_CHECK_SKIP_LINES: isize = 16;
    // Field-average burst magnitude below which the color killer blanks the
    // field. Set on the 16-bit sample scale, which the reader normalizes by
    // 32768; a healthy burst measures a few times this.
    const BURST_MAGNITUDE_THRESHOLD: f32 = 2.5e4 / 32768.0;

    let lineoffset = field.lineoffset + 1;
    let end = field.outlinecount + lineoffset;
    let chroma_rotation = spec.decoder_chroma_rotation.map(|v| (v[0], v[1]));
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    let (mut chroma_rotation_index, mut phase_sequence) = get_phase_sequence(
        spec,
        field,
        chroma,
        chroma_heterodyne,
        chroma_rotation_index,
        burst_area,
        TRACK_CHANGE_THRESHOLD,
    );

    let burst_check_start = BURST_CHECK_SKIP_LINES;
    let burst_check_end = end as isize - BURST_CHECK_SKIP_LINES;

    let flip_track_phase = if chroma_rotation.is_some() {
        let mut delta_counts = [0usize; 4];

        for window in phase_sequence.windows(2) {
            let previous_burst_phase = window[0].burst.phase;
            let line_number = window[1].line_number as isize;
            let current_burst_phase = window[1].burst.phase;

            if line_number > burst_check_start && line_number < burst_check_end {
                let delta = (current_burst_phase - previous_burst_phase).rem_euclid(360.0);
                let bucket = (((delta + 45.0) / 90.0).floor() as i64).rem_euclid(4);
                delta_counts[bucket as usize] += 1;
            }
        }

        if is_ntsc {
            delta_counts[0] < delta_counts[2]
        } else {
            delta_counts[1] + delta_counts[3] < delta_counts[0] + delta_counts[2]
        }
    } else {
        false
    };

    if flip_track_phase {
        (chroma_rotation_index, phase_sequence) = get_phase_sequence(
            spec,
            field,
            chroma,
            chroma_heterodyne,
            Some(chroma_rotation_index),
            burst_area,
            TRACK_CHANGE_THRESHOLD,
        );
    }

    // Average burst phase per line parity (PAL bursts alternate), plus the
    // color killer state.
    let mut even = (0.0f32, 0.0f32);
    let mut odd = (0.0f32, 0.0f32);
    let mut count = 0usize;
    let mut magnitude_sum = 0.0f32;
    let mut burst_start_line = 0i64;
    for entry in &phase_sequence {
        let line_number = entry.line_number as isize;
        let burst = entry.burst;
        if line_number <= burst_check_start
            || line_number >= burst_check_end
            || burst.magnitude == 0.0
        {
            continue;
        }
        count += 1;
        magnitude_sum += burst.magnitude;
        // After a blanked field, color resumes at the first line whose burst
        // clears the threshold: broadcasts could switch the burst on
        // mid-field.
        if spec.rf_enable_color_killer
            && prev_burst_start_line == -1
            && burst_start_line == 0
            && burst.magnitude > BURST_MAGNITUDE_THRESHOLD
        {
            burst_start_line = entry.line_number as i64;
        }
        let total = if entry.line_number % 2 == 1 {
            &mut odd
        } else {
            &mut even
        };
        total.0 += burst.i / burst.magnitude;
        total.1 += burst.q / burst.magnitude;
    }
    let magnitude_avg = if count == 0 {
        0.0
    } else {
        magnitude_sum / count as f32
    };
    if spec.rf_enable_color_killer && magnitude_avg < BURST_MAGNITUDE_THRESHOLD {
        burst_start_line = -1;
    }

    let phase_deg = |(i, q): (f32, f32)| q.atan2(i).to_degrees().rem_euclid(360.0);
    (
        chroma_rotation_index,
        BurstLock {
            phase_sequence,
            burst_start_line,
            phase_avg: phase_deg((even.0 + odd.0, even.1 + odd.1)),
            even_phase_avg: phase_deg(even),
            odd_phase_avg: phase_deg(odd),
        },
    )
}

/// Line locations refined from the burst phase, or `None` when burst hsync
/// refinement is disabled or the color killer blanks the field.
fn burst_refined_linelocs(field: &DecodedField, spec: &DecoderSpec) -> Option<Vec<f64>> {
    if spec.rf_disable_burst_hsync {
        return None;
    }
    let lock = field
        .burst_lock
        .as_ref()
        .filter(|lock| lock.burst_start_line != -1)?;
    let mut linelocs = field.linelocs.clone()?;
    // Every NTSC line targets the field-average burst phase; PAL-family bursts
    // alternate with line parity, so each parity targets its own average.
    let (even_target, odd_target) = if spec.color_system == ColorSystem::Ntsc {
        (lock.phase_avg, lock.phase_avg)
    } else {
        (lock.even_phase_avg, lock.odd_phase_avg)
    };
    // Output samples per subcarrier cycle: a phase error maps to that many
    // output samples, scaled back to the line's input length.
    let fsc_ratio = spec.sys_outfreq / spec.sys_fsc_mhz;
    let fsc_hz = spec.sys_fsc_mhz * 1e6;
    // The first lines sit in vsync; after a mid-field burst turn-on only the
    // lines with color are refined.
    let start = lock.burst_start_line.max(9) as usize;
    for entry in lock.phase_sequence.iter().skip(start) {
        let line = entry.line_number;
        let burst = entry.burst;
        let target = if line % 2 == 1 {
            odd_target
        } else {
            even_target
        };
        let phase_delta = (target - burst.phase + 180.0).rem_euclid(360.0) - 180.0;
        let line_adjust = f64::from(phase_delta) / 360.0 * fsc_ratio;
        // Less the subcarrier drift the fitted frequency accumulates between
        // the line start and the burst center.
        let drift = (burst.frequency - fsc_hz) * burst.center / fsc_hz;
        let scale = (linelocs[line + 1] - linelocs[line]) / field.outlinelen as f64;
        linelocs[line] += (line_adjust - drift) * scale;
    }
    Some(linelocs)
}

fn apply_burst_lock(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    inter_field_state: &mut InterFieldState,
    chroma_afc_state: &ChromaAfcState,
) -> Result<()> {
    let chroma_heterodyne = active_chroma_heterodyne(spec, chroma_afc_state);
    let prev_burst_start_line = field
        .prevfield
        .as_ref()
        .map_or(0, |prev| prev.burst_start_line);

    // The burst lock reads each line only as far past the burst as the
    // burst sits from the line start (see `get_upconverted_burst`).
    let (burst_start, burst_end) = burst_area(spec);
    let burst_downscaled = downscale_raw_vec(
        field,
        &spec.sinc_lut,
        true,
        0.0,
        Some(burst_start + burst_end),
    )?;
    let (track_phase, burst_lock) = get_phase_rotation_sequence(
        spec,
        field,
        &burst_downscaled,
        chroma_heterodyne,
        inter_field_state.track_phase,
        (burst_start, burst_end),
        prev_burst_start_line,
    );
    inter_field_state.track_phase = Some(track_phase);
    field.track_phase = Some(track_phase);
    field.burst_lock = Some(burst_lock);
    Ok(())
}

/// Where the next field is read from.
enum NextField {
    /// A position in this field's buffer.
    At(f64),
    /// Samples to skip from this field's scheduled read location.
    Skip(f64),
}

pub(crate) fn predecode_field_from_rawdecode(
    rawdecode: FieldData,
    spec: &DecoderSpec,
    scheduled_prevfield: Option<PrevFieldState>,
    inter_field_state: &mut InterFieldState,
    scheduled_readloc: u64,
    chroma_afc_state: &ChromaAfcState,
) -> Result<DecodeFieldResult> {
    // Build and classify the next DecodedField from coalesced block data. This is
    // the sync/line-location half of the speculative predecode step; output/TBC
    // conversion and metadata writing remain in the executable orchestrator.
    let readloc = rawdecode.startloc;
    let mut pending_field = DecodedField {
        data: rawdecode,
        prevfield: scheduled_prevfield,
        readloc,
        inlinelen: spec.linelen() as f64,
        outlinelen: spec.sys_outlinelen,
        outlinecount: (spec.sys_frame_lines.line_count() / 2) + 1,
        ire0_backporch: if spec.sys_frame_lines != LineSystem::Line525 {
            (96, 160)
        } else {
            (74, 124)
        },
        wow_level_adjust_smoothing: spec.wow_level_adjust_smoothing,
        wow_interpolation_method: spec.wow_interpolation_method,
        validpulses: Vec::new(),
        is_first_field: None,
        vblank_next: None,
        is_progressive_field: None,
        field_number: 0,
        linelocs: None,
        lineoffset: 0,
        linecount: None,
        out_scale: None,
        field_phase_id: None,
        valid: false,
        levels: inter_field_state.levels,
        burst_lock: None,
        track_phase: inter_field_state.track_phase,
        wow_analysis: None,
    };
    if let Some(prevfield) = &pending_field.prevfield {
        if pending_field.readloc > prevfield.readloc {
            pending_field.field_number = prevfield.field_number + 1;
        } else {
            pending_field.field_number = prevfield.field_number;
            tracing::debug!("readloc loc didn't advance.");
        }
    }

    let do_level_detect = !spec.rf_saved_levels || inter_field_state.compute_linelocs_issues;
    let mut res = try_get_pulses(&mut pending_field, spec, inter_field_state, do_level_detect);
    let needs_level_retry = res.as_ref().is_none_or(|res| res.first_hsync.is_none());
    if needs_level_retry && !do_level_detect {
        tracing::debug!("Search for pulses failed, re-checking levels");
        res = try_get_pulses(&mut pending_field, spec, inter_field_state, true);
    }

    let next_field = if let Some(res) = res.as_ref() {
        let is_first_field = pending_field.is_first_field.unwrap_or(false);
        pending_field.linecount = Some(if is_first_field { 263 } else { 262 });

        // Number of lines to actually process. This is set so that the entire following
        // VSYNC is processed.
        let proclines = pending_field.outlinecount + pending_field.lineoffset + 10;

        if let Some(first_hsync) = res.first_hsync {
            let line0loc = first_hsync.line0loc;
            let input_len = pending_field.data.input_len as f64;
            let lastline = (input_len - line0loc) / res.meanlinelen - 1.0;
            if lastline < proclines as f64 {
                if pending_field.prevfield.is_some() {
                    tracing::info!(lastline, proclines, meanlinelen = res.meanlinelen, line0loc);
                    tracing::info!(
                        "Did not find the expected number of lines (lastline < proclines) , skipping a tiny bit"
                    );
                }
                NextField::At((line0loc - (res.meanlinelen * 20.0)).max(pending_field.inlinelen))
            } else {
                let validpulses = pending_field
                    .validpulses
                    .iter()
                    .map(|&start| start as f64)
                    .collect::<Vec<_>>();
                let linelocs_vec = valid_pulses_to_linelocs(
                    validpulses,
                    (first_hsync.loc as i64) as f64,
                    first_hsync.line as i64,
                    res.meanlinelen,
                    proclines,
                );
                let nextfield = if let Some(vblank_next) = pending_field.vblank_next {
                    vblank_next - (pending_field.inlinelen * 8.0)
                } else {
                    linelocs_vec[pending_field.outlinecount - 7]
                };
                let mut levels = res.levels;
                if !spec.rf_saved_levels {
                    let (sync_level, blank_level) = refine_levels_from_vsync(
                        &pending_field.data.video.demod,
                        line0loc,
                        res.meanlinelen,
                        res.sync_level,
                        res.blank_level,
                    );
                    let adjust = spec.rf_ire0_adjust;
                    if adjust.backporch {
                        levels.ire0 = blank_level;
                    }
                    if adjust.hsync {
                        levels.hz_ire = (blank_level - sync_level) / -spec.sys_vsync_ire;
                    }
                }

                let mut linebad_vec = vec![0u8; linelocs_vec.len()];
                let linelocs2_vec = if !spec.rf_skip_hsync_refine {
                    let normal_hsync_length = usectoinpx(spec, spec.sys_hsync_pulse_us) as usize;
                    refine_linelocs_hsync(
                        spec,
                        &linelocs_vec,
                        &pending_field.data.video.demod_05,
                        &mut linebad_vec,
                        normal_hsync_length,
                        levels,
                    )?
                } else {
                    linelocs_vec
                };
                // With --use-saved-levels this many bad lines has the next
                // field re-detect its levels.
                let bad_lines = linebad_vec.iter().filter(|&&bad| bad != 0).count();
                if bad_lines >= 30 && spec.rf_saved_levels {
                    tracing::debug!(
                        "Possible sync issues, re-running level detection on next field!"
                    );
                }
                pending_field.valid = true;
                pending_field.levels = levels;
                // The field is accepted: the next field's sync search starts
                // from its first hsync, order and levels.
                inter_field_state.prev_first_hsync_readloc = pending_field.readloc as i64;
                inter_field_state.prev_first_hsync_loc = first_hsync.loc;
                inter_field_state.prev_first_hsync_diff = first_hsync.diff;
                inter_field_state.prev_first_field = i64::from(is_first_field);
                inter_field_state.compute_linelocs_issues = bad_lines >= 30;
                inter_field_state.levels = levels;

                if spec.sys_frame_lines != LineSystem::Line525 {
                    pending_field.linelocs = Some(linelocs2_vec);
                    pending_field.out_scale =
                        Some(f64::from(0xD300 - 0x0100) / (100.0 - f64::from(spec.sys_vsync_ire)));
                    if pending_field.valid {
                        if spec.rf_write_chroma {
                            apply_burst_lock(
                                &mut pending_field,
                                spec,
                                inter_field_state,
                                chroma_afc_state,
                            )?;
                            if let Some(refined) = burst_refined_linelocs(&pending_field, spec) {
                                pending_field.linelocs = Some(refined);
                            }
                        }
                        let is_first_field = pending_field.is_first_field.unwrap_or(false);
                        let linecount = match (spec.sys_frame_lines, is_first_field) {
                            (LineSystem::Line405, true) => 203,
                            (LineSystem::Line405, false) => 202,
                            (LineSystem::Line819, true) => 410,
                            (LineSystem::Line819, false) => 409,
                            (_, true) => 312,
                            (_, false) => 313,
                        };
                        pending_field.linecount = Some(linecount);
                        pending_field.lineoffset = if is_first_field { 2 } else { 3 };
                        pending_field.field_phase_id = Some(1);
                    }
                } else {
                    pending_field.linelocs = Some(linelocs2_vec);
                    pending_field.out_scale =
                        Some(f64::from(0xC800 - 0x0400) / (100.0 - f64::from(spec.sys_vsync_ire)));
                    if pending_field.valid {
                        if spec.rf_write_chroma {
                            apply_burst_lock(
                                &mut pending_field,
                                spec,
                                inter_field_state,
                                chroma_afc_state,
                            )?;
                            if let Some(refined) = burst_refined_linelocs(&pending_field, spec) {
                                pending_field.linelocs = Some(refined);
                            }
                        }
                        // Subcarrier phase offset that puts the downscaled image on
                        // the I/Q colour axes (calibrated for ~147 degrees of output
                        // burst phase).
                        let fsc_phase_deg = 117.25;
                        let shift = -((fsc_phase_deg / 360.0) / spec.sys_fsc_mhz * spec.freq);
                        if let Some(linelocs) = pending_field.linelocs.as_mut() {
                            for value in linelocs {
                                *value += shift;
                            }
                        }
                    }
                }
                NextField::At(nextfield)
            }
        } else {
            tracing::warn!("Unable to determine start of field - dropping field");
            NextField::Skip(pending_field.inlinelen * 100.0)
        }
    } else {
        tracing::warn!("Unable to find any sync pulses, jumping 100 ms");
        NextField::Skip(spec.freq_hz() / 10.0)
    };
    // The decoder advances from the scheduled read location, which sits
    // `scheduled_readloc - startloc` samples into the buffer.
    let offset = match next_field {
        NextField::At(position) => {
            position - (scheduled_readloc as f64 - pending_field.data.startloc as f64)
        }
        NextField::Skip(skip) => skip,
    };
    Ok(DecodeFieldResult {
        field: pending_field,
        offset,
    })
}
