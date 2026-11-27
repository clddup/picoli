//! Integer content features shared by palette mapping and JPEG table search.
use crate::{Error, resources};
use image::RgbImage;
use serde::Serialize;
pub(crate) mod opaque;
#[cfg(test)]
mod opaque_tests;
mod transparent;
#[cfg(test)]
mod transparent_tests;

pub(crate) fn color(rgb: [u8; 3]) -> [i32; 3] {
    let [r, g, b] = rgb.map(i32::from);
    [(77 * r + 150 * g + 29 * b) >> 8, r - b, g - ((r + b) >> 1)]
}

pub(crate) fn distance(a: [i32; 3], b: [i32; 3]) -> u64 {
    let [y, co, cg] = std::array::from_fn(|i| i64::from(a[i] - b[i]));
    (4 * y * y + co * co + cg * cg) as u64
}

#[derive(Debug)]
pub struct Analysis {
    pub(crate) weights: Vec<u8>,
    pub(crate) colors: Vec<[i32; 3]>,
    pub(crate) frequency_energy: [u64; 64],
    pub(crate) dimensions: (u32, u32),
    pub(crate) memory_budget_bytes: u64,
    alpha: Option<image::GrayImage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Score {
    pub weighted_pixel_error: f64,
    pub weighted_edge_error: f64,
    pub worst_tile_error: f64,
    /// Research ranking only. Not a calibrated visual-quality pass/fail gate.
    pub loss: f64,
}

impl Analysis {
    pub fn new(image: &RgbImage) -> Result<Self, Error> {
        Self::with_budget(image, resources::DEFAULT_MEMORY_BYTES)
    }

