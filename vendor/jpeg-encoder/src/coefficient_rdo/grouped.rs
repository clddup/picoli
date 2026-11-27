//! Exact predecessor minimization by zero-run residue. No model changes.
use super::*;

#[derive(Clone, Copy)]
struct Group {
    offset: i64,
    previous: Option<usize>,
    predecessor: usize,
}

pub(super) fn solve(
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
    let zrl = u64::from(table[0xf0]);
    let penalty = (lambda * zrl) as i64;
    let mut zeros = [0u64; 65];
    for i in 0..length {
        let v = i64::from(source[start + i]);
        zeros[i + 1] = zeros[i] + (v * v) as u64;
    }
    let mut states = [None::<State>; 64];
    let mut groups = [None::<Group>; 16];
    for i in 0..length {
        let previous = i.checked_sub(1);
        let prefix = match previous {
            None => Some(0),
            Some(p) => states[p].map(|s| s.cost),
        };
        let residue = i % 16;
        // Without ZRL only the latest16 predecessors are legal. Replace the
        // residue even when its new predecessor is unavailable.
        if zrl == 0 {
            groups[residue] = None;
        }
        if let Some(prefix) = prefix {
            // Bounds: legal inputs keep every cost/offset below2^49 in
            // magnitude, including u32::MAX lambda and u8::MAX code lengths.
            let next = Group {
                offset: prefix as i64 - zeros[i] as i64 - penalty * (i / 16) as i64,
                previous,
                predecessor: i,
            };
            if groups[residue]
                .is_none_or(|old| (next.offset, next.previous) < (old.offset, old.previous))
            {
                groups[residue] = Some(next);
            }
        }
        let common = zeros[i] as i64 + penalty * (i / 16) as i64;
        let raw = i32::from(source[start + i]);
        let step = u32::from(steps[start + i]);
        let nearest = (raw.unsigned_abs() + step / 2) / step;
        for size in 1..=10 {
            let magnitude = nearest.clamp(1 << (size - 1), (1 << size) - 1);
            let value = (if raw < 0 {
                -(magnitude as i32)
            } else {
                magnitude as i32
            }) as i16;
            let residual = i64::from(raw) - i64::from(step) * i64::from(value);
            let error = (residual * residual) as u64;
            if states[i].is_some_and(|best| error > best.cost) {
                continue;
            }
            for group in groups.iter().flatten() {
                if zrl == 0 && i - group.predecessor >= 16 {
                    continue;
                }
                let r = group.predecessor % 16;
                let run = (i + 16 - r) % 16;
                let Some(code) = symbol_bits(table, (run << 4) | size as usize) else {
                    continue;
                };
                let prefix = group.offset + common - if r > residue { penalty } else { 0 };
                debug_assert!(prefix >= 0);
                let cost = prefix as u64 + error + lambda * u64::from(code + size);
                let previous = group.previous;
                if states[i].is_none_or(|old| {
                    (cost, value.unsigned_abs(), previous)
                        < (old.cost, old.value.unsigned_abs(), old.previous)
                }) {
                    states[i] = Some(State {
                        cost,
                        previous,
                        value,
                    });
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
mod tests {
    use super::*;
    #[test]
    fn grouped_minima_match_real_direct_solver_including_ties_and_missing_codes() {
        for seed in 0..512u32 {
            let mut source = [0i16; 64];
            let mut steps = [8u16; 64];
            let mut baseline = [9i16; 64];
            let mut table = [0u8; 256];
            let mut state = seed.wrapping_add(1);
            for i in 0..64 {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                source[i] = (state >> 16) as i16;
                steps[i] = 1 + ((state >> 5) % 2040) as u16;
                baseline[i] = (state % 2047) as i16 - 1023;
            }
            for (symbol, code) in table.iter_mut().enumerate() {
                if symbol == 0 || symbol == 0xf0 || (1..=10).contains(&(symbol & 15)) {
                    *code = 1 + ((symbol as u32 + seed * 13) % 255) as u8;
                }
            }
            match seed % 5 {
                0 => table[0xf0] = 0,
                1 => table[0] = 0,
                2 => {
                    for code in table.iter_mut().step_by(7) {
                        *code = 0;
                    }
                }
                _ => {}
            }
            for (start, end) in [
                (1, 64),
                (2, 63),
                (1, 22),
                (22, 43),
                (43, 64),
                (47, 64),
                (61, 64),
            ] {
                for lambda in [0, 1, 16, 256, 1024, u32::MAX] {
                    assert_eq!(
                        solve(&source, &steps, &baseline, start, end, &table, lambda),
                        super::super::solve::<true>(
                            &source, &steps, &baseline, start, end, &table, lambda
                        ),
                        "seed={seed},band={start}..{end},lambda={lambda}"
                    );
                }
            }
        }
    }
    #[test]
    fn invalid_band_or_steps_fail_identically_without_overflow() {
        let source = [i16::MIN; 64];
        let mut steps = [2040u16; 64];
        let baseline = [i16::MAX; 64];
        let table = [255u8; 256];
        for (start, end) in [(0, 64), (64, 64), (1, 65), (64, 1)] {
            assert_eq!(
                solve(&source, &steps, &baseline, start, end, &table, u32::MAX),
                super::super::solve::<true>(
                    &source,
                    &steps,
                    &baseline,
                    start,
                    end,
                    &table,
                    u32::MAX
                )
            );
        }
        for value in [0, 2041, u16::MAX] {
            steps[32] = value;
            assert!(solve(&source, &steps, &baseline, 1, 64, &table, u32::MAX).is_none());
        }
        steps[32] = 2040;
        assert_eq!(
            solve(&source, &steps, &baseline, 1, 64, &table, u32::MAX),
            super::super::solve::<true>(&source, &steps, &baseline, 1, 64, &table, u32::MAX)
        );
    }
}
