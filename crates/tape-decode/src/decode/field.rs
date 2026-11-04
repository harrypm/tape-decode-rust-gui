use super::sync::{refine_levels_from_vsync, try_get_pulses};
use super::*;

/// Measures the burst kept at output sample `burst_start` of the field
/// buffer against the reference subcarrier table.
fn demod_burst(burst: &[f32], burst_start: usize, burst_wave: &[(f32, f32)]) -> BurstMeasurement {
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

    // Over whole subcarrier cycles the reference cosine and sine are
    // orthogonal to each other and to a constant, so the correlations are
    // the least-squares amplitude, phase and dc of the burst.
    let len = burst.len() as f64;
    let magnitude = f64::from(i).hypot(f64::from(q));
    BurstMeasurement {
        phase: f64::from(q)
            .atan2(f64::from(i))
            .to_degrees()
            .rem_euclid(360.0) as f32,
        amplitude: (2.0 * magnitude / len) as f32,
        magnitude: magnitude as f32,
        dc: (f64::from(sum) / len) as f32,
    }
}

/// The sub-sample position of the first crossing of `target` within `count`
/// samples from `start_offset` of the field buffer, rising for `edge` 1,
/// falling for -1, whichever the first sample is on the far side of for 0;
/// NaN when the first sample is already past the level or no crossing
/// follows.
fn calczc_do(data: &[f32], start_offset: isize, target: f32, count: isize, mut edge: i64) -> f64 {
    // The crossing is interpolated from the sample before it, so the search
    // starts at index 1 at the earliest.
    let start = start_offset.max(1) as usize;
    assert!(
        start < data.len(),
        "the search starts inside the field buffer, which holds every processed line"
    );

    if edge == 0 {
        edge = if data[start] < target { 1 } else { -1 };
    }

    if (edge == 1 && data[start] > target) || (edge == -1 && data[start] < target) {
        return f64::NAN;
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
        return f64::NAN;
    };

    let x = start + loc;
    let a = data[x - 1] - target;
    let b = data[x] - target;
    let y = if b - a != 0.0 { -a / (-a + b) } else { 0.0 };

    x as f64 - 1.0 + f64::from(y)
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
) -> Vec<f64> {
    let one_usec = spec.freq as usize;
    let is_pal = spec.sys_frame_lines != LineSystem::Line525;
    let zc_threshold = levels.iretohz(spec.sys_vsync_ire / 2.0);
    let ire_30 = levels.iretohz(30.0);
    let ire_n_65 = levels.iretohz(-65.0);
    let ire_110 = levels.iretohz(110.0);

    let mut linelocs_refined = initial_linelocs.to_vec();
    let mut prev_porch_level = -1.0f32;
    let one_usec_samples = spec.freq;
    let normal_hsync_samples = normal_hsync_length as f64;

    for i in 0..initial_linelocs.len() {
        if (3..=6).contains(&i) || (is_pal && (1..=2).contains(&i)) {
            linebad[i] = 1;
            continue;
        }

        let ll1 = round_ties_even_to_isize(initial_linelocs[i]) - one_usec as isize;
        let zc = calczc_do(demod_05, ll1, zc_threshold, (one_usec * 2) as isize, 0);

        let right_cross = if spec.rf_disable_right_hsync {
            f64::NAN
        } else {
            calczc_do(
                demod_05,
                ll1 + normal_hsync_length as isize - one_usec as isize,
                zc_threshold,
                (normal_hsync_length * 2) as isize,
                1,
            )
        };
        // The line start a sync pulse length before the rising edge's
        // refined crossing.
        let mut refined_from_right = None;

        if !zc.is_nan() {
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

                let zc2 = calczc_do(demod_05, ll1, (porch_level + sync_level) / 2.0, 400, 0);
                if !zc2.is_nan() && (zc2 - zc).abs() < (one_usec_samples / 2.0) {
                    linelocs_refined[i] = zc2;
                    prev_porch_level = porch_level;
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
                );

                if !zc2.is_nan() && (zc2 - right_cross).abs() < (one_usec_samples / 2.0) {
                    let from_right = zc2 - normal_hsync_samples + (2.25 * (spec.freq / 40.0));
                    if (from_right - linelocs_refined[i]).abs() < (one_usec_samples * 2.0) {
                        refined_from_right = Some(from_right);
                        prev_porch_level = porch_level;
                    }
                }
            }
        }

        if linebad[i] != 0 {
            linelocs_refined[i] = initial_linelocs[i];
        }

        if let Some(from_right) = refined_from_right {
            linebad[i] = 0;
            linelocs_refined[i] = from_right;
        }
    }

    linelocs_refined
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
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    chroma_filter: &[Sos<f32>],
    current_phase: usize,
    burst_area: (usize, usize),
    burst_wave: &[(f32, f32)],
    line_number: usize,
) -> BurstMeasurement {
    // Upconvert and filter from the line start to as far past the burst as
    // the burst sits from the line start, so the filter transients fall
    // outside the kept window.
    let (burst_start, burst_end) = burst_area;
    let line_start = field.output_row(line_number) * field.outlinelen;
    let window_end = line_start + burst_end + burst_start;
    let upconverted = chroma[line_start..window_end]
        .iter()
        .zip(&chroma_heterodyne[current_phase][line_start..window_end])
        .map(|(&sample, &het)| sample * het)
        .collect::<Vec<f32>>();
    let filtered = sosfiltfilt_f32(chroma_filter, &upconverted);

    demod_burst(&filtered[burst_start..burst_end], line_start + burst_start, burst_wave)
}

