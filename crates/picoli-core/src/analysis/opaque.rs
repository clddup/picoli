//! Exact streaming opaque scoring for palette mapping; alpha uses the existing
//! background-composited scorer. Only final denominators and complete tiles may
//! reject a candidate. No image-quality threshold is changed here.
use super::{Analysis, Score, distance};
use crate::quality::Limits;

pub(crate) struct OpaqueContext<'a> {
    analysis: &'a Analysis,
    limits: Limits,
    final_weight: u64,
}

impl<'a> OpaqueContext<'a> {
    pub(crate) fn new(analysis: &'a Analysis, limits: Limits) -> Option<Self> {
        if analysis.alpha.is_some() {
            return None;
        }
        Some(Self {
            analysis,
            limits,
            final_weight: analysis
                .weights
                .iter()
                .map(|&w| u64::from(w))
                .sum::<u64>()
                .max(1),
        })
    }

    pub(crate) fn start(&self) -> OpaqueScorer<'a> {
        let width = self.analysis.dimensions.0 as usize;
        OpaqueScorer {
            analysis: self.analysis,
            limits: self.limits,
            final_weight: self.final_weight,
            previous_y: vec![0; width],
            tile_row: vec![(0, 0); width.div_ceil(16)],
            left_y: 0,
            pixel_error: 0,
            edge_error: 0,
            total_weight: 0,
            worst_tile_error: 0.0,
        }
    }
}

pub(crate) struct OpaqueScorer<'a> {
    analysis: &'a Analysis,
    limits: Limits,
    final_weight: u64,
    previous_y: Vec<i32>,
    tile_row: Vec<(u64, u64)>,
    left_y: i32,
    pixel_error: u64,
    edge_error: u64,
    total_weight: u64,
    worst_tile_error: f64,
}

impl OpaqueScorer<'_> {
    pub(crate) fn push(&mut self, index: usize, candidate: [i32; 3]) -> bool {
        let width = self.analysis.dimensions.0 as usize;
        let height = self.analysis.dimensions.1 as usize;
        let x = index % width;
        let y = index / width;
        let reference = self.analysis.colors.at(index);
        let weight = u64::from(self.analysis.weights[index]);
        let error = weight * distance(reference, candidate);
        self.pixel_error += error;
        self.total_weight += weight;
        let tile = &mut self.tile_row[x / 16];
        tile.0 += error;
        tile.1 += weight;
        if x > 0 {
            let delta = (reference[0] - self.analysis.colors.at(index - 1)[0])
                - (candidate[0] - self.left_y);
            self.edge_error += weight * (i64::from(delta) * i64::from(delta)) as u64;
        }
        if y > 0 {
            let delta = (reference[0] - self.analysis.colors.at(index - width)[0])
                - (candidate[0] - self.previous_y[x]);
            self.edge_error += weight * (i64::from(delta) * i64::from(delta)) as u64;
        }
        self.previous_y[x] = candidate[0];
        self.left_y = candidate[0];
        // All rows AND columns of this tile are now complete. A partial tile
        // ratio can fall and must never trigger rejection.
        if ((y + 1).is_multiple_of(16) || y + 1 == height)
            && ((x + 1).is_multiple_of(16) || x + 1 == width)
        {
            let error = tile.0 as f64 / tile.1.max(1) as f64;
            if error > self.limits.tile {
                return false;
            }
            self.worst_tile_error = self.worst_tile_error.max(error);
            *tile = (0, 0);
        }
        if x + 1 == width
            && (self.pixel_error as f64 / self.final_weight as f64 > self.limits.pixel
                || self.edge_error as f64 / self.final_weight as f64 > self.limits.edge)
        {
            return false;
        }
        true
    }

    pub(crate) fn finish(self) -> Score {
        let weighted_pixel_error = self.pixel_error as f64 / self.total_weight.max(1) as f64;
        let weighted_edge_error = self.edge_error as f64 / self.total_weight.max(1) as f64;
        let worst_tile_error = self.worst_tile_error;
        Score {
            weighted_pixel_error,
            weighted_edge_error,
            worst_tile_error,
            loss: weighted_pixel_error + 2.0 * weighted_edge_error + 0.1 * worst_tile_error,
        }
    }
}
