use super::sync::try_get_pulses;
use super::*;

fn demod_burst(
    burst: &[f32],
    burst_start: usize,
    line_start: usize,
    line_scale: f32,
    burst_wave: &[(f32, f32)],
) -> Result<BurstMeasurement> {
    let burst_end = burst_start.saturating_add(burst.len());
    if burst_end > burst_wave.len() {
        bail!("burst window exceeds reference waveform length");
    }

    let mut i = 0.0f32;
    let mut q = 0.0f32;
    for (&sample, &(sin, cos)) in burst.iter().zip(&burst_wave[burst_start..burst_end]) {
        i += sample * cos;
        q += sample * sin;
    }

    let magnitude = i.hypot(q);
    let phase = q.atan2(i).to_degrees().rem_euclid(360.0);
    // Resampling the line by `line_scale` shifts the burst by `1 - line_scale`
    // of its distance from hsync, a quarter subcarrier cycle per output sample.
    let phase_error =
        (burst_start as f32 - line_start as f32) * (1.0 - line_scale) * std::f32::consts::FRAC_PI_2;
    let phase_offset = phase_error.to_degrees().rem_euclid(360.0);
    Ok(BurstMeasurement {
        phase,
        phase_offset,
        magnitude,
        i,
        q,
    })
}

fn resolve_signed_index(len: usize, index: isize) -> Option<usize> {
    if index >= 0 {
        usize::try_from(index).ok().filter(|&index| index < len)
    } else {
        len.checked_sub(index.unsigned_abs())
    }
}