fn get_phase_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    starting_index: i64,
    burst_area: (usize, usize),
    track_change_threshold: f32,
) -> (i64, Vec<PhaseSequenceEntry>) {
    const BURST_CHECK_SKIP_LINES: usize = 16;

    let chroma_rotation = spec.decoder_chroma_rotation.map(|v| (v[0], v[1]));
    let do_phase_rotation_check = spec.rf_detect_chroma_track_phase && chroma_rotation.is_some();
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    let output_lines = field.output_lines();
    let rotation_check_start_line = output_lines.end - BURST_CHECK_SKIP_LINES;
    // The heterodyne phase rotation per line of the track read with `index`;
    // none without chroma rotation.
    let rotation_of = |index: i64| {
        chroma_rotation.map_or(0, |(first, second)| if index == 0 { first } else { second })
    };
    let mut chroma_rotation_index = starting_index;
    let mut track_rotation = rotation_of(starting_index);

    let upconvert = |phase: usize, linenumber: usize| {
        get_upconverted_burst(
            field,
            chroma,
            chroma_heterodyne,
            &spec.chroma_filter_final,
            phase,
            burst_area,
            &spec.rf_fsc_wave,
            linenumber,
        )
    };

    let mut phase_sequence: Vec<PhaseSequenceEntry> = Vec::with_capacity(output_lines.len());
    let mut current_phase = 0usize;
    // The next line's burst, when the rotation check already measured it.
    let mut next: Option<(usize, BurstMeasurement)> = None;

    for linenumber in output_lines.clone() {
        let current_burst = if let Some((phase, burst)) = next.take() {
            current_phase = phase;
            burst
        } else {
            current_phase = rotated_phase(current_phase, track_rotation);
            upconvert(current_phase, linenumber)
        };

        if do_phase_rotation_check
            && linenumber >= rotation_check_start_line
            && linenumber + 1 < output_lines.end
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
                track_rotation = rotation_of(chroma_rotation_index);
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

    // The heads alternate: the next field is read with the other index
    // unless the switch was already seen mid-field.
    if chroma_rotation.is_some() && chroma_rotation_index == starting_index {
        chroma_rotation_index = (chroma_rotation_index + 1).rem_euclid(2);
    }

    (chroma_rotation_index, phase_sequence)
}

/// Locks the heterodyne to the field's bursts starting from the chroma
/// track phase `start_index`, checking that index against the burst phase
/// steps between lines. Returns the index the field was read with, the one
/// the next field starts from, and the lock.
fn get_phase_rotation_sequence(
    spec: &DecoderSpec,
    field: &DecodedField,
    chroma: &[f32],
    chroma_heterodyne: &[Vec<f32>],
    start_index: i64,
    burst_area: (usize, usize),
    prev_burst_start_line: i64,
) -> (i64, i64, BurstLock) {
    const TRACK_CHANGE_THRESHOLD: f32 = 90.0;
    const BURST_CHECK_SKIP_LINES: isize = 16;
    // Field-average burst magnitude below which the color killer blanks the
    // field. Set on the 16-bit sample scale, which the reader normalizes by
    // 32768; a healthy burst measures a few times this.
    const BURST_MAGNITUDE_THRESHOLD: f32 = 2.5e4 / 32768.0;

    let end = field.output_lines().end;
    let chroma_rotation = spec.decoder_chroma_rotation.map(|v| (v[0], v[1]));
    let is_ntsc = spec.color_system == ColorSystem::Ntsc;
    let mut start_index = start_index;
    let (mut next_index, mut phase_sequence) = get_phase_sequence(
        spec,
        field,
        chroma,
        chroma_heterodyne,
        start_index,
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
        start_index = next_index;
        (next_index, phase_sequence) = get_phase_sequence(
            spec,
            field,
            chroma,
            chroma_heterodyne,
            start_index,
            burst_area,
            TRACK_CHANGE_THRESHOLD,
        );
    }

    // The color killer state, from the lines clear of the vertical interval.
    let measured = |entry: &PhaseSequenceEntry| {
        let line_number = entry.line_number as isize;
        line_number > burst_check_start
            && line_number < burst_check_end
            && entry.burst.magnitude != 0.0
    };
    let mut count = 0usize;
    let mut magnitude_sum = 0.0f32;
    let mut burst_start_line = 0i64;
    for entry in phase_sequence.iter().filter(|entry| measured(entry)) {
        count += 1;
        magnitude_sum += entry.burst.magnitude;
        // After a blanked field, color resumes at the first line whose burst
        // clears the threshold: broadcasts could switch the burst on
        // mid-field.
        if spec.rf_enable_color_killer
            && prev_burst_start_line == -1
            && burst_start_line == 0
            && entry.burst.magnitude > BURST_MAGNITUDE_THRESHOLD
        {
            burst_start_line = entry.line_number as i64;
        }
    }
    let magnitude_avg = if count == 0 {
        0.0
    } else {
        magnitude_sum / count as f32
    };
    if spec.rf_enable_color_killer && magnitude_avg < BURST_MAGNITUDE_THRESHOLD {
        burst_start_line = -1;
    }

    (
        start_index,
        next_index,
        BurstLock {
            phase_sequence,
            burst_start_line,
        },
    )
}

/// Half the window of the burst phase trend, in lines: wide enough to
/// average the line jitter out of the trend, short enough that the carrier's
/// wander within it stays linear.
const TREND_HALF_WINDOW: usize = 16;

/// Trend of the burst phase at the reliable burst `at`, in degrees, from the
/// `(line, phase)` pairs of the reliable bursts in line order, whose phases
/// repeat every `stride` lines (two for the alternating bursts of the PAL
/// family). A color-under carrier off its nominal frequency, or wandering,
/// advances the upconverted burst against the reference subcarrier by a
/// slowly varying amount per line, which is no timing error, while a line's
/// departure from the trend is: the local slope is the circular mean of the
/// advances between consecutive bursts of a parity within the window (both
/// parities share the one carrier), and the trend the circular mean of the
/// parity's bursts in the window carried to the line by that slope.
fn burst_phase_trend(reliable: &[(usize, f32)], stride: usize, at: usize) -> f64 {
    let line = reliable[at].0;
    let near = |other: usize| other.abs_diff(line) <= TREND_HALF_WINDOW;
    let (mut cos, mut sin) = (0.0f64, 0.0f64);
    for (index, &(first, phase)) in reliable.iter().enumerate() {
        if !near(first) {
            continue;
        }
        let successor = reliable[index + 1..]
            .iter()
            .find(|&&(next, _)| next >= first + stride);
        if let Some(&(next, next_phase)) = successor {
            if next == first + stride && near(next) {
                let (advance_sin, advance_cos) = f64::from(next_phase - phase).to_radians().sin_cos();
                cos += advance_cos;
                sin += advance_sin;
            }
        }
    }
    // Zero without a pair, which atan2 gives of the zero sums.
    let slope = sin.atan2(cos).to_degrees() / stride as f64;

    let (mut cos, mut sin) = (0.0f64, 0.0f64);
    for &(other, phase) in reliable
        .iter()
        .filter(|&&(other, _)| near(other) && other % stride == line % stride)
    {
        let carried = f64::from(phase) + slope * (line as f64 - other as f64);
        let (carried_sin, carried_cos) = carried.to_radians().sin_cos();
        cos += carried_cos;
        sin += carried_sin;
    }
    sin.atan2(cos).to_degrees()
}

/// Line locations refined from the burst phase, or `None` when burst hsync
/// refinement is disabled, the color killer blanks the field or the field's
/// bursts carry no timing. A reliable burst's departure from the trend is
/// the line's hsync timing error together with the burst's own phase
/// noise, which the field tells apart: the true line spacing varies slowly,
/// so the second differences of the line grid are noise, and the lines are
/// moved by the share of their departures that minimizes them, the
/// least-squares estimate of the hsync's part. The other lines keep their
/// hsync locations.
fn burst_refined_linelocs(field: &DecodedField, spec: &DecoderSpec) -> Option<Vec<f64>> {
    // A burst this far below the field's median is blanked or in a dropout.
    const RELIABLE_MAGNITUDE_RATIO: f32 = 0.5;
    // Least mean resultant length of the departures' phasors for them to be
    // timing rather than noise: random phases give about zero, a jitter of
    // 30 degrees rms 0.87.
    const MIN_COHERENCE: f64 = 0.5;
    // A correction this many MADs beyond the field's median is a burst the
    // trend does not describe (the other head's across the head switch, a
    // phase step), not the line's timing.
    const OUTLIER_MADS: f64 = 5.0;

    if spec.rf_disable_burst_hsync {
        return None;
    }
    let lock = field
        .burst_lock
        .as_ref()
        .filter(|lock| lock.burst_start_line != -1)?;
    let mut linelocs = field.linelocs.clone()?;
    // Output samples per colour-under carrier cycle. The recorded burst sits
    // at the colour-under carrier and the heterodyne runs on across the
    // field, so a line timing error moves the upconverted burst's phase by
    // the carrier's phase over it: a phase error maps to that many output
    // samples per cycle, scaled back to the line's input length.
    let carrier_ratio = spec.tbc_sample_rate() / field.chroma_carrier;
    // The vertical interval's lines, up to 9, carry no burst; after a
    // mid-field burst turn-on only the lines with color are refined.
    let first_line = (lock.burst_start_line as usize).max(10);
    let entries = lock
        .phase_sequence
        .iter()
        .filter(|entry| entry.line_number >= first_line);
    let mut magnitudes: Vec<f32> = entries.clone().map(|entry| entry.burst.magnitude).collect();
    let magnitude_floor = median_from_values(&mut magnitudes) * RELIABLE_MAGNITUDE_RATIO;
    let reliable: Vec<(usize, f32)> = entries
        .filter(|entry| entry.burst.magnitude >= magnitude_floor)
        .map(|entry| (entry.line_number, entry.burst.phase))
        .collect();
    let stride = if spec.color_system == ColorSystem::Ntsc {
        1
    } else {
        2
    };
    // Each reliable line's departure from the trend, in degrees.
    let departures: Vec<(usize, f64)> = (0..reliable.len())
        .map(|at| {
            let (line, phase) = reliable[at];
            let trend = burst_phase_trend(&reliable, stride, at);
            (
                line,
                (trend - f64::from(phase) + 180.0).rem_euclid(360.0) - 180.0,
            )
        })
        .collect();
    let (cos, sin) = departures
        .iter()
        .fold((0.0f64, 0.0f64), |(cos, sin), &(_, departure)| {
            let (departure_sin, departure_cos) = departure.to_radians().sin_cos();
            (cos + departure_cos, sin + departure_sin)
        });
    if cos.hypot(sin) < MIN_COHERENCE * departures.len() as f64 {
        return None;
    }
    // Each reliable line's correction, in input samples.
    let mut corrections = vec![0.0f64; linelocs.len()];
    for &(line, departure) in &departures {
        let scale = (linelocs[line + 1] - linelocs[line]) / field.outlinelen as f64;
        corrections[line] = departure / 360.0 * carrier_ratio * scale;
    }
    let mut magnitudes: Vec<f64> = departures
        .iter()
        .map(|&(line, _)| corrections[line].abs())
        .collect();
    let median = median_from_values(&mut magnitudes);
    let mut deviations: Vec<f64> = magnitudes
        .iter()
        .map(|magnitude| (magnitude - median).abs())
        .collect();
    let limit = median + OUTLIER_MADS * median_from_values(&mut deviations);
    for &(line, _) in &departures {
        if corrections[line].abs() > limit {
            corrections[line] = 0.0;
        }
    }
    // The gain that minimizes the second differences of the corrected grid
    // over the refined lines.
    let (first, last) = (departures.first()?.0, departures.last()?.0);
    let (mut cross, mut energy) = (0.0f64, 0.0f64);
    for line in first..last.saturating_sub(1) {
        let grid = linelocs[line + 2] - 2.0 * linelocs[line + 1] + linelocs[line];
        let correction = corrections[line + 2] - 2.0 * corrections[line + 1] + corrections[line];
        cross += grid * correction;
        energy += correction * correction;
    }
    if energy == 0.0 {
        return None;
    }
    let gain = (-cross / energy).clamp(0.0, 1.0);
    for (location, correction) in linelocs.iter_mut().zip(&corrections) {
        *location += gain * correction;
    }
    Some(linelocs)
}

fn apply_burst_lock(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    inter_field_state: &mut InterFieldState,
) {
    let prev_burst_start_line = field
        .prevfield
        .as_ref()
        .map_or(0, |prev| prev.burst_start_line);

    let (burst_start, burst_end) = spec.burst_area;
    let chroma = if spec.chroma_afc_enabled() {
        // The carrier is measured over the whole field's chroma and the
        // field's heterodyne generated at it.
        let chroma = downscale_raw_vec(field, &spec.sinc_lut, true, 0.0, None);
        let carrier = chroma_afc_carrier(spec, &chroma);
        field.chroma_carrier = carrier;
        field.chroma_heterodyne = Some(color_under_heterodyne(spec, carrier));
        chroma
    } else {
        // The burst lock reads each line only as far past the burst as the
        // burst sits from the line start (see `get_upconverted_burst`).
        downscale_raw_vec(
            field,
            &spec.sinc_lut,
            true,
            0.0,
            Some(burst_start + burst_end),
        )
    };
    let (track_phase, next_track_phase, burst_lock) = get_phase_rotation_sequence(
        spec,
        field,
        &chroma,
        field.heterodyne(spec),
        inter_field_state.track_phase,
        (burst_start, burst_end),
        prev_burst_start_line,
    );
    inter_field_state.track_phase = next_track_phase;
    field.track_phase = Some(track_phase);
    field.burst_lock = Some(burst_lock);
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
) -> DecodeFieldResult {
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
        linelocs: None,
        lineoffset: 0,
        linecount: None,
        out_scale: None,
        field_phase_id: None,
        valid: false,
        levels: inter_field_state.levels,
        burst_lock: None,
        track_phase: None,
        chroma_carrier: spec.decoder_color_under_carrier,
        chroma_heterodyne: None,
        wow_analysis: None,
    };
    let do_level_detect = !spec.rf_saved_levels || inter_field_state.compute_linelocs_issues;
    let mut res = try_get_pulses(&mut pending_field, spec, inter_field_state, do_level_detect);
    let needs_level_retry = res.as_ref().is_none_or(|res| res.first_hsync.is_none());
    if needs_level_retry && !do_level_detect {
        tracing::debug!("Search for pulses failed, re-checking levels");
        res = try_get_pulses(&mut pending_field, spec, inter_field_state, true);
    }

    let next_field = if let Some(res) = res.as_ref() {
        let is_first_field = pending_field.is_first_field.unwrap_or(false);
        let is_525 = spec.sys_frame_lines == LineSystem::Line525;
        // The output geometry follows the line system and the field order.
        pending_field.linecount = Some(spec.sys_field_lines[usize::from(!is_first_field)] as usize);
        if is_525 {
            pending_field.out_scale =
                Some(f64::from(0xC800 - 0x0400) / (100.0 - f64::from(spec.sys_vsync_ire)));
        } else {
            pending_field.lineoffset = if is_first_field { 2 } else { 3 };
            pending_field.out_scale =
                Some(f64::from(0xD300 - 0x0100) / (100.0 - f64::from(spec.sys_vsync_ire)));
            pending_field.field_phase_id = Some(1);
        }

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
                    )
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

                pending_field.linelocs = Some(linelocs2_vec);
                if spec.rf_write_chroma {
                    apply_burst_lock(&mut pending_field, spec, inter_field_state);
                    if let Some(refined) = burst_refined_linelocs(&pending_field, spec) {
                        pending_field.linelocs = Some(refined);
                    }
                }
                if is_525 {
                    // Subcarrier phase offset that puts the downscaled image on
                    // the I/Q colour axes (calibrated for ~147 degrees of output
                    // burst phase).
                    let fsc_phase_deg = 117.25;
                    let shift = -((fsc_phase_deg / 360.0) / spec.sys_fsc_mhz * spec.freq);
                    for value in pending_field.linelocs.iter_mut().flatten() {
                        *value += shift;
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
    DecodeFieldResult {
        field: pending_field,
        offset,
    }
}
