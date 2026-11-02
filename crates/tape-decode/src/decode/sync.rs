use super::*;

/// Vertical-sync pulse classification. State order across a field boundary is
/// HSYNC -> EQPL1 -> VSYNC -> EQPL2 -> HSYNC.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PulseType {
    Hsync,
    Eqpl1,
    Vsync,
    Eqpl2,
}

fn pulse_qualitycheck(
    prev_pulse_type: PulseType,
    prev_pulse_start: i64,
    pulse_type: PulseType,
    pulse_start: i64,
    in_line_len: f64,
) -> bool {
    let exprange = if prev_pulse_type != PulseType::Hsync && pulse_type != PulseType::Hsync {
        (0.4, 0.6)
    } else if prev_pulse_type == PulseType::Hsync && pulse_type == PulseType::Hsync {
        (0.9, 1.1)
    } else {
        (0.4, 1.1)
    };

    let linelen = (pulse_start - prev_pulse_start) as f64 / in_line_len;
    inrange(linelen, exprange.0, exprange.1)
}

fn run_vblank_state_machine(
    raw_pulse_starts: &[i64],
    raw_pulse_lengths: &[i64],
    hsync: (f64, f64),
    eq: (f64, f64),
    vsync: (f64, f64),
    num_pulses: f64,
    in_line_len: f64,
) -> (bool, Vec<PulseType>, Vec<i64>, Vec<bool>) {
    // Look though raw_pulses for a set valid vertical sync pulse series.
    // num_pulses_half: number of equalization pulses per section / 2
    let mut done = false;
    let num_pulses_half = num_pulses / 2.0;

    let mut valid_types = Vec::new();
    let mut valid_starts = Vec::new();
    let mut valid_good = Vec::new();

    // state_end tracks the earliest expected phase transition...
    let mut state_end = 0.0;
    // ... and state length is set by the phase transition to set above (in H)
    let mut state_length: Option<f64> = None;

    for (&start, &length) in raw_pulse_starts.iter().zip(raw_pulse_lengths.iter()) {
        let mut spulse_type = None;
        // `None` is the start state, before any valid pulse has been classified.
        let state = valid_types.last().copied();
        let length_f = length as f64;

        if state.is_none() {
            // First valid pulse must be a regular HSYNC
            if inrange(length_f, hsync.0, hsync.1) {
                spulse_type = Some(PulseType::Hsync);
            }
        } else if state == Some(PulseType::Hsync) {
            // HSYNC can transition to EQPUL/pre-vsync at the end of a field
            if inrange(length_f, hsync.0, hsync.1) {
                spulse_type = Some(PulseType::Hsync);
            } else if inrange(length_f, eq.0, eq.1) {
                spulse_type = Some(PulseType::Eqpl1);
                state_length = Some(num_pulses_half);
            } else if inrange(length_f, vsync.0, vsync.1) {
                // should not happen(tm)
                spulse_type = Some(PulseType::Vsync);
            }
        } else if state == Some(PulseType::Eqpl1) {
            if inrange(length_f, eq.0, eq.1) {
                spulse_type = Some(PulseType::Eqpl1);
            } else if inrange(length_f, vsync.0, vsync.1) {
                // transition to the first VSYNC pulse
                spulse_type = Some(PulseType::Vsync);
                state_length = Some(num_pulses_half);
            } else if inrange(length_f, hsync.0, hsync.1) {
                // previous state transition was likely in error!
                spulse_type = Some(PulseType::Hsync);
            }
        } else if state == Some(PulseType::Vsync) {
            if inrange(length_f, eq.0, eq.1) {
                // transition to the first EQ pulse after VSYNC
                spulse_type = Some(PulseType::Eqpl2);
                state_length = Some(num_pulses_half);
            } else if inrange(length_f, vsync.0, vsync.1) {
                spulse_type = Some(PulseType::Vsync);
            } else if start as f64 > state_end && inrange(length_f, hsync.0, hsync.1) {
                spulse_type = Some(PulseType::Hsync);
            }
        } else if state == Some(PulseType::Eqpl2) {
            if inrange(length_f, eq.0, eq.1) {
                spulse_type = Some(PulseType::Eqpl2);
            } else if inrange(length_f, hsync.0, hsync.1) {
                spulse_type = Some(PulseType::Hsync);
                done = true;
            }
        }

        if let Some(pulse_type) = spulse_type {
            if Some(pulse_type) != state {
                if (start as f64) < state_end {
                    spulse_type = None;
                } else if let Some(length) = state_length.take() {
                    state_end = start as f64 + ((length - 0.1) * in_line_len);
                }
            }
        }

        // Quality check
        if let Some(pulse_type) = spulse_type {
            let good = valid_types.last().zip(valid_starts.last()).is_some_and(
                |(&prev_type, &prev_start)| {
                    pulse_qualitycheck(prev_type, prev_start, pulse_type, start, in_line_len)
                },
            );

            valid_types.push(pulse_type);
            valid_starts.push(start);
            valid_good.push(good);
        }

        if done {
            return (done, valid_types, valid_starts, valid_good);
        }
    }

    (done, valid_types, valid_starts, valid_good)
}

fn round_nearest_line_loc(line_number: f64) -> f64 {
    let rounded = (line_number / 0.5).round_ties_even();
    (0.5 * rounded * 10.0).round_ties_even() / 10.0
}

#[derive(Default)]
struct SyncDistanceResult {
    distance_offset: f64,
    hsync_loc: f64,
    valid_locations: usize,
}

fn calc_sync_from_known_distances(
    meanlinelen: f64,
    vsync_tolerance_lines: f64,
    hsync_start_line: f64,
    first_pulse: f64,
    second_pulse: f64,
    first_line: f64,
    second_line: f64,
) -> SyncDistanceResult {
    let mut output = SyncDistanceResult::default();

    if first_pulse != -1.0 && second_pulse != -1.0 && meanlinelen != 0.0 {
        let actual_lines = (first_pulse - second_pulse) / meanlinelen;
        let expected_lines = first_line - second_line;

        if actual_lines < expected_lines + vsync_tolerance_lines
            && actual_lines > expected_lines - vsync_tolerance_lines
        {
            output.distance_offset = actual_lines - expected_lines;
            output.hsync_loc = second_pulse + meanlinelen * (hsync_start_line - second_line);
            output.valid_locations = 1;
        }
    }

    output
}

struct GetFirstHsyncLocResult {
    line0loc: Option<f64>,
    first_hsync_loc: Option<f64>,
    hsync_start_line: f64,
    next_field: Option<f64>,
    first_field: bool,
    progressive_field: bool,
    prev_hsync_diff: f64,
}

/// Scan the valid pulse train and, for the leading and trailing vblank sections,
/// record the pulse start positions (`vblank_pulses`) and the rounded inter-pulse
/// line lengths used to detect field order (`field_order_lengths`).
fn measure_vblank_sections(
    validpulses_type: &[PulseType],
    validpulses_start: &[f64],
    validpulses_valid: &[bool],
    meanlinelen: f64,
    field_lines: [f64; 2],
) -> ([f64; 8], [f64; 4]) {
    let mut field_order_lengths = [-1.0; 4];
    let mut vblank_pulses = [-1.0; 8];

    let mut last_pulse = -1isize;
    let mut group = 0usize;
    let mut field_group = 0usize;

    for i in 0..validpulses_type.len() {
        if last_pulse != -1 && validpulses_valid[i] {
            let last_pulse_index = last_pulse as usize;
            if group == 0
                && validpulses_start[i] > validpulses_start[0] + field_lines[0] * meanlinelen
            {
                group = 4;
                field_group = 2;
            }

            if validpulses_type[last_pulse_index] == PulseType::Hsync
                && validpulses_type[i] != PulseType::Hsync
            {
                vblank_pulses[group] = validpulses_start[i];
                field_order_lengths[field_group] = round_nearest_line_loc(
                    (validpulses_start[i] - validpulses_start[last_pulse_index]) / meanlinelen,
                );
            } else if validpulses_type[last_pulse_index] == PulseType::Eqpl1
                && validpulses_type[i] == PulseType::Vsync
            {
                vblank_pulses[1 + group] = validpulses_start[i];
            } else if validpulses_type[last_pulse_index] == PulseType::Vsync
                && validpulses_type[i] == PulseType::Eqpl2
            {
                vblank_pulses[2 + group] = validpulses_start[i];
            } else if validpulses_type[last_pulse_index] != PulseType::Hsync
                && validpulses_type[i] == PulseType::Hsync
            {
                vblank_pulses[3 + group] = validpulses_start[last_pulse_index];
                field_order_lengths[1 + field_group] = round_nearest_line_loc(
                    (validpulses_start[i] - validpulses_start[last_pulse_index]) / meanlinelen,
                );
            }
        }

        last_pulse = i as isize;
    }

    (vblank_pulses, field_order_lengths)
}

/// Expected field-order signatures, each [first HSYNC, first EQPL2, last HSYNC,
/// last EQPL2], returned as (first field, second field, progressive field).
fn field_order_signatures(is_ntsc: bool) -> ([f64; 4], [f64; 4], [f64; 4]) {
    if is_ntsc {
        (
            [1.0, 0.5, 0.5, 1.0],
            [0.5, 1.0, 1.0, 0.5],
            [1.0, 0.5, 1.0, 0.5],
        )
    } else {
        (
            [0.5, 0.5, 1.0, 1.0],
            [1.0, 1.0, 0.5, 0.5],
            [0.5, 0.5, 0.5, 0.5],
        )
    }
}

