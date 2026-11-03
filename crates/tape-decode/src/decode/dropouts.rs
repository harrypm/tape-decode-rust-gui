use super::*;

fn find_dropouts_rf(
    env: &[f32],
    start_rf: usize,
    end_rf: usize,
    threshold: f32,
    hysteresis: f32,
    merge_threshold: isize,
) -> Vec<(usize, isize)> {
    let down_thresh = threshold;
    let up_thresh = threshold * hysteresis;
    // Each dropout is (start, end); end == -1 marks one still open. Only the
    // most recent dropout is ever extended or reopened, so the open one is
    // always the last.
    let mut dropouts: Vec<(usize, isize)> = Vec::new();

    // A closed state only reacts to value <= down_thresh and an open one only
    // to value >= up_thresh, so a chunk without the relevant crossing (the
    // overwhelmingly common case) is skipped on one vectorizable min/max scan.
    const CHUNK: usize = 64;
    let mut chunk_start = start_rf;
    while chunk_start < end_rf {
        let chunk_end = (chunk_start + CHUNK).min(end_rf);
        let chunk = &env[chunk_start..chunk_end];
        let open = matches!(dropouts.last(), Some(&(_, -1)));
        let skip = if open {
            chunk.iter().fold(f32::NEG_INFINITY, |acc, &v| acc.max(v)) < up_thresh
        } else {
            chunk.iter().fold(f32::INFINITY, |acc, &v| acc.min(v)) > down_thresh
        };
        if !skip {
            for (offset, &value) in chunk.iter().enumerate() {
                let i = chunk_start + offset;
                if value <= down_thresh {
                    match dropouts.last_mut() {
                        Some((_, end)) if *end == -1 => {}
                        // A dropout starting within the merge threshold of the
                        // previous one's end continues it.
                        Some((_, end)) if i as isize - *end <= merge_threshold => *end = -1,
                        _ => dropouts.push((i, -1)),
                    }
                } else if value >= up_thresh {
                    if let Some(last) = dropouts.last_mut() {
                        if last.1 == -1 {
                            last.1 = i as isize;
                        }
                    }
                }
            }
        }
        chunk_start = chunk_end;
    }

    if let Some(last) = dropouts.last_mut() {
        if last.1 == -1 {
            last.1 = end_rf as isize;
        }
    }

    dropouts
}

fn map_dropouts_rf_to_tbc(
    errlist: &[(usize, isize)],
    start_line_idx: usize,
    end_line_idx: usize,
    linelocs: &[f64],
    outlinelen: usize,
    lineoffset: usize,
) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let mut rv_lines = Vec::new();
    let mut rv_starts = Vec::new();
    let mut rv_ends = Vec::new();

    let mut line_idx = start_line_idx;
    let mut line_start_rf = linelocs[line_idx];
    let mut line_end_rf = linelocs[line_idx + 1];

    for &(start_rf, end_rf) in errlist {
        let start_rf = start_rf as f64;
        let end_rf = end_rf as f64;

        while line_idx < end_line_idx {
            let line_len = line_end_rf - line_start_rf;
            if (start_rf >= line_start_rf || line_idx == start_line_idx)
                && start_rf < line_end_rf
                && line_len > 0.0
            {
                rv_lines.push(line_idx - lineoffset);

                let start_rf_linepos = start_rf - line_start_rf;
                let start_linepos =
                    ((start_rf_linepos / line_len) * outlinelen as f64).floor() as usize;
                rv_starts.push(start_linepos);
                break;
            }

            line_idx += 1;
            line_start_rf = linelocs[line_idx];
            line_end_rf = linelocs[line_idx + 1];
        }

        while line_idx < end_line_idx {
            let line_len = line_end_rf - line_start_rf;
            if end_rf < line_end_rf && line_len > 0.0 {
                let end_rf_linepos = end_rf - line_start_rf;
                let end_linepos = ((end_rf_linepos / line_len) * outlinelen as f64).ceil() as usize;
                rv_ends.push(outlinelen.min(end_linepos));
                break;
            }

            rv_ends.push(outlinelen);
            line_idx += 1;

            if line_idx < end_line_idx {
                line_start_rf = linelocs[line_idx];
                line_end_rf = linelocs[line_idx + 1];

                rv_starts.push(0);
                rv_lines.push(line_idx - lineoffset);
            }
        }
    }

    (rv_lines, rv_starts, rv_ends)
}

pub(crate) fn detect_dropouts_rf(
    spec: &DecoderSpec,
    field: &DecodedField,
    merge_threshold: isize,
    min_length: isize,
) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
    let env: &[f32] = &field.data.video.envelope;
    let linelocs = field
        .linelocs
        .as_ref()
        .expect("a valid field has line locations");
    let threshold = spec
        .dod_threshold_a
        .unwrap_or_else(|| mean_slice(env) as f32 * spec.dod_threshold_p);

    // The written rows, reported as fieldLine 1 ..= outlinecount.
    let std::ops::Range {
        start: start_line,
        end: end_line,
    } = field.output_lines();

    let start_rf = linelocs[start_line].floor() as usize;
    let end_rf = env.len().min(linelocs[end_line].ceil() as usize);

    let mut dropouts_rf = find_dropouts_rf(
        env,
        start_rf,
        end_rf,
        threshold,
        spec.dod_hysteresis,
        merge_threshold,
    );
    dropouts_rf.retain(|(start_rf, end_rf)| *end_rf - *start_rf as isize > min_length);

    let (rv_lines, rv_starts, rv_ends) = map_dropouts_rf_to_tbc(
        &dropouts_rf,
        start_line,
        end_line,
        linelocs,
        field.outlinelen,
        field.lineoffset,
    );

    (rv_lines, rv_starts, rv_ends)
}
