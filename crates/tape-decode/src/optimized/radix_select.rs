//! Exact order statistics by radix counting.

/// Order-preserving key of an f32 under `f32::total_cmp`.
#[inline(always)]
pub(crate) fn f32_total_order_key(value: f32) -> u32 {
    let bits = value.to_bits();
    if bits & 0x8000_0000 != 0 {
        !bits
    } else {
        bits | 0x8000_0000
    }
}

pub(crate) fn f32_from_total_order_key(key: u32) -> f32 {
    f32::from_bits(if key & 0x8000_0000 != 0 {
        key & 0x7FFF_FFFF
    } else {
        !key
    })
}

/// The keys at the given 0-based ranks of `values` sorted by `key`, found by
/// a two-level 16-bit radix count: one pass over the values counting the high
/// halves, then one pass counting the low halves of every value in a high
/// bucket some rank falls in. Nothing is copied or partitioned. Every rank
/// must be below `values.len()`.
pub(crate) fn select_ranks_by_key<T: Copy, const N: usize>(
    values: &[T],
    key: impl Fn(T) -> u32,
    ranks: [usize; N],
) -> [u32; N] {
    const RADIX_BITS: u32 = 16;
    const RADIX: usize = 1 << RADIX_BITS;
    const LOW_MASK: u32 = RADIX as u32 - 1;
    assert!(ranks.iter().all(|&rank| rank < values.len()));

    let mut high_counts = vec![0u32; RADIX];
    for &value in values {
        high_counts[(key(value) >> RADIX_BITS) as usize] += 1;
    }
    let located = ranks.map(|rank| bucket_holding(&high_counts, rank));

    let mut buckets: Vec<usize> = located.iter().map(|&(high, _)| high).collect();
    buckets.sort_unstable();
    buckets.dedup();
    let mut low_counts = vec![0u32; RADIX * buckets.len()];
    for &value in values {
        let key = key(value);
        let high = (key >> RADIX_BITS) as usize;
        if let Some(slot) = buckets.iter().position(|&bucket| bucket == high) {
            low_counts[slot * RADIX + (key & LOW_MASK) as usize] += 1;
        }
    }
    located.map(|(high, rank_in_high)| {
        let slot = buckets.iter().position(|&bucket| bucket == high).unwrap();
        let (low, _) = bucket_holding(&low_counts[slot * RADIX..][..RADIX], rank_in_high);
        ((high as u32) << RADIX_BITS) | low as u32
    })
}

/// The bucket of `counts` holding the item of the given rank, and the item's
/// rank within that bucket.
fn bucket_holding(counts: &[u32], rank: usize) -> (usize, usize) {
    let mut before = 0usize;
    for (bucket, &count) in counts.iter().enumerate() {
        let next = before + count as usize;
        if next > rank {
            return (bucket, rank - before);
        }
        before = next;
    }
    unreachable!("rank beyond the counted items")
}