/// Decide field order, and whether the field is progressive, by scoring the
/// measured `field_order_lengths` against the expected signatures. The interlaced
/// consensus is weighed against the caller's confidence floor, with a VSYNC
/// fallback hint breaking ties when it is more confident than either field.
fn decide_field_order(
    field_order_lengths: [f64; 4],
    is_ntsc: bool,
    inter_field_state: &InterFieldState,
    mut field_order_confidence: i64,
    fallback: Option<Line0FallbackResult>,
) -> (bool, bool) {
    let (fallback_line0loc, fallback_is_first_field, fallback_is_first_field_confidence) =
        match fallback {
            Some(r) => (r.line0, r.first_field, r.first_field_confidence),
            None => (-1.0, -1, -1),
        };

    let mut progressive_field = false;
    let (first_field_lengths, second_field_lengths, progressive_field_lengths) =
        field_order_signatures(is_ntsc);

    let mut interlaced_field_boundaries_consensus = 0;
    let mut interlaced_field_boundaries_detected = 0;
    let mut progressive_field_consensus = 0;
    let mut progressive_field_boundaries_detected = 0;

    for i in 0..field_order_lengths.len() {
        let field_length = field_order_lengths[i];

        if field_length == first_field_lengths[i] {
            interlaced_field_boundaries_consensus += 1;
            interlaced_field_boundaries_detected += 1;
        }

        if field_length == second_field_lengths[i] {
            interlaced_field_boundaries_detected += 1;
        }

        if field_length == progressive_field_lengths[i] {
            progressive_field_consensus += 1;
            progressive_field_boundaries_detected += 1;
        }

        if field_length != -1.0 {
            progressive_field_boundaries_detected += 1;
        }
    }

    let mut first_field = if inter_field_state.prev_first_field == -1 {
        interlaced_field_boundaries_detected == 0
            || (interlaced_field_boundaries_consensus as f64
                / interlaced_field_boundaries_detected as f64)
                .round_ties_even()
                == 1.0
            || fallback_is_first_field == 1
    } else {
        inter_field_state.prev_first_field == 0
    };

    let mut first_field_confidence = 0;
    let mut second_field_confidence = 0;
    let interlaced_field_order_weighting =
        interlaced_field_boundaries_detected as f64 / field_order_lengths.len() as f64;

    let progressive_field_order_weighting =
        progressive_field_boundaries_detected as f64 / field_order_lengths.len() as f64;
    if interlaced_field_boundaries_detected > 0 {
        if fallback_line0loc == -1.0 && inter_field_state.prev_first_hsync_loc < 0.0 {
            field_order_confidence = field_order_confidence.min(50);
        }

        first_field_confidence = ((interlaced_field_boundaries_consensus as f64
            / interlaced_field_boundaries_detected as f64)
            * interlaced_field_order_weighting
            * 100.0)
            .round_ties_even() as i64;
        second_field_confidence = (((interlaced_field_boundaries_detected
            - interlaced_field_boundaries_consensus) as f64
            / interlaced_field_boundaries_detected as f64)
            * interlaced_field_order_weighting
            * 100.0)
            .round_ties_even() as i64;

        if first_field_confidence >= field_order_confidence
            && first_field_confidence > second_field_confidence
        {
            first_field = true;
        } else if second_field_confidence >= field_order_confidence
            && first_field_confidence < second_field_confidence
        {
            first_field = false;
        }

        if progressive_field_boundaries_detected > 0 {
            let progressive_field_confidence =
                (((progressive_field_boundaries_detected - progressive_field_consensus) as f64
                    / progressive_field_boundaries_detected as f64)
                    * progressive_field_order_weighting
                    * 100.0)
                    .round_ties_even() as i64;

            if progressive_field_confidence == field_order_lengths.len() as i64 {
                progressive_field = true;
            }
        }
    }

    if fallback_is_first_field_confidence > first_field_confidence
        && fallback_is_first_field_confidence > second_field_confidence
    {
        first_field = fallback_is_first_field == 1;
    }

    (first_field, progressive_field)
}

