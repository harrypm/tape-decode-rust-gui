/// Mean of the `window` (odd) samples centred on each sample, with the ends
/// zero-padded. The sums run in f64 so their rounding stays far below the
/// resolution of the f32 output. They are formed one tap at a time over a
/// block of outputs, which vectorizes, instead of as a running sum whose
/// add and subtract per sample form a serial chain.
pub(crate) fn boxcar_same(data: &[f32], window: usize) -> Vec<f32> {
    debug_assert!(window % 2 == 1);
    const BLOCK: usize = 512;
    let n = data.len();
    let half = window / 2;
    let scale = 1.0 / window as f64;
    let mean_at = |i: usize| -> f32 {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        (data[lo..hi].iter().map(|&v| f64::from(v)).sum::<f64>() * scale) as f32
    };
    // Outputs whose whole window lies inside the data.
    let interior = half..n.saturating_sub(half);

    let mut out = Vec::with_capacity(n);
    out.extend((0..interior.start.min(n)).map(mean_at));
    let mut acc = [0.0f64; BLOCK];
    let mut start = interior.start;
    while start < interior.end {
        let len = (interior.end - start).min(BLOCK);
        let acc = &mut acc[..len];
        acc.fill(0.0);
        for tap in 0..window {
            let taps = &data[start - half + tap..start - half + tap + len];
            for (sum, &value) in acc.iter_mut().zip(taps) {
                *sum += f64::from(value);
            }
        }
        out.extend(acc.iter().map(|&sum| (sum * scale) as f32));
        start += len;
    }
    out.extend((interior.end.max(interior.start)..n).map(mean_at));
    out
}
