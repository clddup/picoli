//! Fuse the unchanged black/gray/white proxy scores into one pixel traversal.
//! All three backgrounds retain separate integer accumulators and hard bounds.
use super::{Analysis, Score, color};
use crate::{Error, quality::Limits};
use image::RgbImage;

impl Analysis {
    pub(super) fn score_transparent_fused(
        &self,
        candidate: &RgbImage,
        limits: Option<Limits>,
    ) -> Result<Option<Score>, Error> {
        if candidate.dimensions() != self.dimensions {
            return Err(Error::Argument("score dimensions differ from analysis"));
        }
        let Some(alpha) = &self.alpha else {
            return self.score_on_background(candidate, 0, limits);
        };
        let (w, h) = (self.dimensions.0 as usize, self.dimensions.1 as usize);
        let tile_width = w.div_ceil(16);
        let mut tiles = vec![([0u64; 3], 0u64); tile_width * h.div_ceil(16)];
        let mut pixel_error = [0u64; 3];
        let mut edge_error = [0u64; 3];
        let mut total_weight = 0u64;
        let final_weight = if limits.is_some() {
            self.weights
                .iter()
                .map(|&w| u64::from(w))
                .sum::<u64>()
                .max(1)
        } else {
            1
        };
        // Store reference-minus-candidate luma, not reconstructed RGB. Integer
        // (r-r_prev)-(c-c_prev) == (r-c)-(r_prev-c_prev), without overflow here.
        let mut previous_row = vec![[0i32; 3]; w];
        for y in 0..h {
            let mut left = [0i32; 3];
            for (x, previous) in previous_row.iter_mut().enumerate() {
                let i = y * w + x;
                let a = i32::from(alpha.as_raw()[i]);
                let reference = self.colors.at(i);
                let c = color(candidate.get_pixel(x as u32, y as u32).0);
                // Chroma composition is background-independent in the frozen
                // integer proxy. Preserve signed division toward zero exactly.
                let co = i64::from(reference[1] * a / 255 - c[1] * a / 255);
                let cg = i64::from(reference[2] * a / 255 - c[2] * a / 255);
                let chroma_error = (co * co + cg * cg) as u64;
                let ref_numerator = reference[0] * a + 127;
                let c_numerator = c[0] * a + 127;
                let black_ref = ref_numerator / 255;
                let black_c = c_numerator / 255;
                let gray_offset = 128 * (255 - a);
                // White adds the exact integer255-a to black on BOTH sides.
                // Still accumulate and bound all three scores, without three
                // RGB conversions, repeated chroma divisions or neighbor blends.
                let residual = [
                    black_ref - black_c,
                    (ref_numerator + gray_offset) / 255 - (c_numerator + gray_offset) / 255,
                    (black_ref + 255 - a) - (black_c + 255 - a),
                ];
                let weight = u64::from(self.weights[i]);
                total_weight += weight;
                let tile = &mut tiles[(y / 16) * tile_width + x / 16];
                tile.1 += weight;
                for (b, &delta) in residual.iter().enumerate() {
                    let d = i64::from(delta);
                    let error = weight * ((4 * d * d) as u64 + chroma_error);
                    pixel_error[b] += error;
                    tile.0[b] += error;
                    if x > 0 {
                        let edge = i64::from(delta - left[b]);
                        edge_error[b] += weight * (edge * edge) as u64;
                    }
                    if y > 0 {
                        let edge = i64::from(delta - previous[b]);
                        edge_error[b] += weight * (edge * edge) as u64;
                    }
                }
                *previous = residual;
                left = residual;
            }
            if let Some(limits) = limits {
                if pixel_error.iter().zip(edge_error).any(|(&p, e)| {
                    p as f64 / final_weight as f64 > limits.pixel
                        || e as f64 / final_weight as f64 > limits.edge
                }) {
                    return Ok(None);
                }
                if (y + 1) % 16 == 0 || y + 1 == h {
                    let start = (y / 16) * tile_width;
                    if tiles[start..start + tile_width]
                        .iter()
                        .any(|(errors, weight)| {
                            errors
                                .iter()
                                .any(|&e| e as f64 / (*weight).max(1) as f64 > limits.tile)
                        })
                    {
                        return Ok(None);
                    }
                }
            }
        }
        let mut score = Score {
            weighted_pixel_error: 0.0,
            weighted_edge_error: 0.0,
            worst_tile_error: 0.0,
            loss: 0.0,
        };
        for (b, (&p, &e)) in pixel_error.iter().zip(&edge_error).enumerate() {
            score.weighted_pixel_error = score
                .weighted_pixel_error
                .max(p as f64 / total_weight.max(1) as f64);
            score.weighted_edge_error = score
                .weighted_edge_error
                .max(e as f64 / total_weight.max(1) as f64);
            let worst = tiles
                .iter()
                .map(|(errors, weight)| errors[b] as f64 / (*weight).max(1) as f64)
                .fold(0.0, f64::max);
            score.worst_tile_error = score.worst_tile_error.max(worst);
        }
        score.loss = score.weighted_pixel_error
            + 2.0 * score.weighted_edge_error
            + 0.1 * score.worst_tile_error;
        Ok(limits.is_none_or(|l| l.accepts(&score)).then_some(score))
    }
}
