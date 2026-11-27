//! One natural-frequency pass, fixed top8 move shortlist. Evaluate the complete
//! accumulated Q14/current-Jacobian field before each commit, including cross
//! terms. This is exact only within that approximate model, not actual JPEG RGB.
use super::{
    model::{self, Model},
    projection::{self, basis::BASIS},
    solver::{Point, Solution, Total},
};
use crate::{Error, analysis::Analysis};
use image::RgbImage;
use serde::Serialize;
use zune_jpeg::QuantizedCoefficients;
const Q: i128 = 256 * (1 << 14);
const SHORTLIST: usize = 8;
#[derive(Default, Serialize)]
pub struct Info {
    pub trials: usize,
    pub rejected: usize,
    pub accepted: usize,
}
#[derive(Clone, Copy, Debug)]
struct Change {
    first: (usize, usize),
    second: Option<(usize, usize)>,
}
type Key = (i64, i128, i128, i128, usize, usize, usize);
struct Move {
    key: Key,
    change: Change,
    total: Total,
}
fn exchange(t: Total, old: Point, new: Point) -> Total {
    Total {
        pixel: t.pixel + new.pixel - old.pixel,
        edge: t.edge + new.edge - old.edge,
        tile: t.tile + new.tile - old.tile,
        rate: t.rate + new.rate - old.rate,
    }
}
fn feasible(t: Total) -> bool {
    t.pixel <= 0 && t.edge <= 0 && t.tile <= 0
}
fn offer(moves: &mut Vec<Move>, candidate: Move) {
    if moves.len() == SHORTLIST && candidate.key >= moves[SHORTLIST - 1].key {
        return;
    }
    let i = moves
        .binary_search_by(|m| m.key.cmp(&candidate.key))
        .unwrap_or_else(|i| i);
    moves.insert(i, candidate);
    if moves.len() > SHORTLIST {
        moves.pop();
    }
}
fn search(
    curves: &[Vec<Point>],
    paired: bool,
    mut guard: impl FnMut(&[usize], Change) -> Result<bool, Error>,
) -> Result<(Solution, Info), Error> {
    let mut choices = vec![0; curves.len()];
    let mut total = Total::default();
    let mut info = Info::default();
    for f in 1..curves.len() {
        let mut moves = Vec::with_capacity(SHORTLIST + 1);
        for (a, &point) in curves[f].iter().enumerate() {
            if a == choices[f] {
                continue;
            }
            let base = exchange(total, curves[f][choices[f]], point);
            if feasible(base) && base.rate < total.rate {
                offer(
                    &mut moves,
                    Move {
                        key: (
                            base.rate,
                            base.pixel,
                            base.edge,
                            base.tile,
                            usize::MAX,
                            a,
                            0,
                        ),
                        change: Change {
                            first: (f, a),
                            second: None,
                        },
                        total: base,
                    },
                );
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
                    let candidate = exchange(base, curves[g][choices[g]], other);
                    if feasible(candidate) && candidate.rate < total.rate {
                        offer(
                            &mut moves,
                            Move {
                                key: (
                                    candidate.rate,
                                    candidate.pixel,
                                    candidate.edge,
                                    candidate.tile,
                                    g,
                                    a,
                                    b,
                                ),
                                change: Change {
                                    first: (f, a),
                                    second: Some((g, b)),
                                },
                                total: candidate,
                            },
                        );
                    }
                }
            }
        }
        for candidate in moves {
            info.trials += 1;
            if guard(&choices, candidate.change)? {
                choices[f] = candidate.change.first.1;
                if let Some((g, b)) = candidate.change.second {
                    choices[g] = b;
                }
                total = candidate.total;
                info.accepted += 1;
                break;
            }
            info.rejected += 1;
        }
    }
    Ok((
        Solution {
            choices,
            total,
            exchanges: info.accepted,
        },
        info,
    ))
}
struct State<'a> {
    model: &'a Model,
    frame: &'a QuantizedCoefficients,
    width: usize,
    height: usize,
    cols: usize,
    factors: Vec<[i32; 2]>,
    field: Vec<i64>,
    pixel: i128,
    tile: i128,
}
impl<'a> State<'a> {
    fn new(
        model: &'a Model,
        frame: &'a QuantizedCoefficients,
        analysis: &Analysis,
        current: &RgbImage,
    ) -> Result<Self, Error> {
        let (width, height) = current.dimensions();
        let n = width as usize * height as usize;
        let mut factors = Vec::new();
        factors
            .try_reserve_exact(n)
            .map_err(|_| Error::Limit("coupled factors allocation failed"))?;
        for (i, p) in current.pixels().enumerate() {
            let (a, b) = projection::factors(analysis.colors[i], p.0, analysis.weights[i]);
            factors.push([
                i32::try_from(a).map_err(|_| Error::Limit("coupled quadratic factor overflow"))?,
                i32::try_from(b).map_err(|_| Error::Limit("coupled linear factor overflow"))?,
            ]);
        }
        let mut field = Vec::new();
        field
            .try_reserve_exact(n)
            .map_err(|_| Error::Limit("coupled field allocation failed"))?;
        field.resize(n, 0);
        Ok(Self {
            model,
            frame,
            width: width as usize,
            height: height as usize,
            cols: (width as usize).div_ceil(8),
            factors,
            field,
            pixel: 0,
            tile: 0,
        })
    }
    fn delta(&self, index: usize, f: usize, choice: usize) -> Result<i64, Error> {
        let point = self.model.curves[f][choice];
        if point.retain {
            return Ok(0);
        }
        let coefficient = model::quantize(self.model.raw[index][f], point.quantizer)
            .ok_or(Error::Argument("coupled quantizer invalid"))?;
        let first =
            (index / self.cols * self.frame.components[0].block_width + index % self.cols) * 64;
        Ok(i64::from(point.quantizer) * i64::from(coefficient)
            - i64::from(self.frame.components[0].quantization_table[f])
                * i64::from(self.frame.components[0].coefficients[first + f]))
    }
    fn step(&self, index: usize, choices: &[usize], change: Change) -> Result<(i64, i64), Error> {
        let (f, a) = change.first;
        let first = self.delta(index, f, a)? - self.delta(index, f, choices[f])?;
        let second = if let Some((g, b)) = change.second {
            self.delta(index, g, b)? - self.delta(index, g, choices[g])?
        } else {
            0
        };
        Ok((first, second))
    }
    fn visit(
        &self,
        choices: &[usize],
        change: Change,
        mut pixel: impl FnMut(usize, i64, bool),
    ) -> Result<(), Error> {
        let f = change.first.0;
        let g = change.second.map_or(0, |p| p.0);
        for index in 0..self.model.raw.len() {
            let (a, b) = self.step(index, choices, change)?;
            if a == 0 && b == 0 {
                continue;
            }
            let (bx, by) = (index % self.cols, index / self.cols);
            let critical = self.model.critical_tile == (bx / 2, by / 2);
            for (t, (&fb, &gb)) in BASIS[f].iter().zip(&BASIS[g]).enumerate() {
                let (x, y) = (bx * 8 + t % 8, by * 8 + t / 8);
                if x >= self.width || y >= self.height {
                    continue;
                }
                pixel(
                    y * self.width + x,
                    a * i64::from(fb) + b * i64::from(gb),
                    critical,
                );
            }
        }
        Ok(())
    }
    fn preview(&self, choices: &[usize], change: Change) -> Result<(i128, i128), Error> {
        let (mut pixel, mut tile) = (0, 0);
        self.visit(choices, change, |i, step, critical| {
            let d = i128::from(step);
            let old = i128::from(self.field[i]);
            let [a, b] = self.factors[i];
            let delta = i128::from(a) * (2 * old * d + d * d) - 2 * Q * i128::from(b) * d;
            pixel += delta;
            if critical {
                tile += delta;
            }
        })?;
        Ok((pixel, tile))
    }
    fn try_move(&mut self, choices: &[usize], change: Change) -> Result<bool, Error> {
        let (pixel, tile) = self.preview(choices, change)?;
        if self.pixel + pixel > 0 || self.tile + tile > 0 {
            return Ok(false);
        }
        // Compute every step fallibly before mutation. The small per-block cache
        // avoids borrowing mutable field and immutable layout through one visitor.
        let mut steps = Vec::new();
        steps
            .try_reserve_exact(self.model.raw.len())
            .map_err(|_| Error::Limit("coupled step cache allocation failed"))?;
        for index in 0..self.model.raw.len() {
            steps.push(self.step(index, choices, change)?);
        }
        let f = change.first.0;
        let g = change.second.map_or(0, |p| p.0);
        for (index, (a, b)) in steps.into_iter().enumerate() {
            if a == 0 && b == 0 {
                continue;
            }
            let (bx, by) = (index % self.cols, index / self.cols);
            for (t, (&fb, &gb)) in BASIS[f].iter().zip(&BASIS[g]).enumerate() {
                let (x, y) = (bx * 8 + t % 8, by * 8 + t / 8);
                if x < self.width && y < self.height {
                    self.field[y * self.width + x] += a * i64::from(fb) + b * i64::from(gb);
                }
            }
        }
        self.pixel += pixel;
        self.tile += tile;
        Ok(true)
    }
}
pub fn solve(
    model: &Model,
    frame: &QuantizedCoefficients,
    analysis: &Analysis,
    current: &RgbImage,
    paired: bool,
) -> Result<(Solution, Info), Error> {
    let mut state = State::new(model, frame, analysis, current)?;
    let (mut solution, info) = search(&model.curves, paired, |choices, change| {
        state.try_move(choices, change)
    })?;
    // Expose the complete joint-model deltas, not the linear shortlist estimates.
    solution.total.pixel = state.pixel;
    solution.total.tile = state.tile;
    Ok((solution, info))
}
#[cfg(test)]
mod tests;