fn calczc_do(
    data: &[f32],
    start_offset: isize,
    target: f32,
    count: isize,
    mut edge: i64,
) -> Result<f32> {
    let start_offset_clamped = start_offset.max(1);
    let start_offset_index =
        usize::try_from(start_offset_clamped).context("start_offset out of range")?;
    if start_offset_index >= data.len() {
        bail!("start_offset out of range");
    }

    let target_f = target;
    if edge == 0 {
        edge = if data[start_offset_index] < target_f {
            1
        } else {
            -1
        };
    }

    let edge_index =
        resolve_signed_index(data.len(), start_offset).context("start_offset out of range")?;
    let edge_value = data[edge_index];
    if (edge == 1 && edge_value > target_f) || (edge == -1 && edge_value < target_f) {
        return Ok(f32::NAN);
    }

    let search_len = count + 1;
    let search_end = if search_len <= 0 {
        start_offset_index
    } else {
        start_offset_index
            .saturating_add(search_len as usize)
            .min(data.len())
    };
    let Some(loc) = data[start_offset_index..search_end]
        .iter()
        .position(|&sample| (edge == 1 && sample >= target_f) || (edge != 1 && sample <= target_f))
    else {
        return Ok(f32::NAN);
    };

    let x = start_offset_index + loc;
    let a = data[x - 1] - target_f;
    let b = data[x] - target_f;
    let y = if b - a != 0.0 { -a / (-a + b) } else { 0.0 };

    Ok(x as f32 - 1.0 + y)
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

fn round_ties_even_to_isize(value: f32) -> isize {
    value.round_ties_even() as isize
}

fn refine_linelocs_hsync(
    spec: &DecoderSpec,
    initial_linelocs: &[f32],
    demod_05: &[f32],
    linebad: &mut [u8],
    normal_hsync_length: usize,
    zc_threshold: f32,
) -> Result<Vec<f32>> {
    if linebad.len() != initial_linelocs.len() {
        bail!("linebad length doesn't match linelocs length");
    }

    let one_usec = spec.freq as usize;
    let is_pal = spec.sys_frame_lines != LineSystem::Line525;
    let ire_30 = iretohz(spec.sys_ire0, spec.sys_hz_ire, 30.0);
    let ire_n_65 = iretohz(spec.sys_ire0, spec.sys_hz_ire, -65.0);
    let ire_110 = iretohz(spec.sys_ire0, spec.sys_hz_ire, 110.0);

    let mut linelocs_refined = initial_linelocs.to_vec();
    let mut refined_from_right_lineloc = -1.0;
    let mut prev_porch_level = -1.0f32;
    let one_usec_samples = spec.freq as f32;
    let normal_hsync_samples = normal_hsync_length as f32;

    for i in 0..initial_linelocs.len() {
        if (3..=6).contains(&i) || (is_pal && (1..=2).contains(&i)) {
            linebad[i] = 1;
            continue;
        }

        let ll1 = round_ties_even_to_isize(initial_linelocs[i]) - one_usec as isize;
        let zc = calczc_do(demod_05, ll1, zc_threshold, (one_usec * 2) as isize, 0)?;

        let mut right_cross = f32::NAN;
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
                        right_cross - normal_hsync_samples + (2.25 * (spec.freq as f32 / 40.0));
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
    mut validpulses: Vec<f32>,
    reference_pulse: f32,
    reference_line: i64,
    meanlinelen: f32,
    proclines: usize,
) -> (Vec<f32>, Vec<u8>) {
    validpulses.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Greater));

    let mut line_locations = vec![0.0; proclines];
    let line_location_errs = vec![0; proclines];
    let validpulses_len = validpulses.len();
    let max_allowed_distance_between_pulse_and_line = meanlinelen / 1.5;
    let reference_line = reference_line as f32;
    let mut current_pulse_index = 0usize;

    for (line_index, line_location) in line_locations.iter_mut().enumerate() {
        let expected_line_location =
            reference_pulse + meanlinelen * (line_index as f32 - reference_line);
        *line_location = expected_line_location;

        if current_pulse_index < validpulses_len {
            let mut current_distance_from_pulse_to_line =
                (validpulses[current_pulse_index] - expected_line_location).abs();
            let mut smallest_distance_observed_from_pulse_to_line =
                max_allowed_distance_between_pulse_and_line;
            let mut current_pulse_sample_location = -1.0;

            let mut pulse_search_index = current_pulse_index;
            while pulse_search_index < validpulses_len.saturating_sub(1) {
                if current_distance_from_pulse_to_line
                    <= smallest_distance_observed_from_pulse_to_line
                {
                    smallest_distance_observed_from_pulse_to_line =
                        current_distance_from_pulse_to_line;
                    current_pulse_index = pulse_search_index;
                    current_pulse_sample_location = validpulses[pulse_search_index];
                }

                let next_observed_distance_between_pulse_and_line =
                    (validpulses[pulse_search_index + 1] - expected_line_location).abs();
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

    (line_locations, line_location_errs)
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
    burstarea: (isize, isize),
    burst_wave: &[(f32, f32)],
    line_scale: f32,
    linenumber: usize,
    lineoffset: usize,
    outwidth: usize,
) -> Result<BurstMeasurement> {
    let burst_padding = burstarea.1 - burstarea.0;
    if burst_padding < 0 {
        bail!("burst area end precedes start");
    }
    let burst_padding = burst_padding as usize;

    let line_start = (linenumber as isize - lineoffset as isize) * outwidth as isize;
    let burst_start = 0.max(line_start + burstarea.0 - burst_padding as isize) as usize;
    let burst_end_limit = burst_start as isize + burstarea.1 + burst_padding as isize;
    let burst_end = chroma.len().min(0.max(burst_end_limit) as usize);
    if burst_start >= burst_end {
        bail!("empty burst window");
    }

    let heterodyne_row = chroma_heterodyne
        .get(current_phase)
        .context("heterodyne phase row is out of range")?;
    if burst_end > heterodyne_row.len() {
        bail!("burst window exceeds heterodyne row length");
    }

    let burst = (burst_start..burst_end)
        .map(|index| chroma[index] * heterodyne_row[index])
        .collect::<Vec<f32>>();

    let filtered_padded = sosfiltfilt_f32(chroma_filter, &burst);
    let filtered = if burst_padding == 0 {
        &filtered_padded[0..0]
    } else {
        let start = burst_padding.min(filtered_padded.len());
        let end = filtered_padded.len().saturating_sub(burst_padding);
        if start <= end {
            &filtered_padded[start..end]
        } else {
            &filtered_padded[0..0]
        }
    };

    demod_burst(
        filtered,
        burst_start + burst_padding,
        line_start as usize,
        line_scale,
        burst_wave,
    )
}

fn get_phase_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_rotation_starting_index: Option<i64>,
    burstarea: (isize, isize),
    track_change_threshold: f32,
) -> Result<(i64, Vec<PhaseSequenceEntry>)> {
    const BURST_CHECK_SKIP_LINES: usize = 16;

    let chroma_rotation = spec.decoder_chroma_rotation.map(|v| (v[0], v[1]));
    let do_phase_rotation_check = spec.rf_detect_chroma_track_phase && chroma_rotation.is_some();
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    let linelocs = field.linelocs.as_deref().context("missing linelocs")?;
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
            _ => bail!("chroma rotation index must be 0 or 1"),
        };
    } else {
        chroma_rotation_index = 0;
        track_rotation = starting_index;
    }

    // Length of the line relative to the nominal one, as the TBC resamples it.
    let line_scale = |linenumber: usize| -> f32 {
        if linenumber + 1 < last_line {
            (f64::from(linelocs[linenumber + 1] - linelocs[linenumber]) / field.inlinelen) as f32
        } else {
            1.0
        }
    };

    let upconvert = |phase: usize, linenumber: usize| {
        get_upconverted_burst(
            chroma,
            chroma_heterodyne,
            &spec.chroma_filter_final,
            phase,
            burstarea,
            &spec.rf_fsc_wave,
            line_scale(linenumber),
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
            upconvert(current_phase, linenumber)?
        };

        if do_phase_rotation_check
            && linenumber >= rotation_check_start_line
            && linenumber + 1 < last_line
        {
            let next_phase = rotated_phase(current_phase, track_rotation);
            let next_burst = upconvert(next_phase, linenumber + 1)?;

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

    Ok((chroma_rotation_index, phase_sequence))
}

fn get_phase_rotation_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_rotation_index: Option<i64>,
    burstarea: (isize, isize),
    prev_burst_start_line: i64,
) -> Result<(i64, BurstLock)> {
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
        burstarea,
        TRACK_CHANGE_THRESHOLD,
    )?;

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
            burstarea,
            TRACK_CHANGE_THRESHOLD,
        )?;
    }

    // Average burst phase per line parity (PAL bursts alternate), plus the
    // color killer state.
    let mut even = (0.0f32, 0.0f32);
    let mut odd = (0.0f32, 0.0f32);
    let mut count = 0usize;
    let mut magnitude_sum = 0.0f64;
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
        magnitude_sum += f64::from(burst.magnitude);
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
        (magnitude_sum / count as f64) as f32
    };
    if spec.rf_enable_color_killer && magnitude_avg < BURST_MAGNITUDE_THRESHOLD {
        burst_start_line = -1;
    }

    let phase_deg = |(i, q): (f32, f32)| q.atan2(i).to_degrees().rem_euclid(360.0);
    Ok((
        chroma_rotation_index,
        BurstLock {
            phase_sequence,
            burst_start_line,
            phase_avg: phase_deg((even.0 + odd.0, even.1 + odd.1)),
            even_phase_avg: phase_deg(even),
            odd_phase_avg: phase_deg(odd),
        },
    ))
}