    pub fn with_budget(image: &RgbImage, memory_budget_bytes: u64) -> Result<Self, Error> {
        let (w, h) = image.dimensions();
        resources::Plan::new(w, h, 0, memory_budget_bytes, 1)?;
        let w = w as usize;
        let h = h as usize;
        let colors: Vec<_> = image.pixels().map(|p| color(p.0)).collect();
        let mut raw = vec![1u8; colors.len()];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let mut activity = 0;
                for j in [y * w + (x + 1).min(w - 1), (y + 1).min(h - 1) * w + x] {
                    activity += (colors[i][0] - colors[j][0]).unsigned_abs();
                    activity += ((colors[i][1] - colors[j][1]).unsigned_abs()
                        + (colors[i][2] - colors[j][2]).unsigned_abs())
                        / 4;
                }
                raw[i] = 1 + (activity / 24).min(7) as u8;
            }
        }
        let mut weights = raw.clone();
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                for j in [
                    y * w + x.saturating_sub(1),
                    y * w + (x + 1).min(w - 1),
                    y.saturating_sub(1) * w + x,
                    (y + 1).min(h - 1) * w + x,
                ] {
                    weights[i] = weights[i].max(raw[j]);
                }
            }
        }
        let frequency_energy = spectrum(&colors, &weights, w, h);
        Ok(Self {
            weights,
            colors,
            frequency_energy,
            dimensions: image.dimensions(),
            memory_budget_bytes,
            alpha: None,
        })
    }

    pub fn for_reference(
        reference: &crate::Reference,
        memory_budget_bytes: u64,
    ) -> Result<Self, Error> {
        resources::Plan::with_metadata(
            reference.pixels.width(),
            reference.pixels.height(),
            0,
            reference.jpeg_metadata.len(),
            memory_budget_bytes,
            1,
        )?;
        let mut analysis = Self::with_budget(&reference.pixels, memory_budget_bytes)?;
        if let Some(alpha) = &reference.alpha {
            if alpha.dimensions() != reference.pixels.dimensions() {
                return Err(Error::Argument("alpha dimensions differ from RGB"));
            }
            let width = alpha.width() as usize;
            let values = alpha.as_raw();
            for (i, weight) in analysis.weights.iter_mut().enumerate() {
                if values[i] == 0 {
                    *weight = 0;
                    continue;
                }
                let mut edge = 0;
                if i % width > 0 {
                    edge = edge.max(values[i].abs_diff(values[i - 1]));
                }
                if i >= width {
                    edge = edge.max(values[i].abs_diff(values[i - width]));
                }
                *weight = (*weight).max(1 + (edge / 32).min(7));
            }
            analysis.alpha = Some(alpha.clone());
        }
        Ok(analysis)
    }

    fn composite(&self, value: [i32; 3], i: usize, background: i32) -> [i32; 3] {
        let Some(alpha) = &self.alpha else {
            return value;
        };
        let a = i32::from(alpha.as_raw()[i]);
        [
            (value[0] * a + background * (255 - a) + 127) / 255,
            value[1] * a / 255,
            value[2] * a / 255,
        ]
    }

    /// Candidate RGB is evaluated with the unchanged reference alpha. Encoding
    /// must preserve every alpha byte; transparency is never a quality knob.
    pub fn score(&self, candidate: &RgbImage) -> Result<Score, Error> {
        self.score_impl(candidate, None)?.ok_or_else(|| {
            Error::Encoding("unbounded scoring unexpectedly rejected a candidate".into())
        })
    }

    /// Reject only when accumulated nonnegative error proves a frozen limit
    /// impossible. Accepted candidates receive exactly the full score.
    pub fn score_with_limits(
        &self,
        candidate: &RgbImage,
        limits: crate::quality::Limits,
    ) -> Result<Option<Score>, Error> {
        self.score_impl(candidate, Some(limits))
    }

    fn score_impl(
        &self,
        candidate: &RgbImage,
        limits: Option<crate::quality::Limits>,
    ) -> Result<Option<Score>, Error> {
        if self.alpha.is_some() {
            self.score_transparent_fused(candidate, limits)
        } else {
            self.score_on_background(candidate, 0, limits)
        }
    }

    fn score_on_background(
        &self,
        candidate: &RgbImage,
        background: i32,
        limits: Option<crate::quality::Limits>,
    ) -> Result<Option<Score>, Error> {
        if candidate.dimensions() != self.dimensions {
            return Err(Error::Argument("score dimensions differ from analysis"));
        }
        let (w, h) = (self.dimensions.0 as usize, self.dimensions.1 as usize);
        let tile_width = w.div_ceil(16);
        let mut tiles = vec![(0u64, 0u64); tile_width * h.div_ceil(16)];
        let mut pixel_error = 0u64;
        let mut edge_error = 0u64;
        let mut total_weight = 0u64;
        // Use the FINAL denominator for partial global bounds. A partial mean
        // can fall later and must never be used to reject a candidate.
        let final_weight = if limits.is_some() {
            self.weights
                .iter()
                .map(|&w| u64::from(w))
                .sum::<u64>()
                .max(1)
        } else {
            1
        };
        // Only a single row of reconstructed colors is needed for vertical edges.
        let mut previous_row = vec![[0i32; 3]; w];
        for y in 0..h {
            let mut left = [0i32; 3];
            for (x, previous) in previous_row.iter_mut().enumerate() {
                let i = y * w + x;
                let c = self.composite(
                    color(candidate.get_pixel(x as u32, y as u32).0),
                    i,
                    background,
                );
                let reference = self.composite(self.colors[i], i, background);
                let weight = u64::from(self.weights[i]);
                let error = weight * distance(reference, c);
                pixel_error += error;
                total_weight += weight;
                let tile = &mut tiles[(y / 16) * tile_width + x / 16];
                tile.0 += error;
                tile.1 += weight;
                if x > 0 {
                    let delta = (reference[0]
                        - self.composite(self.colors[i - 1], i - 1, background)[0])
                        - (c[0] - left[0]);
                    edge_error += weight * (i64::from(delta) * i64::from(delta)) as u64;
                }
                if y > 0 {
                    let delta = (reference[0]
                        - self.composite(self.colors[i - w], i - w, background)[0])
                        - (c[0] - previous[0]);
                    edge_error += weight * (i64::from(delta) * i64::from(delta)) as u64;
                }
                *previous = c;
                left = c;
            }
            if let Some(limits) = limits {
                if pixel_error as f64 / final_weight as f64 > limits.pixel
                    || edge_error as f64 / final_weight as f64 > limits.edge
                {
                    return Ok(None);
                }
                // A tile ratio is final only after all its rows are processed,
                // including the short bottom tile of an odd-height image.
                if (y + 1) % 16 == 0 || y + 1 == h {
                    let start = (y / 16) * tile_width;
                    if tiles[start..start + tile_width]
                        .iter()
                        .any(|&(error, weight)| error as f64 / weight.max(1) as f64 > limits.tile)
                    {
                        return Ok(None);
                    }
                }
            }
        }
        let weighted_pixel_error = pixel_error as f64 / total_weight.max(1) as f64;
        let weighted_edge_error = edge_error as f64 / total_weight.max(1) as f64;
        let worst_tile_error = tiles
            .iter()
            .map(|&(error, count)| error as f64 / count.max(1) as f64)
            .fold(0.0, f64::max);
        let score = Score {
            weighted_pixel_error,
            weighted_edge_error,
            worst_tile_error,
            loss: weighted_pixel_error + 2.0 * weighted_edge_error + 0.1 * worst_tile_error,
        };
        Ok((limits.is_none_or(|l| l.accepts(&score))).then_some(score))
    }
}