fn get_first_hsync_loc(
    validpulses_type: &[PulseType],
    validpulses_start: &[f64],
    validpulses_valid: &[bool],
    meanlinelen: f64,
    is_ntsc: bool,
    field_lines: [f64; 2],
    num_eq_pulses: f64,
    inter_field_state: &InterFieldState,
    last_field_offset_lines: f64,
    field_order_confidence: i64,
    fallback: Option<Line0FallbackResult>,
) -> GetFirstHsyncLocResult {
    // Only the fallback line-0 location is consulted below; the field-order hints
    // are consumed inside `decide_field_order`. -1.0 means "no fallback".
    let fallback_line0loc = fallback.map_or(-1.0, |r| r.line0);
    const VSYNC_TOLERANCE_LINES: f64 = 0.5;
    const FIRST_VBLANK_EQ_1_START: usize = 0;
    const FIRST_VBLANK_VSYNC_START: usize = 1;
    const FIRST_VBLANK_VSYNC_END: usize = 2;
    const FIRST_VBLANK_EQ_2_END: usize = 3;
    const LAST_VBLANK_EQ_1_START: usize = 4;
    const LAST_VBLANK_VSYNC_START: usize = 5;
    const LAST_VBLANK_VSYNC_END: usize = 6;
    const LAST_VBLANK_EQ_2_END: usize = 7;
    const FIRST_HSYNC_LENGTH: usize = 0;
    const FIRST_EQPL2_LENGTH: usize = 1;
    const LAST_HSYNC_LENGTH: usize = 2;

    let mut prev_hsync_diff = inter_field_state.prev_first_hsync_diff;

    let validpulses_len = validpulses_type.len();
    let mut vblank_lines = [-1.0; 8];

    let (vblank_pulses, field_order_lengths) = measure_vblank_sections(
        validpulses_type,
        validpulses_start,
        validpulses_valid,
        meanlinelen,
        field_lines,
    );

    let (first_field, progressive_field) = decide_field_order(
        field_order_lengths,
        is_ntsc,
        inter_field_state,
        field_order_confidence,
        fallback,
    );
    let (first_field_lengths, second_field_lengths, _) = field_order_signatures(is_ntsc);

    let line0loc_line = 0.0;
    let vsync_section_lines = num_eq_pulses / 2.0;

    let (current_field_lengths, previous_field_lines, current_field_lines) = if first_field {
        (first_field_lengths, field_lines[1], field_lines[0])
    } else {
        (second_field_lengths, field_lines[0], field_lines[1])
    };

    vblank_lines[FIRST_VBLANK_EQ_1_START] =
        line0loc_line + current_field_lengths[FIRST_HSYNC_LENGTH];
    vblank_lines[FIRST_VBLANK_VSYNC_START] =
        vblank_lines[FIRST_VBLANK_EQ_1_START] + vsync_section_lines;
    vblank_lines[FIRST_VBLANK_VSYNC_END] =
        vblank_lines[FIRST_VBLANK_VSYNC_START] + vsync_section_lines;
    vblank_lines[FIRST_VBLANK_EQ_2_END] =
        vblank_lines[FIRST_VBLANK_VSYNC_END] + vsync_section_lines - 0.5;

    let hsync_start_line =
        vblank_lines[FIRST_VBLANK_EQ_2_END] + current_field_lengths[FIRST_EQPL2_LENGTH];

    vblank_lines[LAST_VBLANK_EQ_1_START] =
        current_field_lines + current_field_lengths[LAST_HSYNC_LENGTH];
    vblank_lines[LAST_VBLANK_VSYNC_START] =
        vblank_lines[LAST_VBLANK_EQ_1_START] + vsync_section_lines;
    vblank_lines[LAST_VBLANK_VSYNC_END] =
        vblank_lines[LAST_VBLANK_VSYNC_START] + vsync_section_lines;
    vblank_lines[LAST_VBLANK_EQ_2_END] =
        vblank_lines[LAST_VBLANK_VSYNC_END] + vsync_section_lines - 0.5;

    let first_vblank_pulse_indexes = [
        FIRST_VBLANK_EQ_1_START,
        FIRST_VBLANK_VSYNC_START,
        FIRST_VBLANK_VSYNC_END,
        FIRST_VBLANK_EQ_2_END,
    ];
    let last_vblank_pulse_indexes = [
        LAST_VBLANK_EQ_1_START,
        LAST_VBLANK_VSYNC_START,
        LAST_VBLANK_VSYNC_END,
        LAST_VBLANK_EQ_2_END,
    ];

    // Accumulate (offset, hsync_loc, valid_count) over every pulse pair within
    // one vblank section.
    let accumulate_vblank = |pulse_indexes: [usize; 4]| {
        let mut offset = 0.0;
        let mut hsync_loc = 0.0;
        let mut valid_count = 0usize;
        for first_index in 0..pulse_indexes.len() {
            for second_index in first_index + 1..pulse_indexes.len() {
                let sync_distance_output = calc_sync_from_known_distances(
                    meanlinelen,
                    VSYNC_TOLERANCE_LINES,
                    hsync_start_line,
                    vblank_pulses[pulse_indexes[first_index]],
                    vblank_pulses[pulse_indexes[second_index]],
                    vblank_lines[pulse_indexes[first_index]],
                    vblank_lines[pulse_indexes[second_index]],
                );
                offset += sync_distance_output.distance_offset;
                hsync_loc += sync_distance_output.hsync_loc;
                valid_count += sync_distance_output.valid_locations;
            }
        }
        (offset, hsync_loc, valid_count)
    };

    let (first_vblank_offset, first_vblank_first_hsync_loc, first_vblank_valid_location_count) =
        accumulate_vblank(first_vblank_pulse_indexes);
    let (last_vblank_offset, last_vblank_first_hsync_loc, last_vblank_valid_location_count) =
        accumulate_vblank(last_vblank_pulse_indexes);

    let mut first_hsync_loc = 0.0;
    let mut valid_location_count = 0usize;
    let mut offset = 0.0;

    let first_vblank_hsync_estimate = if first_vblank_valid_location_count != 0 {
        first_vblank_first_hsync_loc / first_vblank_valid_location_count as f64
    } else {
        0.0
    };
    let last_vblank_hsync_estimate = if last_vblank_valid_location_count != 0 {
        last_vblank_first_hsync_loc / last_vblank_valid_location_count as f64
    } else {
        0.0
    };

    if first_vblank_valid_location_count != 0
        && last_vblank_valid_location_count != 0
        && first_vblank_hsync_estimate
            < last_vblank_hsync_estimate + VSYNC_TOLERANCE_LINES * meanlinelen
        && first_vblank_hsync_estimate
            > last_vblank_hsync_estimate - VSYNC_TOLERANCE_LINES * meanlinelen
    {
        first_hsync_loc = first_vblank_first_hsync_loc + last_vblank_first_hsync_loc;
        valid_location_count = first_vblank_valid_location_count + last_vblank_valid_location_count;
        offset = first_vblank_offset + last_vblank_offset;

        for first_index in 0..first_vblank_pulse_indexes.len() {
            for second_index in 0..last_vblank_pulse_indexes.len() {
                let sync_distance_output = calc_sync_from_known_distances(
                    meanlinelen,
                    VSYNC_TOLERANCE_LINES,
                    hsync_start_line,
                    vblank_pulses[first_vblank_pulse_indexes[first_index]],
                    vblank_pulses[last_vblank_pulse_indexes[second_index]],
                    vblank_lines[first_vblank_pulse_indexes[first_index]],
                    vblank_lines[last_vblank_pulse_indexes[second_index]],
                );

                offset += sync_distance_output.distance_offset;
                first_hsync_loc += sync_distance_output.hsync_loc;
                valid_location_count += sync_distance_output.valid_locations;
            }
        }
    } else if fallback_line0loc != -1.0 {
        first_hsync_loc = fallback_line0loc + meanlinelen * hsync_start_line;
        valid_location_count = 1;
    } else if first_vblank_valid_location_count == 6
        || (inter_field_state.prev_first_hsync_loc <= 0.0
            && first_vblank_valid_location_count != 0
            && first_vblank_valid_location_count > last_vblank_valid_location_count)
    {
        first_hsync_loc = first_vblank_first_hsync_loc;
        valid_location_count = first_vblank_valid_location_count;
        offset = first_vblank_offset;
    } else if last_vblank_valid_location_count == 6
        || (inter_field_state.prev_first_hsync_loc <= 0.0
            && last_vblank_valid_location_count != 0
            && last_vblank_valid_location_count > first_vblank_valid_location_count)
    {
        first_hsync_loc = last_vblank_first_hsync_loc;
        valid_location_count = last_vblank_valid_location_count;
        offset = last_vblank_offset;
    }

    let estimated_hsync_field_lines = if is_ntsc {
        previous_field_lines
    } else {
        current_field_lines
    };

    let estimated_hsync_loc = ((last_field_offset_lines
        + estimated_hsync_field_lines
        + inter_field_state.prev_first_hsync_loc / meanlinelen)
        * meanlinelen)
        .round_ties_even();

    let mut used_estimated_hsync = false;
    if valid_location_count == 0 && inter_field_state.prev_first_hsync_loc > 0.0 {
        let mut estimated_hsync_with_offset = if (-0.5..=0.5).contains(&prev_hsync_diff) {
            estimated_hsync_loc + meanlinelen * prev_hsync_diff
        } else {
            estimated_hsync_loc
        };

        if estimated_hsync_with_offset <= 0.0 {
            estimated_hsync_with_offset = if validpulses_len > 0 {
                validpulses_start[0]
            } else {
                0.0
            };
        }

        first_hsync_loc += estimated_hsync_with_offset;
        valid_location_count += 1;
        used_estimated_hsync = true;
    }

    if valid_location_count > 0 {
        offset /= valid_location_count as f64;
        first_hsync_loc =
            ((first_hsync_loc + offset) / valid_location_count as f64).round_ties_even();

        if !used_estimated_hsync {
            prev_hsync_diff = (first_hsync_loc - estimated_hsync_loc) / meanlinelen;
        }

        let mut hsync_offset = 0.0;
        let mut hsync_count = 0usize;
        for i in 0..validpulses_len {
            if validpulses_type[i] != PulseType::Hsync || !validpulses_valid[i] {
                continue;
            }

            let lineloc = (validpulses_start[i] - first_hsync_loc) / meanlinelen + hsync_start_line;
            let rlineloc = lineloc.round_ties_even();

            if rlineloc > current_field_lines {
                break;
            }

            if rlineloc >= hsync_start_line {
                hsync_offset += first_hsync_loc + meanlinelen * (rlineloc - hsync_start_line)
                    - validpulses_start[i];
                hsync_count += 1;
            }
        }

        if hsync_count > 0 {
            hsync_offset /= hsync_count as f64;
            first_hsync_loc -= hsync_offset;
        }

        let line0loc = first_hsync_loc - meanlinelen * hsync_start_line;
        let next_field = first_hsync_loc
            + meanlinelen * (vblank_lines[LAST_VBLANK_EQ_1_START] - hsync_start_line);

        return GetFirstHsyncLocResult {
            line0loc: Some(line0loc),
            first_hsync_loc: Some(first_hsync_loc),
            hsync_start_line,
            next_field: Some(next_field),
            first_field,
            progressive_field,
            prev_hsync_diff,
        };
    }

    GetFirstHsyncLocResult {
        line0loc: None,
        first_hsync_loc: None,
        hsync_start_line,
        next_field: None,
        first_field,
        progressive_field,
        prev_hsync_diff,
    }
}

#[derive(Clone, Copy)]
struct PulseSample {
    start: i64,
    len: i64,
}

#[derive(Clone, Copy)]
struct ValidPulseSample {
    pulse_type: PulseType,
    start: i64,
}

#[derive(Clone, Copy)]
struct Line0FallbackResult {
    line0: f64,
    first_field: i64,
    first_field_confidence: i64,
}

struct PulseClassificationResult {
    valid_starts: Vec<i64>,
    meanlinelen: f64,
    line0loc: Option<f64>,
    first_hsync_loc: Option<f64>,
    first_hsync_loc_line: f64,
    vblank_next: Option<f64>,
    is_first_field: bool,
    is_progressive_field: bool,
    prev_hsync_diff: f64,
}

#[inline(always)]
fn half_frame_limit(linelen: f64, frame_lines: i64) -> f64 {
    linelen * (frame_lines - 1) as f64 / 2.0
}

#[inline(always)]
fn line0_missing_or_late(line_0: Option<f64>, linelen: f64, frame_lines: i64) -> bool {
    line_0.is_none_or(|value| value > half_frame_limit(linelen, frame_lines))
}

#[inline(always)]
fn pulse_distance(pulses: &[PulseSample], later: usize, earlier: usize, linelen: f64) -> f64 {
    (pulses[later].start - pulses[earlier].start) as f64 / linelen
}

/// Whether an inter-pulse distance (in lines) is within the fixed 0.06-line
/// tolerance of `target` (a 0.5- or 1.0-line spacing) used by line-0 recovery.
#[inline(always)]
fn distance_matches(distance: f64, target: f64) -> bool {
    (distance - target).abs() < 0.06
}

#[inline(always)]
fn phase_sum(values: [f64; 3]) -> i64 {
    values
        .iter()
        .map(|value| (value * 2.0).round_ties_even().rem_euclid(2.0) as i64)
        .sum()
}

#[inline(always)]
fn argmax3(values: [i64; 3]) -> usize {
    if values[1] > values[0] && values[1] >= values[2] {
        1
    } else if values[2] > values[0] && values[2] > values[1] {
        2
    } else {
        0
    }
}

#[inline(always)]
fn phase_confidence(count: i64, phase_cnt: [i64; 3]) -> i64 {
    count * 100 / (phase_cnt[0] + phase_cnt[1] + phase_cnt[2])
}

/// Tally phase-parity votes over a back-scan of pulse triplets used by line-0
/// recovery. Returns [all-0, all-1, other] counts; the scan stops early once
/// five decisive votes accumulate. For the start of one field all three values
/// should be one parity and for the other field the other (PAL/NTSC swap which).
#[inline(always)]
fn count_phase_votes(
    pulses: &[PulseSample],
    pivot: usize,
    range: std::ops::RangeInclusive<usize>,
    measured_linelen: f64,
) -> [i64; 3] {
    let anchor = pulses[pivot - 2].start;
    let mut phase_cnt = [0, 0, 0];
    for d in range {
        let pp = [
            (anchor - pulses[pivot - d].start) as f64 / measured_linelen,
            (anchor - pulses[pivot - d + 1].start) as f64 / measured_linelen,
            (anchor - pulses[pivot - d + 2].start) as f64 / measured_linelen,
        ];
        match phase_sum(pp) {
            0 => phase_cnt[0] += 1,
            3 => phase_cnt[1] += 1,
            _ => phase_cnt[2] += 1,
        }
        if phase_cnt[0] + phase_cnt[1] >= 5 {
            break;
        }
    }
    phase_cnt
}