/// Line locations refined from the burst phase, or `None` when burst hsync
/// refinement is disabled or the color killer blanks the field.
fn burst_refined_linelocs(field: &DecodedField, spec: &DecoderSpec) -> Option<Vec<f32>> {
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
    let fsc_ratio = (spec.sys_outfreq / spec.sys_fsc_mhz) as f32;
    // The first lines sit in vsync; after a mid-field burst turn-on only the
    // lines with color are refined.
    let start = lock.burst_start_line.max(9) as usize;
    for entry in lock.phase_sequence.iter().skip(start) {
        let line = entry.line_number;
        let target = if line % 2 == 1 {
            odd_target
        } else {
            even_target
        };
        let phase_delta = (target - entry.burst.phase + entry.burst.phase_offset + 180.0)
            .rem_euclid(360.0)
            - 180.0;
        let scale = (linelocs[line + 1] - linelocs[line]) / field.outlinelen as f32;
        linelocs[line] += phase_delta / 360.0 * fsc_ratio * scale;
    }
    Some(linelocs)
}

fn apply_burst_lock(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    inter_field_state: &mut InterFieldState,
    chroma_afc_state: &ChromaAfcState,
) -> Result<()> {
    let burstarea = padded_burst_area(spec);
    let chroma_heterodyne = active_chroma_heterodyne(spec, chroma_afc_state);
    let prev_burst_start_line = field
        .prevfield
        .as_ref()
        .map_or(0, |prev| prev.burst_start_line);

    let burst_downscaled = downscale_raw_vec(field, &spec.sinc_lut, None, None, None, true)?;
    let (track_phase, burst_lock) = get_phase_rotation_sequence(
        spec,
        field,
        &burst_downscaled,
        chroma_heterodyne,
        inter_field_state.track_phase,
        burstarea,
        prev_burst_start_line,
    )?;
    inter_field_state.track_phase = Some(track_phase);
    field.burst_lock = Some(burst_lock);
    Ok(())
}

