//! Isolated integer JPEG AC rate/distortion model, NOT a product codec path.
//! Standard shortest-path idea informed by MozJPEG documentation; no source copied.
//! Models independent-block spectral AC Huffman coding, NOT progressive EOBRUN
//! savings, image-domain quality, optimized future tables, or human perception.

#[derive(Clone, Copy, Debug)]
struct State {
    cost: u64,
    previous: Option<usize>,
    value: i16,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Solution {
    pub(crate) coefficients: [i16; 64],
    cost: u64,
    bits: u32,
}

fn symbol_bits(table: &[u8; 256], symbol: usize) -> Option<u32> {
    (table[symbol] != 0).then_some(u32::from(table[symbol]))
}

fn category(value: i16) -> u32 {
    16 - value.unsigned_abs().leading_zeros()
}

pub(crate) fn rate(
    coefficients: &[i16; 64],
    start: usize,
    end: usize,
    table: &[u8; 256],
) -> Option<u32> {
    let mut zeros = 0;
    let mut bits = 0;
    for &value in &coefficients[start..end] {
        if value == 0 {
            zeros += 1;
            continue;
        }
        let size = category(value);
        if size > 10 {
            return None;
        }
        while zeros >= 16 {
            bits += symbol_bits(table, 0xf0)?;
            zeros -= 16;
        }
        bits += symbol_bits(table, (zeros << 4) | size as usize)? + size;
        zeros = 0;
    }
    if zeros > 0 {
        bits += symbol_bits(table, 0)?;
    }
    Some(bits)
}

fn distortion(
    source: &[i16; 64],
    steps: &[u16; 64],
    coefficients: &[i16; 64],
    start: usize,
    end: usize,
) -> u64 {
    (start..end)
        .map(|i| {
            let residual = i64::from(source[i]) - i64::from(steps[i]) * i64::from(coefficients[i]);
            (residual * residual) as u64
        })
        .sum()
}

pub(crate) fn optimize(
    source: &[i16; 64],
    steps: &[u16; 64],
    baseline: &[i16; 64],
    start: usize,
    end: usize,
    table: &[u8; 256],
    lambda: u32,
) -> Option<Solution> {
    if end.saturating_sub(start) > 32 {
        grouped::solve(source, steps, baseline, start, end, table, lambda)
    } else {
        solve::<true>(source, steps, baseline, start, end, table, lambda)
    }
}

mod grouped;

fn solve<const PRUNE: bool>(
    source: &[i16; 64],
    steps: &[u16; 64],
    baseline: &[i16; 64],
    start: usize,
    end: usize,
    table: &[u8; 256],
    lambda: u32,
) -> Option<Solution> {
    if start == 0
        || start >= end
        || end > 64
        || steps[start..end].iter().any(|&q| q == 0 || q > 2040)
    {
        return None;
    }
    let length = end - start;
    let lambda = u64::from(lambda);
    let mut zeros = [0u64; 65];
    for i in 0..length {
        let value = i64::from(source[start + i]);
        zeros[i + 1] = zeros[i] + (value * value) as u64;
    }
    let mut states = [None::<State>; 64];
    let mut min_prefix = 0u64;
    for i in 0..length {
        if i > 0 {
            let zero_step = zeros[i] - zeros[i - 1];
            min_prefix = min_prefix.saturating_add(zero_step);
            if let Some(prev) = states[i - 1] {
                min_prefix = min_prefix.min(prev.cost);
            }
        }
        let raw = i32::from(source[start + i]);
        let step = u32::from(steps[start + i]);
        let nearest = (raw.unsigned_abs() + step / 2) / step;
        let nearest_cat = if nearest == 0 {
            1u32
        } else {
            (32 - nearest.leading_zeros()).min(10)
        };
        let mut sizes = [0u32; 10];
        sizes[0] = nearest_cat;
        let mut idx = 1usize;
        for s in 1..=10u32 {
            if s != nearest_cat {
                sizes[idx] = s;
                idx += 1;
            }
        }
        for size in sizes {
            // Within a bit category the rate is constant, so its closest
            // magnitude is the exact minimum of the squared-error term.
            let magnitude = nearest.clamp(1 << (size - 1), (1 << size) - 1);
            let value = (if raw < 0 {
                -(magnitude as i32)
            } else {
                magnitude as i32
            }) as i16;
            let residual = i64::from(raw) - i64::from(step) * i64::from(value);
            let error = (residual * residual) as u64;
            // All predecessor, zero-run and bit costs are nonnegative, and every
            // valid symbol code has length >= 1. Equality retains old ties.
            let lower_bound = min_prefix
                .saturating_add(error)
                .saturating_add(lambda.saturating_mul(u64::from(size) + 1));
            if PRUNE && states[i].is_some_and(|best| lower_bound > best.cost) {
                continue;
            }
            for predecessor in 0..=i {
                let previous = predecessor.checked_sub(1);
                let prefix = if let Some(p) = previous {
                    if let Some(s) = states[p] {
                        s.cost
                    } else {
                        continue;
                    }
                } else {
                    0
                };
                let run = i - predecessor;
                let Some(code) = symbol_bits(table, ((run % 16) << 4) | size as usize) else {
                    continue;
                };
                let zrl = if run >= 16 {
                    let Some(bits) = symbol_bits(table, 0xf0) else {
                        continue;
                    };
                    bits * (run / 16) as u32
                } else {
                    0
                };
                let cost = prefix + zeros[i] - zeros[predecessor]
                    + error
                    + lambda * u64::from(code + size + zrl);
                let next = State {
                    cost,
                    previous,
                    value,
                };
                if states[i].is_none_or(|old| {
                    (cost, value.unsigned_abs(), previous)
                        < (old.cost, old.value.unsigned_abs(), old.previous)
                }) {
                    states[i] = Some(next);
                }
            }
        }
    }
    let mut winner =
        symbol_bits(table, 0).map(|bits| (zeros[length] + lambda * u64::from(bits), None));
    for (i, state) in states[..length].iter().enumerate() {
        let Some(state) = state else {
            continue;
        };
        let tail = if i + 1 == length {
            0
        } else {
            let Some(bits) = symbol_bits(table, 0) else {
                continue;
            };
            bits
        };
        let cost = state.cost + zeros[length] - zeros[i + 1] + lambda * u64::from(tail);
        if winner.is_none_or(|old| (cost, Some(i)) < old) {
            winner = Some((cost, Some(i)));
        }
    }
    let (cost, mut position) = winner?;
    let mut coefficients = *baseline;
    coefficients[start..end].fill(0);
    while let Some(i) = position {
        let state = states[i]?;
        coefficients[start + i] = state.value;
        position = state.previous;
    }
    let bits = rate(&coefficients, start, end, table)?;
    debug_assert_eq!(
        cost,
        distortion(source, steps, &coefficients, start, end) + lambda * u64::from(bits)
    );
    Some(Solution {
        coefficients,
        cost,
        bits,
    })
}

#[cfg(test)]
mod pruning_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn table() -> [u8; 256] {
        let mut t = [0; 256];
        t[0] = 4;
        t[0xf0] = 11;
        for run in 0..16 {
            for size in 1..=10 {
                t[(run << 4) | size] = 2 + run as u8 % 5 + size as u8;
            }
        }
        t
    }
    #[test]
    fn shortest_path_matches_exhaustive_signed_coefficient_choices() {
        let table = table();
        for seed in 0..8 {
            let mut source = [0; 64];
            let mut steps = [8; 64];
            let mut baseline = [9; 64];
            let n = 4;
            for i in 1..=n {
                steps[i] = 8 * ((seed + i) % 5 + 1) as u16;
                source[i] = ((seed * 13 + i * 7) % 23) as i16 * 8 - 80;
                baseline[i] = 0;
            }
            for lambda in [0, 1, 8, 100, 1000] {
                let actual =
                    optimize(&source, &steps, &baseline, 1, n + 1, &table, lambda).unwrap();
                let mut expected = u64::MAX;
                for encoded in 0usize..16usize.pow(n as u32) {
                    let mut rest = encoded;
                    let mut candidate = baseline;
                    for i in 1..=n {
                        let m = (rest % 16) as i16;
                        rest /= 16;
                        candidate[i] = if source[i] < 0 { -m } else { m };
                    }
                    let cost = distortion(&source, &steps, &candidate, 1, n + 1)
                        + u64::from(lambda)
                            * u64::from(rate(&candidate, 1, n + 1, &table).unwrap());
                    expected = expected.min(cost);
                }
                assert_eq!(actual.cost, expected, "seed={seed}, lambda={lambda}");
                assert_eq!(actual.coefficients[0], 9);
                assert_eq!(actual.coefficients[n + 1..], baseline[n + 1..]);
            }
        }
    }
    #[test]
    fn run_boundaries_and_eob_costs_are_not_conflated() {
        let t = table();
        for run in [0, 1, 15, 16, 17, 31, 32, 47, 48, 61] {
            let mut c = [0; 64];
            c[1 + run] = 1;
            let expected = (run / 16) as u32 * u32::from(t[0xf0])
                + u32::from(t[((run % 16) << 4) | 1])
                + 1
                + u32::from(t[0]);
            assert_eq!(rate(&c, 1, 64, &t).unwrap(), expected);
        }
        let mut c = [0; 64];
        c[1] = 1;
        assert_eq!(
            rate(&c, 1, 64, &t).unwrap(),
            u32::from(t[1]) + 1 + u32::from(t[0])
        );
        c = [0; 64];
        c[63] = 1;
        assert_eq!(
            rate(&c, 1, 64, &t).unwrap(),
            3 * u32::from(t[0xf0]) + u32::from(t[(14 << 4) | 1]) + 1
        );
    }
    #[test]
    fn whole_band_solution_never_loses_to_legal_baseline_and_is_deterministic() {
        let t = table();
        for seed in 0..32u32 {
            let mut src = [0; 64];
            let mut steps = [8; 64];
            let mut baseline = [0; 64];
            for i in 1..64 {
                let x = (seed + i as u32)
                    .wrapping_mul(747796405)
                    .wrapping_add(2891336453);
                steps[i] = (1 + x % 255) as u16 * 8;
                src[i] = ((x >> 16) % 16001) as i16 - 8000;
                let magnitude = ((i32::from(src[i]).unsigned_abs() + u32::from(steps[i]) / 2)
                    / u32::from(steps[i]))
                .min(1023) as i16;
                baseline[i] = if src[i] < 0 { -magnitude } else { magnitude };
            }
            for lambda in [0, 1, 1000, u32::MAX] {
                let result = optimize(&src, &steps, &baseline, 1, 64, &t, lambda).unwrap();
                let old = distortion(&src, &steps, &baseline, 1, 64)
                    + u64::from(lambda) * u64::from(rate(&baseline, 1, 64, &t).unwrap());
                assert!(result.cost <= old);
                assert_eq!(
                    Some(result),
                    optimize(&src, &steps, &baseline, 1, 64, &t, lambda)
                );
            }
        }
    }
    #[test]
    fn unavailable_codes_and_invalid_quantizers_fail_closed() {
        let source = [0; 64];
        let mut steps = [8; 64];
        let b = [0; 64];
        let t = table();
        assert!(optimize(&source, &steps, &b, 0, 64, &t, 0).is_none());
        assert!(optimize(&source, &steps, &b, 1, 65, &t, 0).is_none());
        assert!(optimize(&source, &steps, &b, 1, 64, &[0; 256], 0).is_none());
        steps[1] = 0;
        assert!(optimize(&source, &steps, &b, 1, 64, &t, 0).is_none());
    }
}
