//! Deterministic single/pair exchange on frozen separable proposal curves.
//! This is a bounded heuristic, not a global knapsack/actual-JPEG optimum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub quantizer: u16,
    pub retain: bool,
    pub pixel: i128,
    pub edge: i128,
    pub tile: i128,
    pub rate: i64,
}
impl Point {
    pub fn keep(quantizer: u16) -> Self {
        Self {
            quantizer,
            retain: true,
            pixel: 0,
            edge: 0,
            tile: 0,
            rate: 0,
        }
    }
}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Total {
    pub pixel: i128,
    pub edge: i128,
    pub tile: i128,
    pub rate: i64,
}
impl Total {
    fn exchange(self, old: Point, new: Point) -> Self {
        Self {
            pixel: self.pixel + new.pixel - old.pixel,
            edge: self.edge + new.edge - old.edge,
            tile: self.tile + new.tile - old.tile,
            rate: self.rate + new.rate - old.rate,
        }
    }
    fn feasible(self) -> bool {
        self.pixel <= 0 && self.edge <= 0 && self.tile <= 0
    }
}
fn dominates(a: Point, b: Point) -> bool {
    a.rate <= b.rate && a.pixel <= b.pixel && a.edge <= b.edge && a.tile <= b.tile
}
/// Preserve the explicit retain option at index0 even if another point dominates
/// it. Equal model points keep the earlier syntax choice deterministically.
pub fn pareto(points: &[Point]) -> Vec<Point> {
    let mut result = Vec::new();
    for (i, &p) in points.iter().enumerate() {
        if i == 0
            || !points.iter().enumerate().any(|(j, &q)| {
                j != i
                    && dominates(q, p)
                    && (q.rate < p.rate
                        || q.pixel < p.pixel
                        || q.edge < p.edge
                        || q.tile < p.tile
                        || j < i)
            })
        {
            result.push(p);
        }
    }
    result
}
pub struct Solution {
    pub choices: Vec<usize>,
    pub total: Total,
    pub exchanges: usize,
}
/// One natural-frequency pass. Every accepted exchange involves the current
/// frequency, optionally one other frequency, and strictly reduces model rate
/// while respecting all three unchanged model budgets. Curves never change.
pub fn solve(curves: &[Vec<Point>], paired: bool) -> Solution {
    assert!(curves.iter().all(|v| v.first().is_some_and(|p| p.retain
        && p.pixel == 0
        && p.edge == 0
        && p.tile == 0
        && p.rate == 0)));
    let mut choices = vec![0; curves.len()];
    let mut total = Total::default();
    let mut exchanges = 0;
    for f in 1..curves.len() {
        let mut best = None;
        for (a, &point) in curves[f].iter().enumerate() {
            if a == choices[f] {
                continue;
            }
            let base = total.exchange(curves[f][choices[f]], point);
            if base.feasible() && base.rate < total.rate {
                let key = (
                    base.rate,
                    base.pixel,
                    base.edge,
                    base.tile,
                    usize::MAX,
                    a,
                    0,
                );
                if best.as_ref().is_none_or(|(old, _, _, _)| key < *old) {
                    best = Some((key, a, None, base));
                }
            }
            if !paired {
                continue;
            }
            for g in 1..curves.len() {
                if g == f {
                    continue;
                }
                for (b, &other) in curves[g].iter().enumerate() {
                    if b == choices[g] {
                        continue;
                    }
                    let candidate = base.exchange(curves[g][choices[g]], other);
                    if !candidate.feasible() || candidate.rate >= total.rate {
                        continue;
                    }
                    let key = (
                        candidate.rate,
                        candidate.pixel,
                        candidate.edge,
                        candidate.tile,
                        g,
                        a,
                        b,
                    );
                    if best.as_ref().is_none_or(|(old, _, _, _)| key < *old) {
                        best = Some((key, a, Some((g, b)), candidate));
                    }
                }
            }
        }
        if let Some((_, a, other, new_total)) = best {
            choices[f] = a;
            if let Some((g, b)) = other {
                choices[g] = b;
            }
            total = new_total;
            exchanges += 1;
        }
    }
    Solution {
        choices,
        total,
        exchanges,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paired_exchange_crosses_a_barrier_that_no_single_feasible_move_can_cross() {
        let keep = Point::keep(8);
        let credit = Point {
            quantizer: 7,
            retain: false,
            pixel: -10,
            edge: -20,
            tile: -5,
            rate: 3,
        };
        let saving = Point {
            quantizer: 10,
            retain: false,
            pixel: 8,
            edge: 19,
            tile: 4,
            rate: -9,
        };
        let curves = vec![vec![keep], vec![keep, credit], vec![keep, saving]];
        assert_eq!(solve(&curves, false).total, Total::default());
        let paired = solve(&curves, true);
        assert_eq!(paired.choices, vec![0, 1, 1]);
        assert_eq!(
            paired.total,
            Total {
                pixel: -2,
                edge: -1,
                tile: -1,
                rate: -6
            }
        );
        assert_eq!(paired.exchanges, 1);
    }
    #[test]
    fn every_selected_model_point_is_feasible_and_pareto_pruning_keeps_retain_and_ties() {
        let keep = Point::keep(8);
        let good = Point {
            quantizer: 4,
            retain: false,
            pixel: -1,
            edge: -1,
            tile: -1,
            rate: -1,
        };
        let duplicate = Point {
            quantizer: 5,
            ..good
        };
        let bad = Point {
            quantizer: 9,
            retain: false,
            pixel: 10,
            edge: 10,
            tile: 10,
            rate: 10,
        };
        let front = pareto(&[keep, good, duplicate, bad]);
        assert_eq!(front, vec![keep, good]);
        let curves = vec![vec![keep], front.clone(), front];
        let result = solve(&curves, true);
        let mut total = Total::default();
        for (f, &choice) in result.choices.iter().enumerate() {
            let p = curves[f][choice];
            total = total.exchange(keep, p);
        }
        assert_eq!(result.total, total);
        assert!(total.feasible());
        assert!(total.rate < 0);
    }
}