pub(crate) fn predecode_field_from_rawdecode(
    rawdecode: FieldData,
    spec: &DecoderSpec,
    scheduled_prevfield: Option<PrevFieldState>,
    inter_field_state: &mut InterFieldState,
    scheduled_readloc: u64,
    resync_state: &mut ResyncState,
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
        linebad: None,
        nextfieldoffset: None,
        vblank_next: None,
        lt_vsync: None,
        is_progressive_field: None,
        field_number: 0,
        linelocs: None,
        lineoffset: 0,
        linecount: None,
        out_scale: None,
        field_phase_id: None,
        valid: false,
        sync_confidence: 100,
        burst_lock: None,
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

    let has_levels = resync_state.has_levels();
    let do_level_detect =
        !spec.rf_saved_levels || !has_levels || inter_field_state.compute_linelocs_issues;
    let mut res = try_get_pulses(
        &mut pending_field,
        spec,
        inter_field_state,
        do_level_detect,
        resync_state,
    )?;
    let needs_level_retry = match &res {
        None => true,
        Some(res) => res.line0loc.is_none() || pending_field.sync_confidence == 0,
    };
    if needs_level_retry && !do_level_detect {
        tracing::debug!("Search for pulses failed, re-checking levels");
        res = try_get_pulses(
            &mut pending_field,
            spec,
            inter_field_state,
            true,
            resync_state,
        )?;
    }

    inter_field_state.compute_linelocs_issues = true;
    if let Some(res) = res.as_ref() {
        let is_first_field = pending_field.is_first_field.unwrap_or(false);
        pending_field.linecount = Some(if is_first_field { 263 } else { 262 });

        // Number of lines to actually process. This is set so that the entire following
        // VSYNC is processed.
        let proclines = pending_field.outlinecount + pending_field.lineoffset + 10;

        if let Some(first_hsync_loc) = res.first_hsync_loc {
            let line0loc = res.line0loc.context("missing line0loc with first hsync")?;
            let first_hsync_loc_line = res
                .first_hsync_loc_line
                .context("missing first_hsync_loc_line")?;
            let input_len = pending_field.data.input_len as f32;
            let lastline = (input_len - line0loc) / res.meanlinelen - 1.0;
            if lastline < proclines as f32 {
                if pending_field.prevfield.is_some() {
                    tracing::info!(lastline, proclines, meanlinelen = res.meanlinelen, line0loc);
                    tracing::info!(
                        "Did not find the expected number of lines (lastline < proclines) , skipping a tiny bit"
                    );
                }
                pending_field.nextfieldoffset = Some(f64::from(
                    (line0loc - (res.meanlinelen * 20.0)).max(pending_field.inlinelen as f32),
                ));
            } else {
                let validpulses = pending_field
                    .validpulses
                    .iter()
                    .map(|&start| start as f32)
                    .collect::<Vec<_>>();
                let (linelocs_vec, mut linebad_vec) = valid_pulses_to_linelocs(
                    validpulses,
                    (first_hsync_loc as i64) as f32,
                    first_hsync_loc_line as i64,
                    res.meanlinelen,
                    proclines,
                );
                let nextfield = if let Some(vblank_next) = pending_field.vblank_next {
                    vblank_next - (pending_field.inlinelen * 8.0)
                } else {
                    f64::from(linelocs_vec[pending_field.outlinecount - 7])
                };
                if linebad_vec.iter().filter(|&&value| value != 0).count() < 30 {
                    inter_field_state.compute_linelocs_issues = false;
                } else if spec.rf_saved_levels {
                    tracing::debug!(
                        "Possible sync issues, re-running level detection on next field!"
                    );
                }
                pending_field.nextfieldoffset = Some(nextfield);

                let linelocs2_vec = if !spec.rf_skip_hsync_refine {
                    let normal_hsync_length = usectoinpx(
                        spec.linelen() as f64,
                        spec.samplesperline(),
                        pending_field.linecount,
                        pending_field.lineoffset,
                        spec.sys_hsync_pulse_us,
                        None,
                        None,
                    ) as usize;
                    refine_linelocs_hsync(
                        spec,
                        &linelocs_vec,
                        &pending_field.data.video.demod_05,
                        &mut linebad_vec,
                        normal_hsync_length,
                        resync_state.last_pulse_threshold(),
                    )?
                } else {
                    linelocs_vec.clone()
                };
                pending_field.linebad = Some(linebad_vec.clone());
                pending_field.valid = true;

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
                        let shift33 = 83.0 * (std::f64::consts::PI / 180.0);
                        let shift = (-shift33 * (spec.freq / (4.0 * 315.0 / 88.0))) as f32;
                        if let Some(linelocs) = pending_field.linelocs.as_mut() {
                            for value in linelocs {
                                *value += shift;
                            }
                        }
                    }
                }
            }
        } else {
            tracing::warn!("Unable to determine start of field - dropping field");
            pending_field.nextfieldoffset = Some(pending_field.inlinelen * 100.0);
        }
    } else {
        tracing::warn!("Unable to find any sync pulses, jumping 100 ms");
        pending_field.nextfieldoffset = Some(spec.freq_hz() / 10.0);
    }
    let mut pending_offset = pending_field
        .nextfieldoffset
        .context("missing nextfieldoffset")?;
    if pending_field.valid {
        pending_offset -= scheduled_readloc as f64 - pending_field.data.startloc as f64;
    }
    Ok(DecodeFieldResult {
        field: pending_field,
        offset: pending_offset,
    })
}