fn demod_std(data: &[f32], start: i64, end: i64) -> f32 {
    let Some((start, end)) = demod_slice_bounds(data.len(), start, end) else {
        return f32::NAN;
    };
    // Population standard deviation == RMS of the mean-centered samples.
    rms(&data[start..end]) as f32
}

/// Mean and standard deviation of the demod samples in the gap between
/// consecutive pulses `a` and `a + 1` (the back porch / active interval), with
/// a fixed 40-sample inset on each side to skip the pulse edges.
fn pulse_gap_stats(demod_05: &[f32], pulses: &[PulseSample], a: usize) -> (f32, f32) {
    let start = pulses[a].start + pulses[a].len + 40;
    let end = pulses[a + 1].start - 40;
    (
        demod_mean(demod_05, start, end),
        demod_std(demod_05, start, end),
    )
}

fn get_line0_fallback(
    valid_pulses: &[ValidPulseSample],
    raw_pulses: &[PulseSample],
    demod_05: &[f32],
    lt_vsync: (f64, f64),
    linelen: f64,
    frame_lines: i64,
    relaxed: bool,
    expected_line0: Option<f64>,
    expected_first_field: Option<i64>,
) -> Option<Line0FallbackResult> {
    assert!(
        !raw_pulses.is_empty(),
        "line 0 fallback on a field without pulses"
    );

    // Try a more primitive way of locating line 0 if the normal approach fails.
    // This doesn't actually fine line 0, rather it locates the approx position of the last vsync before vertical blanking
    // as the later code is designed to work off of that.
    // It is searched for in this order:
    //  -Find the start of long vsync pulses
    //  -Find the end of long vsync pulses
    //  -Find the end of short-distance eq pulses
    //  -Find the start of short-distance eq pulses
    //  -Just look for the first "long" pulse that could be start of vsync pulses in
    //   e.g a 240p/280p signal (that is, a pulse that is at least vsync pulse length.)
    let mut filtered_pulses = Vec::with_capacity(raw_pulses.len());
    filtered_pulses.push(raw_pulses[0]);

    let mut i = 1usize;
    while i < raw_pulses.len().saturating_sub(2) {
        if (raw_pulses[i + 1].start - raw_pulses[i].start) as f64 > 0.45 * linelen
            && (raw_pulses[i].start - raw_pulses[i - 1].start) as f64 > 0.45 * linelen
        {
            // normal case: pulse starts are at least 0.45 lines apart
            filtered_pulses.push(raw_pulses[i]);
        } else {
            // either pulse i or i+1 is wrong
            let dis12 = pulse_distance(raw_pulses, i, i - 1, linelen);
            let dis13 = pulse_distance(raw_pulses, i + 1, i - 1, linelen);
            let dis24 = pulse_distance(raw_pulses, i + 2, i, linelen);
            let dis34 = pulse_distance(raw_pulses, i + 2, i + 1, linelen);
            let ddis12 = (dis12 - 0.5).abs().min((dis12 - 1.0).abs());
            let ddis13 = (dis13 - 0.5).abs().min((dis13 - 1.0).abs());
            let ddis24 = (dis24 - 0.5).abs().min((dis24 - 1.0).abs());
            let ddis34 = (dis34 - 0.5).abs().min((dis34 - 1.0).abs());

            if ddis13 + ddis34 < ddis12 + ddis24 {
                filtered_pulses.push(raw_pulses[i + 1]);
            } else {
                filtered_pulses.push(raw_pulses[i]);
            }
            i += 1;
        }
        i += 1;
    }
    while i < raw_pulses.len() {
        filtered_pulses.push(raw_pulses[i]);
        i += 1;
    }

    let mut line_0 = None;
    let mut line_0_backup = None;

    let mut first_field = -1;
    let mut first_field_confidence = -1;
    let mut first_field_backup = -1;
    let mut first_field_confidence_backup = -1;

    let short_pulse_max = 0.2 * linelen;
    let long_pulse_min = 0.35 * linelen;

    // First try: Find end of long sync pulses
    i = 15;
    while line_0.is_none() && i < filtered_pulses.len().saturating_sub(2) {
        let dis_ppspp = pulse_distance(&filtered_pulses, i - 1, i - 2, linelen);
        let disp_pspp = pulse_distance(&filtered_pulses, i, i - 1, linelen);
        let dispp_spp = pulse_distance(&filtered_pulses, i + 1, i, linelen);
        let dispps_pp = pulse_distance(&filtered_pulses, i + 2, i + 1, linelen);
        if distance_matches(dis_ppspp, 0.5)
            && distance_matches(disp_pspp, 0.5)
            && distance_matches(dispp_spp, 0.5)
            && distance_matches(dispps_pp, 0.5)
            && filtered_pulses[i - 2].len as f64 > long_pulse_min
            && filtered_pulses[i - 1].len as f64 > long_pulse_min
            && (filtered_pulses[i].len as f64) < short_pulse_max
            && (filtered_pulses[i + 1].len as f64) < short_pulse_max
            && (filtered_pulses[i + 2].len as f64) < short_pulse_max
        {
            // we measure the distance of three pulses in previous field to start of long pulses
            // to check if this is first or second field
            // as there may be broken sync pulses we scan backwards
            let measured_linelen =
                (dis_ppspp + disp_pspp + dispp_spp + dispps_pp) * (linelen / 2.0);
            let mut line_offset = None;
            // count "half lines" for detecting top/bottom field:
            let mut half_lines = 0;
            let mut j = i;
            while j < i + 9 {
                let dis = pulse_distance(&filtered_pulses, j + 1, j, linelen);
                if distance_matches(dis, 0.5)
                    && (filtered_pulses[j].len as f64) < short_pulse_max
                    && (filtered_pulses[j + 1].len as f64) < short_pulse_max
                {
                    half_lines += 1;
                } else if distance_matches(dis, 1.0)
                    && (filtered_pulses[j].len as f64) < short_pulse_max
                    && (filtered_pulses[j + 1].len as f64) < short_pulse_max
                {
                    break;
                } else {
                    half_lines = 0;
                    break;
                }
                j += 1;
            }
            if half_lines == 4 && frame_lines == 625 {
                first_field = 0;
                first_field_confidence = 100;
                line_offset = Some(5.0);
            } else if half_lines == 5 {
                if frame_lines == 625 {
                    first_field = 1;
                    first_field_confidence = 100;
                    line_offset = Some(4.5);
                } else {
                    first_field = 0;
                    first_field_confidence = 100;
                    line_offset = Some(5.5);
                }
            } else if half_lines == 6 && frame_lines == 525 {
                first_field = 1;
                line_offset = Some(6.0);
            }

            // if we couldn't detect field type based on half lines check phase
            if line_offset.is_none() {
                let phase_cnt =
                    count_phase_votes(&filtered_pulses, i, 15..=i.min(30), measured_linelen);
                // PAL:  for start of first field all values should be 1, for second field all should be 0
                // NTSC: for start of first field all values should be 0, for second field all should be 1
                let phase = argmax3(phase_cnt);
                if phase == 0 {
                    // we need to differ between 625 and 525 line
                    if frame_lines == 625 {
                        first_field = 0;
                        line_offset = Some(5.0);
                    } else {
                        first_field = 1;
                        line_offset = Some(6.0);
                    }
                    first_field_confidence = phase_confidence(phase_cnt[0], phase_cnt);
                } else if phase == 1 {
                    // we need to differ between 625 and 525 line
                    if frame_lines == 625 {
                        first_field = 1;
                        line_offset = Some(4.5);
                    } else {
                        first_field = 0;
                        line_offset = Some(5.5);
                    }
                    first_field_confidence = phase_confidence(phase_cnt[1], phase_cnt);
                }
            }

            if let Some(line_offset) = line_offset {
                // in case we cannot find a matching pulse, we can still use this prediction
                let line_0_est =
                    filtered_pulses[i - 2].start as f64 - line_offset * measured_linelen;
                if line_0_backup.is_none() {
                    line_0_backup = Some(line_0_est);
                    first_field_backup = first_field;
                    first_field_confidence_backup = first_field_confidence;
                }
                // find pulse
                let (start, end) = if relaxed {
                    (i.saturating_sub(20), i)
                } else {
                    (i.saturating_sub(16), i.saturating_sub(4))
                };
                for pulse in &filtered_pulses[start..end] {
                    if (pulse.start as f64 - line_0_est).abs() / linelen < 0.08 {
                        line_0 = Some(pulse.start as f64);
                        break;
                    }
                }
            }
        }
        i += 1;
    }

    // Second try: Find beginng of long sync pulses
    i = 10;
    while line0_missing_or_late(line_0, linelen, frame_lines)
        && i < filtered_pulses.len().saturating_sub(2)
    {
        let dis_ppspp = pulse_distance(&filtered_pulses, i - 1, i - 2, linelen);
        let disp_pspp = pulse_distance(&filtered_pulses, i, i - 1, linelen);
        let dispp_spp = pulse_distance(&filtered_pulses, i + 1, i, linelen);
        let dispps_pp = pulse_distance(&filtered_pulses, i + 2, i + 1, linelen);

        if distance_matches(dis_ppspp, 0.5)
            && distance_matches(disp_pspp, 0.5)
            && distance_matches(dispp_spp, 0.5)
            && distance_matches(dispps_pp, 0.5)
            && (filtered_pulses[i - 2].len as f64) < short_pulse_max
            && (filtered_pulses[i - 1].len as f64) < short_pulse_max
            && filtered_pulses[i].len as f64 > long_pulse_min
            && filtered_pulses[i + 1].len as f64 > long_pulse_min
            && filtered_pulses[i + 2].len as f64 > long_pulse_min
        {
            // we measure the distance of three pulses in previous field to start of long pulses
            // to check if this is first or second field
            // as there may be broken syncs we scan backwards
            let measured_linelen =
                (dis_ppspp + disp_pspp + dispp_spp + dispps_pp) * (linelen / 2.0);
            let mut line_offset = None;
            let phase_cnt =
                count_phase_votes(&filtered_pulses, i, 10..=i.min(25), measured_linelen);
            // for start of first field all values should be 0, for second field all should be 1
            let phase = argmax3(phase_cnt);
            let mut candidate_first_field = 0;
            let mut candidate_confidence = 0;
            if phase == 0 {
                // we need to differ between 625 and 525 line
                line_offset = Some(if frame_lines == 625 { 2.0 } else { 3.0 });
                candidate_first_field = 1;
                candidate_confidence = phase_confidence(phase_cnt[0], phase_cnt);
            } else if phase == 1 {
                line_offset = Some(2.5);
                candidate_first_field = 0;
                candidate_confidence = phase_confidence(phase_cnt[1], phase_cnt);
            }

            if let Some(line_offset) = line_offset {
                // in case we cannot find a matching pulse, we can still use this prediction
                let line_0_est =
                    filtered_pulses[i - 2].start as f64 - line_offset * measured_linelen;
                if line_0_backup.is_none() {
                    line_0_backup = Some(line_0_est);
                    first_field_backup = candidate_first_field;
                    first_field_confidence_backup = candidate_confidence;
                }
                // find pulse
                let (start, end) = if relaxed {
                    (i.saturating_sub(15), i)
                } else {
                    (i.saturating_sub(10), i.saturating_sub(3))
                };
                for pulse in &filtered_pulses[start..end] {
                    if (pulse.start as f64 - line_0_est).abs() / linelen < 0.08 {
                        if line_0 != Some(pulse.start as f64)
                            || candidate_confidence > first_field_confidence
                        {
                            first_field = candidate_first_field;
                            // This branch updates the field candidate while leaving confidence unchanged.
                        }
                        line_0 = Some(pulse.start as f64);
                        break;
                    }
                }
            }
        }
        i += 1;
    }

    // Third try: Find end of blanking
    i = 10;
    while line0_missing_or_late(line_0, linelen, frame_lines)
        && i < filtered_pulses.len().saturating_sub(2)
    {
        let dis_ppspp_prev = pulse_distance(&filtered_pulses, i - 2, i - 3, linelen);
        let dis_ppspp = pulse_distance(&filtered_pulses, i - 1, i - 2, linelen);
        let disp_pspp = pulse_distance(&filtered_pulses, i, i - 1, linelen);
        let dispp_spp = pulse_distance(&filtered_pulses, i + 1, i, linelen);
        let dispps_pp = pulse_distance(&filtered_pulses, i + 2, i + 1, linelen);

        // Relaxed check: ignore the first interval (disPpspp) to handle dropouts better
        let check_strict = distance_matches(dis_ppspp_prev, 0.5)
            && distance_matches(dis_ppspp, 0.5)
            && distance_matches(disp_pspp, 0.5)
            && distance_matches(dispp_spp, 1.0)
            && distance_matches(dispps_pp, 1.0);
        let check_relaxed = distance_matches(dis_ppspp, 0.5)
            && distance_matches(disp_pspp, 0.5)
            && distance_matches(dispp_spp, 1.0)
            && distance_matches(dispps_pp, 1.0);

        if (if relaxed { check_relaxed } else { check_strict })
            && (filtered_pulses[i - 2].len as f64) < short_pulse_max
            && (filtered_pulses[i - 1].len as f64) < short_pulse_max
            && (filtered_pulses[i].len as f64) < short_pulse_max
            && (filtered_pulses[i + 1].len as f64) < short_pulse_max
            && (filtered_pulses[i + 2].len as f64) < short_pulse_max
        {
            // we measure the distance of three pulses in previous field to start of long pulses
            // to check if this is first or second field
            // as there may be broken sync pulses we scan backwards
            let measured_linelen =
                (dis_ppspp + disp_pspp + dispp_spp + dispps_pp) * (linelen / 3.0);
            let mut line_offset = None;
            let eq_pulse_len =
                (filtered_pulses[i - 2].len + filtered_pulses[i - 1].len) as f64 / 2.0;
            let hsync_pulse_len =
                (filtered_pulses[i + 1].len + filtered_pulses[i + 2].len) as f64 / 2.0;
            let mut candidate_first_field = 0;
            let mut candidate_confidence = 0;

            if hsync_pulse_len / eq_pulse_len > 1.75 {
                if (filtered_pulses[i].len as f64) < eq_pulse_len * 1.25 {
                    line_offset = Some(if frame_lines == 625 { 7.0 } else { 8.0 });
                    candidate_first_field = 0;
                    candidate_confidence = if (filtered_pulses[i].len as f64) < eq_pulse_len * 1.1 {
                        80
                    } else {
                        60
                    };
                } else if (filtered_pulses[i].len as f64) > hsync_pulse_len * 0.75 {
                    line_offset = Some(if frame_lines == 625 { 7.0 } else { 9.0 });
                    candidate_first_field = 1;
                    candidate_confidence =
                        if (filtered_pulses[i].len as f64) > hsync_pulse_len * 0.9 {
                            80
                        } else {
                            60
                        };
                }
            }

            if let Some(line_offset) = line_offset {
                // in case we cannot find a matching pulse, we can still use this prediction
                let line_0_est =
                    filtered_pulses[i - 2].start as f64 - line_offset * measured_linelen;
                if line_0_backup.is_none() {
                    line_0_backup = Some(line_0_est);
                    first_field_backup = candidate_first_field;
                    first_field_confidence_backup = candidate_confidence;
                }
                // find pulse
                let (start, end) = if relaxed {
                    (i.saturating_sub(25), i)
                } else {
                    (i.saturating_sub(20), i.saturating_sub(4))
                };
                for pulse in &filtered_pulses[start..end] {
                    let diff = (pulse.start as f64 - line_0_est).abs() / linelen;

                    if diff < 0.08 {
                        if line_0 != Some(pulse.start as f64)
                            || candidate_confidence > first_field_confidence
                        {
                            first_field = candidate_first_field;
                            // This branch updates the field candidate while leaving confidence unchanged.
                        }
                        line_0 = Some(pulse.start as f64);
                        break;
                    }
                }
            }
        }
        i += 1;
    }

    // Fourth try: Find beginning of blanking
    i = 2;
    while line0_missing_or_late(line_0, linelen, frame_lines)
        && i < filtered_pulses.len().saturating_sub(3)
    {
        let dis_ppspp = pulse_distance(&filtered_pulses, i - 1, i - 2, linelen);
        let disp_pspp = pulse_distance(&filtered_pulses, i, i - 1, linelen);
        let dispp_spp = pulse_distance(&filtered_pulses, i + 1, i, linelen);
        let dispps_pp = pulse_distance(&filtered_pulses, i + 2, i + 1, linelen);
        let disppsp_p = pulse_distance(&filtered_pulses, i + 3, i + 2, linelen);

        // Relaxed check: ignore the last interval (disppspP)
        let check_strict = distance_matches(dis_ppspp, 1.0)
            && distance_matches(disp_pspp, 1.0)
            && distance_matches(dispp_spp, 0.5)
            && distance_matches(dispps_pp, 0.5)
            && distance_matches(disppsp_p, 0.5);
        let check_relaxed = distance_matches(dis_ppspp, 1.0)
            && distance_matches(disp_pspp, 1.0)
            && distance_matches(dispp_spp, 0.5)
            && distance_matches(dispps_pp, 0.5);

        if (if relaxed { check_relaxed } else { check_strict })
            && (filtered_pulses[i - 2].len as f64) < short_pulse_max
            && (filtered_pulses[i - 1].len as f64) < short_pulse_max
            && (filtered_pulses[i].len as f64) < short_pulse_max
            && (filtered_pulses[i + 1].len as f64) < short_pulse_max
            && (filtered_pulses[i + 2].len as f64) < short_pulse_max
        {
            let hsync_pulse_len =
                (filtered_pulses[i - 2].len + filtered_pulses[i - 1].len) as f64 / 2.0;
            let eq_pulse_len =
                (filtered_pulses[i + 1].len + filtered_pulses[i + 2].len) as f64 / 2.0;

            if hsync_pulse_len / eq_pulse_len > 1.75 {
                if (filtered_pulses[i].len as f64) < eq_pulse_len * 1.25 {
                    let candidate_confidence =
                        if (filtered_pulses[i].len as f64) < eq_pulse_len * 1.1 {
                            60
                        } else {
                            40
                        };
                    if line_0 != Some(filtered_pulses[i - 1].start as f64)
                        || candidate_confidence > first_field_confidence
                    {
                        first_field_confidence = candidate_confidence;
                        if frame_lines == 625 {
                            first_field = 0;
                        } else {
                            first_field = 1;
                        }
                    }
                    line_0 = Some(filtered_pulses[i - 1].start as f64);
                } else if (filtered_pulses[i].len as f64) > hsync_pulse_len * 0.75 {
                    let candidate_confidence =
                        if (filtered_pulses[i].len as f64) > hsync_pulse_len * 0.9 {
                            60
                        } else {
                            40
                        };
                    if line_0 != Some(filtered_pulses[i].start as f64)
                        || candidate_confidence > first_field_confidence
                    {
                        first_field_confidence = candidate_confidence;
                        if frame_lines == 625 {
                            first_field = 0;
                        } else {
                            first_field = 1;
                        }
                    }
                    line_0 = Some(filtered_pulses[i].start as f64);
                }
            }

            // the pulse duration was not clear, we need to check contents
            // the interval between the first pulses half a line apart is either active or not
            if line_0.is_none() {
                let (line_p_avg, line_p_std) = pulse_gap_stats(demod_05, &filtered_pulses, i - 1);
                let (line_i_avg, line_i_std) = pulse_gap_stats(demod_05, &filtered_pulses, i);
                let (line_n_avg, line_n_std) = pulse_gap_stats(demod_05, &filtered_pulses, i + 1);

                if (line_p_avg - line_i_avg).abs() / (line_p_avg + line_i_avg) < 0.05
                    && (line_p_avg - line_n_avg).abs() / (line_p_avg + line_n_avg) > 0.15
                    && (line_p_std - line_i_std).abs() * 2.0 < (line_p_std - line_n_std).abs()
                {
                    if line_0 != Some(filtered_pulses[i - 1].start as f64)
                        || 20 > first_field_confidence
                    {
                        if frame_lines == 625 {
                            first_field = 0;
                        }
                        // NTSC set only a local candidate here in the source, so nothing to apply.
                        first_field_confidence = 20;
                    }
                    line_0 = Some(filtered_pulses[i - 1].start as f64);
                } else if (line_p_avg - line_i_avg).abs() / (line_p_avg + line_i_avg) > 0.15
                    && (line_p_avg - line_n_avg).abs() / (line_p_avg + line_n_avg) > 0.15
                    && line_i_std * 2.0 > line_p_std
                {
                    if line_0 != Some(filtered_pulses[i].start as f64)
                        || 20 > first_field_confidence
                    {
                        if frame_lines == 625 {
                            first_field = 0;
                        } else {
                            first_field = 1;
                        }
                        first_field_confidence = 20;
                    }
                    line_0 = Some(filtered_pulses[i].start as f64);
                }
            }
        }
        i += 1;
    }

    let line0_limit = half_frame_limit(linelen, frame_lines);
    if let (Some(line0), Some(backup)) = (line_0, line_0_backup) {
        if line0 > line0_limit
            && (backup < line0 - (linelen * (frame_lines - 5) as f64 / 2.0)
                || (relaxed && backup < line0_limit))
        {
            line_0 = Some(backup);
            first_field = first_field_backup;
            first_field_confidence = first_field_confidence_backup - 20;
        }
    }

    if line_0.is_none() && line_0_backup.is_some() {
        line_0 = line_0_backup;
        first_field = first_field_backup;
        first_field_confidence = first_field_confidence_backup - 20;
    }

    if let Some(expected_line0) = expected_line0 {
        if line0_missing_or_late(line_0, linelen, frame_lines) {
            let limit = half_frame_limit(linelen, frame_lines);
            if expected_line0 < limit && expected_line0 > -5.0 * linelen {
                let mut best_p = None;
                let mut min_diff = 1_000_000.0;
                // Only snap to a pulse very close to the prediction (0.7 lines).
                // A wider range can choose an adjacent HSYNC/EQ pulse when dropout hides the VSYNC pulse.
                let search_range = 0.7 * linelen;
                for &pulse in &filtered_pulses {
                    let diff = (pulse.start as f64 - expected_line0).abs();
                    if diff < search_range && diff < min_diff {
                        min_diff = diff;
                        best_p = Some(pulse);
                    }
                }
                if let Some(best_p) = best_p {
                    line_0 = Some(best_p.start as f64);
                    if let Some(expected_first_field) = expected_first_field {
                        first_field = expected_first_field;
                        first_field_confidence = 50;
                    }
                } else if relaxed && expected_line0 > 0.0 {
                    line_0 = Some(expected_line0);
                    if let Some(expected_first_field) = expected_first_field {
                        first_field = expected_first_field;
                        first_field_confidence = 40;
                    }
                }
            }
        }
    }

    if let Some(line0) = line_0 {
        return Some(Line0FallbackResult {
            line0,
            first_field,
            first_field_confidence,
        });
    }

    // Fifth fallback: find the last hsync in front of a long block.
    let long_pulses = raw_pulses
        .iter()
        .copied()
        .filter(|pulse| inrange(pulse.len as f64, lt_vsync.0, lt_vsync.1 * 10.0))
        .collect::<Vec<_>>();

    if long_pulses.is_empty() {
        return None;
    }

    // Offset from the first vsync to the location expected by line-zero recovery.
    let first_long_pulse_pos = long_pulses[0].start;

    // This fallback assumes the last hsync before the vsync area is intact;
    // damaged hsyncs can still point it at the wrong pulse.
    // Find the last vsync before the vsync area for downstream pulse classification.
    for pulse in valid_pulses {
        if pulse.start > first_long_pulse_pos {
            break;
        }
        if pulse.pulse_type == PulseType::Hsync {
            line_0 = Some(pulse.start as f64);
        }
    }

    if line_0.is_none() {
        line_0 = Some(first_long_pulse_pos as f64 - (3.0 * linelen));
    }

    Some(Line0FallbackResult {
        line0: line_0.unwrap(),
        first_field: -1,
        first_field_confidence: -1,
    })
}

