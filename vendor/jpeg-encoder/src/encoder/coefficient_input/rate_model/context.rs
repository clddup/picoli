//! Exact standard independent-AC single-category replacement deltas.
use super::{CoefficientRateModel, ZIGZAG};
impl CoefficientRateModel {
    /// For every NATURAL AC position, report the change in independent standard
    /// Huffman bits for replacing its coefficient by category0..10 (0=zero).
    /// Sign/amplitude value within a category does not affect this bit count.
    /// Neighbors stay at their CURRENT categories. Summing multiple frequency
    /// replacements is only a linearized proposal model, not exact joint rate.
    pub fn category_replacement_deltas(
        &self,
        natural: &[i16; 64],
        chroma: bool,
    ) -> Option<[[i32; 11]; 64]> {
        let mut categories = [0usize; 64];
        for position in 1..64 {
            categories[position] = (16
                - natural[ZIGZAG[position] as usize & 63]
                    .unsigned_abs()
                    .leading_zeros()) as usize;
            if categories[position] > 10 {
                return None;
            }
        }
        let codes = &self.codes[usize::from(chroma)];
        let token = |run: usize, category: usize| -> Option<i32> {
            let symbol = (run % 16) * 16 + category;
            if category == 0 || codes[symbol] == 0 || (run >= 16 && codes[240] == 0) {
                return None;
            }
            Some(
                (run / 16) as i32 * i32::from(codes[240])
                    + i32::from(codes[symbol])
                    + category as i32,
            )
        };
        if codes[0] == 0 {
            return None;
        }
        let mut result = [[0; 11]; 64];
        for (start, end) in crate::progressive::ac_ranges(self.scans) {
            let mut left = [0usize; 64];
            let mut next = [end; 64];
            let mut run = 0;
            for position in start..end {
                left[position] = run;
                if categories[position] == 0 {
                    run += 1;
                } else {
                    run = 0;
                }
            }
            let mut after = end;
            for position in (start..end).rev() {
                next[position] = after;
                if categories[position] != 0 {
                    after = position;
                }
            }
            for position in start..end {
                let following = next[position];
                let right = following - position - 1;
                let cost = |category: usize| -> Option<i32> {
                    if category == 0 {
                        if following < end {
                            token(left[position] + 1 + right, categories[following])
                        } else {
                            Some(i32::from(codes[0]))
                        }
                    } else {
                        let suffix = if following < end {
                            token(right, categories[following])?
                        } else if right > 0 {
                            i32::from(codes[0])
                        } else {
                            0
                        };
                        token(left[position], category)?.checked_add(suffix)
                    }
                };
                let baseline = cost(categories[position])?;
                for (category, delta) in result[ZIGZAG[position] as usize & 63]
                    .iter_mut()
                    .enumerate()
                {
                    *delta = cost(category)? - baseline;
                }
            }
        }
        Some(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_context_delta_matches_complete_rate_for_signed_sparse_dense_and_zrl_blocks() {
        let mut seed = 80u64;
        for scans in [2, 4] {
            for chroma in [false, true] {
                let model = CoefficientRateModel::new(scans).unwrap();
                for pattern in 0..48 {
                    let mut natural = [0; 64];
                    natural[0] = i16::MAX;
                    for (i, value) in natural.iter_mut().enumerate().skip(1) {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        if pattern == 0
                            || (pattern % 3 == 0 && i % 17 != 0)
                            || (pattern % 3 == 1 && seed & 3 != 0)
                        {
                            continue;
                        }
                        *value = ((seed >> 32) % 31) as i16 - 15;
                    }
                    let baseline = model.bits(&natural, chroma).unwrap() as i32;
                    let table = model.category_replacement_deltas(&natural, chroma).unwrap();
                    assert_eq!(table[0], [0; 11]);
                    for position in 1..64 {
                        let index = CoefficientRateModel::natural_index(position).unwrap();
                        for (category, &delta) in table[index].iter().enumerate() {
                            for sign in [-1i16, 1] {
                                let mut replacement = natural;
                                replacement[index] = if category == 0 {
                                    0
                                } else {
                                    sign * (1i16 << (category - 1))
                                };
                                assert_eq!(
                                    delta,
                                    model.bits(&replacement, chroma).unwrap() as i32 - baseline,
                                    "{:?}",
                                    (scans, chroma, pattern, position, category)
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