// Fixed-point orthogonal cosine basis, scaled by 256; no platform-dependent cos().
const BASIS: [[i64; 8]; 8] = [
    [181, 181, 181, 181, 181, 181, 181, 181],
    [251, 213, 142, 50, -50, -142, -213, -251],
    [237, 98, -98, -237, -237, -98, 98, 237],
    [213, -50, -251, -142, 142, 251, 50, -213],
    [181, -181, -181, 181, 181, -181, -181, 181],
    [142, -251, 50, 213, -213, -50, 251, -142],
    [98, -237, 237, -98, -98, 237, -237, 98],
    [50, -142, 213, -251, 251, -213, 142, -50],
];

fn spectrum(colors: &[[i32; 3]], weights: &[u8], w: usize, h: usize) -> [u64; 64] {
    let mut result = [0u64; 64];
    let bw = w.div_ceil(8);
    let bh = h.div_ceil(8);
    let step = (bw * bh).div_ceil(1024).max(1);
    for block in (0..bw * bh).step_by(step) {
        let (bx, by) = (block % bw * 8, block / bw * 8);
        let mut horizontal = [[0i64; 8]; 8];
        let mut weight = 0u64;
        for (y, row) in horizontal.iter_mut().enumerate() {
            let indices: [usize; 8] =
                std::array::from_fn(|x| (by + y).min(h - 1) * w + (bx + x).min(w - 1));
            weight += indices.iter().map(|&i| u64::from(weights[i])).sum::<u64>();
            for (cell, basis) in row.iter_mut().zip(&BASIS) {
                *cell = indices
                    .iter()
                    .zip(basis)
                    .map(|(&i, &b)| i64::from(colors[i][0] - 128) * b)
                    .sum();
            }
        }
        for v in 0..8 {
            for u in 0..8 {
                let coefficient: i64 = (0..8).map(|y| horizontal[y][u] * BASIS[v][y]).sum();
                result[v * 8 + u] += (coefficient.unsigned_abs() / 65536) * weight;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analysis_is_exact_and_detects_fine_edge_damage() {
        let original = RgbImage::from_fn(32, 32, |x, _| {
            image::Rgb(if x == 15 { [0; 3] } else { [255; 3] })
        });
        let analysis = Analysis::new(&original).unwrap();
        assert_eq!(analysis.score(&original).unwrap().loss, 0.0);
        assert!(analysis.weights[15] > analysis.weights[0]);
        let damaged = RgbImage::from_pixel(32, 32, image::Rgb([255; 3]));
        assert!(analysis.score(&damaged).unwrap().weighted_edge_error > 0.0);
        assert!(analysis.score(&RgbImage::new(1, 1)).is_err());
    }
}