fn median_sample_count(values: &mut [i64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }

    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid] as f64
    } else {
        (values[mid - 1] as f64 + values[mid] as f64) / 2.0
    }
}

fn try_get_pulses_core(
    spec: &DecoderSpec,
    raw_pulses: &[PulseSample],
    field: &DecodedField,
    inter_field_state: &InterFieldState,
) -> PulseClassificationResult {
    let num_pulses = spec.sys_num_pulses as f64;
    let frame_lines = spec.sys_frame_lines.line_count() as i64;
    let field_lines = (spec.sys_field_lines[0], spec.sys_field_lines[1]);
    let is_ntsc = spec.sys_frame_lines == LineSystem::Line525;

    let to_inpx = |us: f64| usectoinpx(spec, us);

    let hsync_typical = to_inpx(spec.sys_hsync_pulse_us);

    // Some disks have odd sync levels resulting in short and/or long pulse lengths.
    // So, take the median hsync and adjust the expected values accordingly.
    let hsync_checkmin = to_inpx(spec.sys_hsync_pulse_us - 1.75);
    let hsync_checkmax = to_inpx(spec.sys_hsync_pulse_us + 2.0);
    let mut hlens = raw_pulses
        .iter()
        .filter_map(|pulse| {
            inrange(pulse.len as f64, hsync_checkmin, hsync_checkmax).then_some(pulse.len)
        })
        .collect::<Vec<_>>();

    let hsync_median = if hlens.is_empty() {
        // Fall back to the configured microsecond value when no plausible hsync
        // lengths are available, even though the common path uses sample counts.
        spec.sys_hsync_pulse_us
    } else {
        median_sample_count(&mut hlens)
    };

    let hsync_offset = hsync_median - hsync_typical;
    let hsync = (hsync_median + to_inpx(-0.7), hsync_median + to_inpx(0.7));
    let eq = (
        to_inpx(spec.sys_eq_pulse_us - 0.9) + hsync_offset,
        to_inpx(spec.sys_eq_pulse_us + 0.9) + hsync_offset,
    );
    let vsync = (
        to_inpx(spec.sys_vsync_pulse_us * 0.5) + hsync_offset,
        to_inpx(spec.sys_vsync_pulse_us + 1.0) + hsync_offset,
    );

    let mut valid_types = Vec::new();
    let mut valid_starts = Vec::new();
    let mut valid_good = Vec::new();

    let mut i = 0usize;
    while i < raw_pulses.len() {
        let curpulse = raw_pulses[i];
        if inrange(curpulse.len as f64, hsync.0, hsync.1) {
            let good = valid_types.last().zip(valid_starts.last()).is_some_and(
                |(&prev_type, &prev_start)| {
                    pulse_qualitycheck(
                        prev_type,
                        prev_start,
                        PulseType::Hsync,
                        curpulse.start,
                        field.inlinelen,
                    )
                },
            );
            valid_types.push(PulseType::Hsync);
            valid_starts.push(curpulse.start);
            valid_good.push(good);
            i += 1;
        } else if i > 2
            && inrange(raw_pulses[i].len as f64, eq.0, eq.1)
            && valid_types
                .last()
                .is_some_and(|&pulse_type| pulse_type == PulseType::Hsync)
        {
            let start = i - 2;
            let end = (i + 24).min(raw_pulses.len());
            let starts = raw_pulses[start..end]
                .iter()
                .map(|pulse| pulse.start)
                .collect::<Vec<_>>();
            let lengths = raw_pulses[start..end]
                .iter()
                .map(|pulse| pulse.len)
                .collect::<Vec<_>>();
            let (done, pulse_types, starts, good_flags) = run_vblank_state_machine(
                &starts,
                &lengths,
                hsync,
                eq,
                vsync,
                num_pulses,
                field.inlinelen,
            );
            if done {
                for j in 2..pulse_types.len() {
                    valid_types.push(pulse_types[j]);
                    valid_starts.push(starts[j]);
                    valid_good.push(good_flags[j]);
                }
                i += pulse_types.len().saturating_sub(2);
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    // Determine longest run of hsync pulses.
    let mut longrun = (-1isize, -1isize);
    let mut currun: Option<(isize, isize)> = None;
    for (idx, &pulse_type) in valid_types.iter().enumerate() {
        if pulse_type != PulseType::Hsync {
            if let Some(run) = currun {
                if run.1 > longrun.1 {
                    longrun = run;
                }
            }
            currun = None;
        } else if currun.is_none() {
            currun = Some((idx as isize, 0));
        } else if let Some(run) = currun.as_mut() {
            run.1 += 1;
        }
    }
    if let Some(run) = currun {
        if run.1 > longrun.1 {
            longrun = run;
        }
    }

    let mut linelens = Vec::new();
    let start = longrun.0 + 1;
    let end = longrun.0 + longrun.1;
    if start < end {
        for idx in start..end {
            let idx = idx as usize;
            let linelen = valid_starts[idx] - valid_starts[idx - 1];
            if inrange(linelen as f64 / field.inlinelen, 0.95, 1.05) {
                linelens.push(linelen as f64);
            }
        }
    }

    let meanlinelen: f64 = if linelens.is_empty() {
        field.inlinelen
    } else {
        linelens.iter().sum::<f64>() / linelens.len() as f64
    };

    // Calculate in terms of lines to prevent integer overflow when seeking ahead large amounts.
    let prev_first_hsync_offset_lines = if inter_field_state.prev_first_hsync_readloc != -1 {
        (inter_field_state.prev_first_hsync_readloc - field.readloc as i64) as f64 / meanlinelen
    } else {
        0.0
    };

    let mut fallback = None;

    let valid_samples = valid_types
        .iter()
        .zip(valid_starts.iter())
        .map(|(&pulse_type, &start)| ValidPulseSample { pulse_type, start })
        .collect::<Vec<_>>();

    if spec.rf_fallback_vsync {
        let mut expected_line0 = None;
        let mut expected_first_field = None;
        if inter_field_state.prev_first_hsync_readloc != -1 {
            let prev_abs = inter_field_state.prev_first_hsync_readloc as f64
                + inter_field_state.prev_first_hsync_loc;
            let lines_per_field = frame_lines as f64 / 2.0;
            let target_abs = prev_abs + (lines_per_field * meanlinelen);
            // Target VSYNC area approx 8 lines before active video (Start of VSYNC block).
            let expected_line0_abs = target_abs - (8.0 * meanlinelen);
            expected_line0 = Some(expected_line0_abs - field.readloc as f64);
            if inter_field_state.prev_first_field != -1 {
                expected_first_field = Some(1 - inter_field_state.prev_first_field);
            }
        }

        fallback = get_line0_fallback(
            &valid_samples,
            raw_pulses,
            &field.data.video.demod_05,
            vsync,
            field.inlinelen,
            frame_lines,
            spec.rf_relaxed_line0,
            expected_line0,
            expected_first_field,
        );
    }

    // Find the location of the first hsync pulse (first line of video after the vsync pulses).
    // This relies on the pulse type (hsync, vsync, eq pulse) being accurate in valid pulses.
    let first_hsync = get_first_hsync_loc(
        &valid_types,
        &valid_starts
            .iter()
            .map(|&start| start as f64)
            .collect::<Vec<_>>(),
        &valid_good,
        meanlinelen,
        is_ntsc,
        [field_lines.0 as f64, field_lines.1 as f64],
        num_pulses,
        inter_field_state,
        prev_first_hsync_offset_lines,
        spec.rf_field_order_confidence,
        fallback,
    );

    PulseClassificationResult {
        valid_starts,
        meanlinelen,
        line0loc: first_hsync.line0loc,
        first_hsync_loc: first_hsync.first_hsync_loc,
        first_hsync_loc_line: first_hsync.hsync_start_line,
        vblank_next: first_hsync.next_field,
        is_first_field: first_hsync.first_field,
        is_progressive_field: first_hsync.progressive_field,
        prev_hsync_diff: first_hsync.prev_hsync_diff,
    }
}

pub(crate) struct PulseResult {
    pub(crate) line0loc: Option<f64>,
    pub(crate) first_hsync_loc: Option<f64>,
    pub(crate) first_hsync_loc_line: Option<f64>,
    pub(crate) meanlinelen: f64,
    /// The field's sync tip and blanking levels as measured on its pulses.
    pub(crate) sync_level: f32,
    pub(crate) blank_level: f32,
}

/// Sync pulses of a field, with the sync tip and blanking levels they were
/// sliced between.
struct SyncPulses {
    pulses: Vec<PulseSample>,
    sync_level: f32,
    blank_level: f32,
}

/// A pulse of plausible hsync width, with the levels measured on its tip and
/// on the porch after it.
#[derive(Clone, Copy)]
struct HsyncCandidate {
    fall: usize,
    sync_level: f32,
    porch_level: f32,
}

/// The element at the middle index of the sorted values (the larger of the
/// two middle ones for an even count).
fn upper_median(values: impl Iterator<Item = f32>) -> Option<f32> {
    let mut values: Vec<f32> = values.collect();
    if values.is_empty() {
        return None;
    }
    let mid = values.len() / 2;
    Some(*values.select_nth_unstable_by(mid, f32::total_cmp).1)
}

/// Median of the five samples centred on `center` (fewer at the ends of the
/// buffer), `None` past its end.
fn window_median(data: &[f32], center: usize) -> Option<f32> {
    let window = data.get(center.saturating_sub(2)..(center + 3).min(data.len()))?;
    upper_median(window.iter().copied())
}

/// Sync tip and blanking level estimates as the 5th and 25th percentiles of
/// the field: the sync tips are its lowest few percent and the blanking
/// intervals the band just above them.
fn level_estimates(smoothed: &[f32]) -> (f32, f32) {
    let rank = |fraction: f64| (smoothed.len() as f64 * fraction) as usize;
    let [sync, blank] =
        select_ranks_by_key(smoothed, f32_total_order_key, [rank(0.05), rank(0.25)]);
    (
        f32_from_total_order_key(sync),
        f32_from_total_order_key(blank),
    )
}

/// Pulses of `data` sliced at `level`, as `(fall, rise)` sample pairs with
/// `data[fall] >= level > data[fall + 1]` and `data[rise] < level <= data[rise + 1]`.
fn sliced_pulses(data: &[f32], level: f32) -> Vec<(usize, usize)> {
    // Crossings are a few per line, so a chunk of sample pairs whose samples
    // all lie on one side of the level is skipped on a vectorizable count,
    // and only the rest is walked pair by pair.
    const CHUNK: usize = 64;
    let mut pulses = Vec::new();
    let mut fall = None;
    let last_pair = data.len().saturating_sub(1);
    let mut first = 0;
    while first < last_pair {
        let end = (first + CHUNK).min(last_pair);
        let samples = &data[first..=end];
        let below = samples.iter().filter(|&&v| v < level).count();
        if below != 0 && below != samples.len() {
            for (i, pair) in samples.windows(2).enumerate() {
                let (a, b) = (pair[0], pair[1]);
                if a >= level && b < level {
                    fall = Some(first + i);
                } else if a < level && b >= level {
                    if let Some(fall) = fall.take() {
                        pulses.push((fall, first + i));
                    }
                }
            }
        }
        first = end;
    }
    pulses
}

/// Sub-sample position of the level crossing between samples `i` and `i + 1`
/// by linear interpolation of their offsets `y0`, `y1` from the level; the
/// samples are on opposite sides, so the result lies in `[i, i + 1)`.
fn crossing(i: usize, y0: f32, y1: f32) -> f64 {
    i as f64 + f64::from(y0 / (y0 - y1))
}

/// Locates the sync pulses of a field by slicing the smoothed `demod_05`
/// between the sync tip and blanking levels.
///
/// The levels are estimated first (`estimates`, else from the field's
/// percentiles), measured on the pulses of hsync width whose spacing fits
/// enough other such pulses, and the field is then sliced halfway between the
/// measured levels keeping only the pulses that fit the spacing of those, so
/// the pulses of another recording across a gap or an overlap are dropped.
fn find_sync_pulses(
    spec: &DecoderSpec,
    field: &DecodedField,
    estimates: Option<(f32, f32)>,
) -> SyncPulses {
    // Speed tolerance of the line spacing, in one direction.
    const SYNC_SPACING_TOLERANCE: f64 = 0.15;
    // Pulses whose spacing fits fewer pulses than this (themselves included)
    // are not on a grid.
    const MIN_GRID_LENGTH: usize = 8;

    let to_inpx = |us: f64| usectoinpx(spec, us);
    let hsync_len = to_inpx(spec.sys_hsync_pulse_us);
    // The porch between the sync pulse and the active video, less 2 us of
    // margin; the blanking level is read halfway into it.
    let porch_len = to_inpx(spec.sys_active_video_us[0] - spec.sys_hsync_pulse_us - 2.0);
    let line_len = to_inpx(spec.sys_line_period).round_ties_even();

    // A boxcar about one sync transition long takes the burst and any pilot
    // tone out of the slicing.
    let window = (to_inpx(0.22) as usize).max(3) | 1;
    let smoothed = boxcar_same(&field.data.video.demod_05, window);

    let (sync_est, blank_est) = estimates.unwrap_or_else(|| level_estimates(&smoothed));

    let width_range = (hsync_len * 0.6, hsync_len * 1.4);
    let mut candidates: Vec<HsyncCandidate> =
        sliced_pulses(&smoothed, (sync_est + blank_est) / 2.0)
            .into_iter()
            .filter(|&(fall, rise)| {
                let width = (rise - fall) as f64;
                width > width_range.0 && width < width_range.1
            })
            .map(|(fall, rise)| HsyncCandidate {
                fall,
                sync_level: window_median(&smoothed, (fall + rise) / 2).unwrap_or(sync_est),
                porch_level: window_median(&smoothed, (rise as f64 + porch_len * 0.5) as usize)
                    .unwrap_or(blank_est),
            })
            .collect();

    // Drop the candidates whose tip level is more than 2.5 MADs off the
    // median: equalizing pulses and static.
    if let Some(median) = upper_median(candidates.iter().map(|c| c.sync_level)) {
        let mad = upper_median(candidates.iter().map(|c| (c.sync_level - median).abs()))
            .filter(|&mad| mad > 0.0)
            .unwrap_or(1.0);
        candidates.retain(|c| (c.sync_level - median).abs() <= 2.5 * mad);
    }

    // Two falling edges fit when their distance, modulo the stretched line
    // length, lands within the jitter window at either end.
    let eff_line_len = line_len * (1.0 + SYNC_SPACING_TOLERANCE);
    let inv_eff_line_len = 1.0 / eff_line_len;
    let jitter = line_len * 0.1;
    let on_grid = |a: usize, b: usize| {
        let distance = a.abs_diff(b) as f64;
        let rem = distance - (distance * inv_eff_line_len).floor() * eff_line_len;
        rem < jitter || rem > eff_line_len - jitter
    };
    let mut support = vec![1usize; candidates.len()];
    for i in 0..candidates.len() {
        for j in i + 1..candidates.len() {
            if on_grid(candidates[i].fall, candidates[j].fall) {
                support[i] += 1;
                support[j] += 1;
            }
        }
    }
    let grid: Vec<HsyncCandidate> = candidates
        .iter()
        .zip(&support)
        .filter(|&(_, &count)| count >= MIN_GRID_LENGTH)
        .map(|(&candidate, _)| candidate)
        .collect();

    let (sync_level, blank_level) = if grid.is_empty() {
        (sync_est, blank_est)
    } else {
        let mean = |level: fn(&HsyncCandidate) -> f32| {
            (grid.iter().map(|c| f64::from(level(c))).sum::<f64>() / grid.len() as f64) as f32
        };
        (mean(|c| c.sync_level), mean(|c| c.porch_level))
    };

    let midpoint = (sync_level + blank_level) / 2.0;
    let pulses: Vec<PulseSample> = sliced_pulses(&smoothed, midpoint)
        .into_iter()
        .filter(|&(fall, _)| grid.iter().any(|c| on_grid(fall, c.fall)))
        .map(|(fall, rise)| {
            let edge = |i: usize| crossing(i, smoothed[i] - midpoint, smoothed[i + 1] - midpoint);
            let start = edge(fall);
            let len = edge(rise) - start;
            PulseSample {
                start: start.round_ties_even() as i64,
                len: len.round_ties_even() as i64,
            }
        })
        .collect();

    SyncPulses {
        pulses,
        sync_level,
        blank_level,
    }
}

pub(crate) fn try_get_pulses(
    field: &mut DecodedField,
    spec: &DecoderSpec,
    inter_field_state: &mut InterFieldState,
    do_level_detect: bool,
) -> Option<PulseResult> {
    // Without level detection the field is sliced between the working levels.
    let estimates = (!do_level_detect).then(|| {
        let levels = inter_field_state.levels;
        (levels.iretohz(spec.sys_vsync_ire), levels.ire0)
    });
    let found = find_sync_pulses(spec, field, estimates);

    let adjust = spec.rf_ire0_adjust;
    if adjust.backporch {
        inter_field_state.levels.ire0 = found.blank_level;
    }
    if adjust.hsync {
        inter_field_state.levels.hz_ire =
            (found.blank_level - found.sync_level) / -spec.sys_vsync_ire;
    }

    if found.pulses.is_empty()
        && (inter_field_state.prev_first_hsync_loc == -1.0 || spec.rf_fallback_vsync)
    {
        return None;
    }

    let result = try_get_pulses_core(spec, &found.pulses, field, inter_field_state);

    field.validpulses = result.valid_starts;
    field.vblank_next = result.vblank_next;
    field.is_first_field = Some(result.is_first_field);
    field.is_progressive_field = Some(result.is_progressive_field);
    if let Some(first_hsync_loc) = result.first_hsync_loc {
        inter_field_state.prev_first_hsync_readloc = field.readloc as i64;
        inter_field_state.prev_first_hsync_loc = first_hsync_loc;
        inter_field_state.prev_first_hsync_diff = result.prev_hsync_diff;
    }
    inter_field_state.prev_first_field = if result.is_first_field { 1 } else { 0 };
    Some(PulseResult {
        line0loc: result.line0loc,
        first_hsync_loc: result.first_hsync_loc,
        first_hsync_loc_line: Some(result.first_hsync_loc_line),
        meanlinelen: result.meanlinelen,
        sync_level: found.sync_level,
        blank_level: found.blank_level,
    })
}

/// Refines the sync tip and blanking levels measured on the hsync pulses
/// against the vertical interval, lines 1 to 9.5 of the field on the
/// full-band demod, whose broad pulses and their gaps hold both levels for
/// far longer than an hsync pulse does. Each sample joins the nearer
/// level's cluster, the clusters are cleaned of outliers, and each level is
/// blended towards its cluster mean by a weight that rises with the
/// cluster's SNR (level separation squared over the cluster variance). A
/// level stays put unless both clusters are populated, the measured
/// separation is within half of the expected one and its SNR is at least 9.
pub(crate) fn refine_levels_from_vsync(
    demod: &[f32],
    line0loc: f64,
    meanlinelen: f64,
    sync_level: f32,
    blank_level: f32,
) -> (f32, f32) {
    const WINDOW_LINES: f64 = 8.5;
    const MIN_YIELD_FRACTION: f32 = 0.15;
    const MIN_YIELD_SAMPLES: usize = 5;
    const OUTLIER_MADS: f32 = 3.5;
    const MAD_FLOOR: f32 = 1e-5;
    const SEPARATION_RANGE: (f32, f32) = (0.5, 1.5);
    const SIGNAL_POWER_FLOOR: f32 = 1e-5;
    const VARIANCE_FLOOR: f32 = 1e-6;
    const MIN_SNR: f32 = 9.0;
    // The SNR at which a level is blended halfway to its cluster mean.
    const HALF_WEIGHT_SNR: f32 = 20.0;

    let start = (line0loc + meanlinelen).round_ties_even();
    let end = (start + meanlinelen * WINDOW_LINES).round_ties_even();
    let Some((start, end)) = demod_slice_bounds(demod.len(), start as i64, end as i64) else {
        return (sync_level, blank_level);
    };
    let window = &demod[start..end];
    let min_yield = ((window.len() as f32 * MIN_YIELD_FRACTION) as usize).max(MIN_YIELD_SAMPLES);

    let (mut sync, mut blank): (Vec<f32>, Vec<f32>) = window
        .iter()
        .partition(|&&v| (v - sync_level).abs() < (v - blank_level).abs());
    let drop_outliers = |samples: &mut Vec<f32>| {
        if samples.len() <= MIN_YIELD_SAMPLES {
            return;
        }
        let median = median_from_values(&mut samples.clone());
        let mut deviations: Vec<f32> = samples.iter().map(|v| (v - median).abs()).collect();
        let mad = median_from_values(&mut deviations).max(MAD_FLOOR);
        samples.retain(|v| (v - median).abs() < OUTLIER_MADS * mad);
    };
    drop_outliers(&mut sync);
    drop_outliers(&mut blank);
    if sync.len() < min_yield || blank.len() < min_yield {
        return (sync_level, blank_level);
    }

    let (sync_mean, blank_mean) = (mean_slice(&sync) as f32, mean_slice(&blank) as f32);
    let measured = blank_mean - sync_mean;
    let expected = blank_level - sync_level;
    if !(SEPARATION_RANGE.0 * expected < measured && measured < SEPARATION_RANGE.1 * expected) {
        return (sync_level, blank_level);
    }
    let signal_power = (measured * measured).max(SIGNAL_POWER_FLOOR);
    let refine = |orig: f32, samples: &[f32], cluster_mean: f32| {
        let variance = samples
            .iter()
            .map(|v| (v - cluster_mean).powi(2))
            .sum::<f32>()
            / samples.len() as f32;
        let snr = signal_power / variance.max(VARIANCE_FLOOR);
        if snr < MIN_SNR {
            return orig;
        }
        let weight = snr / (HALF_WEIGHT_SNR + snr);
        (1.0 - weight) * orig + weight * cluster_mean
    };
    (
        refine(sync_level, &sync, sync_mean),
        refine(blank_level, &blank, blank_mean),
    )
}
