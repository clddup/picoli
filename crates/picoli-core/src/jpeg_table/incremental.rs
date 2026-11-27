//! Exact, reversible decoded-RGB scoring for the opaque JPEG research route.
use crate::{
    Error,
    analysis::{Analysis, Score, color, distance, opaque::OpaqueContext},
    quality::Strength,
    resources,
};
use image::RgbImage;

pub struct State<'a> {
    analysis: &'a Analysis,
    pixels: RgbImage,
    tile_max: Vec<f64>,
    tile_base: usize,
    pixel_error: u64,
    edge_error: u64,
    total_weight: u64,
    tiles: Vec<(u64, u64)>,
}

impl<'a> State<'a> {
    pub fn new(analysis: &'a Analysis, current: &RgbImage) -> Result<Self, Error> {
        if current.dimensions() != analysis.dimensions {
            return Err(Error::Argument(
                "incremental score dimensions differ from analysis",
            ));
        }
        // Reuse the alpha-aware gate: positive weights alone do not prove opacity.
        if OpaqueContext::new(analysis, Strength::default().limits()).is_none() {
            return Err(Error::Argument(
                "incremental JPEG scoring requires opaque analysis",
            ));
        }
        let (width, height) = analysis.dimensions;
        let plan = resources::Plan::new(width, height, 0, analysis.memory_budget_bytes, 1)?;
        let count = (width as usize)
            .checked_mul(height as usize)
            .ok_or(Error::Limit("incremental pixel count overflow"))?;
        let rgb_bytes = allocation_bytes(count, 3)?;
        let tile_count = (width as usize)
            .div_ceil(16)
            .checked_mul((height as usize).div_ceil(16))
            .ok_or(Error::Limit("incremental tile count overflow"))?;
        let tile_bytes = allocation_bytes(tile_count, std::mem::size_of::<(u64, u64)>())?;
        let tile_base = tile_count
            .checked_next_power_of_two()
            .ok_or(Error::Limit("tile max tree overflow"))?;
        let tree_count = tile_base
            .checked_mul(2)
            .ok_or(Error::Limit("tile max tree size overflow"))?;
        let tree_bytes = allocation_bytes(tree_count, std::mem::size_of::<f64>())?;
        // Admission estimate, not an allocator ceiling. Add owned RGB, maintained tile-max tree,
        // tile totals and a worst-case full-image rollback buffer to the common
        // decode/analysis/search estimate, before allocating any state.
        let estimate = plan
            .estimated_shared_bytes
            .checked_add(plan.estimated_worker_bytes)
            .and_then(|n| n.checked_add(rgb_bytes as u64))
            .and_then(|n| n.checked_add(tree_bytes as u64))
            .and_then(|n| n.checked_add(tile_bytes as u64))
            .and_then(|n| n.checked_add(rgb_bytes as u64))
            .ok_or(Error::Limit("incremental working-set estimate overflow"))?;
        if estimate > analysis.memory_budget_bytes {
            return Err(Error::Limit(
                "incremental scoring exceeds host memory budget",
            ));
        }
        if analysis.colors.len() != count || analysis.weights.len() != count {
            return Err(Error::Argument(
                "incremental analysis buffers differ from dimensions",
            ));
        }
        let total_weight = analysis
            .weights
            .iter()
            .try_fold(0u64, |sum, &weight| sum.checked_add(u64::from(weight)))
            .ok_or(Error::Limit("incremental weight total overflow"))?;
        // RGB-derived colors have |Y delta| <= 255 and |Co/Cg delta|
        // <= 510. This also bounds both edge directions (2 * 510^2).
        total_weight
            .checked_mul(4 * 255 * 255 + 2 * 510 * 510)
            .ok_or(Error::Limit("incremental error totals overflow"))?;
        let raw = current
            .as_raw()
            .get(..rgb_bytes)
            .ok_or(Error::Argument("incremental RGB buffer is truncated"))?;
        let mut owned = reserved(rgb_bytes)?;
        owned.extend_from_slice(raw);
        let pixels = RgbImage::from_raw(width, height, owned)
            .ok_or(Error::Argument("invalid incremental RGB dimensions"))?;
        let mut tile_max = reserved(tree_count)?;
        tile_max.resize(tree_count, 0.0);
        let mut tiles = reserved(tile_count)?;
        tiles.resize(tile_count, (0, 0));
        for (i, &weight) in analysis.weights.iter().enumerate() {
            tiles[(i / width as usize / 16) * (width as usize).div_ceil(16)
                + (i % width as usize / 16)]
                .1 += u64::from(weight);
        }
        let mut state = Self {
            analysis,
            pixels,
            tile_max,
            tile_base,
            pixel_error: 0,
            edge_error: 0,
            total_weight,
            tiles,
        };
        state.accumulate((0, 0, width as usize, height as usize), true);
        for (i, &(error, weight)) in state.tiles.iter().enumerate() {
            state.tile_max[tile_base + i] = error as f64 / weight.max(1) as f64;
        }
        for i in (1..tile_base).rev() {
            state.tile_max[i] = state.tile_max[2 * i].max(state.tile_max[2 * i + 1]);
        }
        Ok(state)
    }

    pub fn pixels(&self) -> &RgbImage {
        &self.pixels
    }

    pub fn score(&self) -> Score {
        let weighted_pixel_error = self.pixel_error as f64 / self.total_weight.max(1) as f64;
        let weighted_edge_error = self.edge_error as f64 / self.total_weight.max(1) as f64;
        let worst_tile_error = self.tile_max[1];
        Score {
            weighted_pixel_error,
            weighted_edge_error,
            worst_tile_error,
            loss: weighted_pixel_error + 2.0 * weighted_edge_error + 0.1 * worst_tile_error,
        }
    }

    /// Commit only if all three exact components fit both the supplied bounds
    /// and the frozen strength-50 limits. `loss` is not an admission criterion.
    #[cfg(test)]
    pub fn try_patch(
        &mut self,
        rect: (usize, usize, usize, usize),
        bytes: &[u8],
        bounds: &Score,
    ) -> Result<bool, Error> {
        self.try_patch_at_strength(rect, bytes, bounds, Strength::default())
    }

    /// The caller's unchanged requested strength, in addition to original bounds.
    pub fn try_patch_at_strength(
        &mut self,
        rect: (usize, usize, usize, usize),
        bytes: &[u8],
        bounds: &Score,
        strength: Strength,
    ) -> Result<bool, Error> {
        let (left, top, width, height) = rect;
        let right = left.checked_add(width);
        let bottom = top.checked_add(height);
        let length = width.checked_mul(height).and_then(|n| n.checked_mul(3));
        if width == 0
            || height == 0
            || right.is_none_or(|x| x > self.pixels.width() as usize)
            || bottom.is_none_or(|y| y > self.pixels.height() as usize)
            || length != Some(bytes.len())
        {
            return Err(Error::Argument(
                "invalid incremental RGB patch rectangle or length",
            ));
        }
        // Fast non-mutating single-pass path for small chroma/luma patches (up to 18x18).
        // Avoids heap allocations, avoids mutating `pixels`/`tiles`/`tile_max` on
        // rejected trials, and skips unchanged pixels and edges.
        let limits = strength.limits();
        let max_tile_bound = bounds.worst_tile_error.min(limits.tile);
        if width <= 18 && height <= 18 && self.tile_max[1] <= max_tile_bound {
            let stride = self.pixels.width() as usize;
            let img_height = self.pixels.height() as usize;
            let tile_width = stride.div_ceil(16);
            let raw_pixels = self.pixels.as_raw();

            let mut old_y = [0i32; 324];
            let mut new_y = [0i32; 324];
            let mut old_pixel_sum = 0u64;
            let mut new_pixel_sum = 0u64;
            // An <=18x18 patch touches at most 2x2 = 4 tiles (or up to 3x3 = 9 if straddling).
            let mut tile_ids = [usize::MAX; 9];
            let mut tile_old = [0u64; 9];
            let mut tile_new = [0u64; 9];
            let mut tile_count = 0usize;

            for row in 0..height {
                let y = top + row;
                let ty = (y / 16) * tile_width;
                let img_row = y * stride + left;
                let patch_row = row * width;
                for col in 0..width {
                    let x = left + col;
                    let i = img_row + col;
                    let p_idx = patch_row + col;
                    let old_rgb = [
                        raw_pixels[i * 3],
                        raw_pixels[i * 3 + 1],
                        raw_pixels[i * 3 + 2],
                    ];
                    let new_rgb = [bytes[p_idx * 3], bytes[p_idx * 3 + 1], bytes[p_idx * 3 + 2]];
                    if old_rgb == new_rgb {
                        let yv = color(old_rgb)[0];
                        old_y[p_idx] = yv;
                        new_y[p_idx] = yv;
                        continue;
                    }
                    let oc = color(old_rgb);
                    let nc = color(new_rgb);
                    old_y[p_idx] = oc[0];
                    new_y[p_idx] = nc[0];
                    let w = u64::from(self.analysis.weights[i]);
                    let src = self.analysis.colors.at(i);
                    let oe = w * distance(src, oc);
                    let ne = w * distance(src, nc);
                    old_pixel_sum += oe;
                    new_pixel_sum += ne;
                    if oe != ne {
                        let leaf = ty + x / 16;
                        let mut slot = tile_count;
                        for (k, &tid) in tile_ids.iter().enumerate().take(tile_count) {
                            if tid == leaf {
                                slot = k;
                                break;
                            }
                        }
                        if slot == tile_count {
                            tile_ids[slot] = leaf;
                            tile_count += 1;
                        }
                        tile_old[slot] += oe;
                        tile_new[slot] += ne;
                    }
                }
            }

            let candidate_pixel_error = self.pixel_error - old_pixel_sum + new_pixel_sum;
            let weighted_pixel_error =
                candidate_pixel_error as f64 / self.total_weight.max(1) as f64;
            if weighted_pixel_error > bounds.weighted_pixel_error
                || weighted_pixel_error > limits.pixel
            {
                return Ok(false);
            }

            for k in 0..tile_count {
                let leaf = tile_ids[k];
                let (cur_err, weight) = self.tiles[leaf];
                let new_err = cur_err - tile_old[k] + tile_new[k];
                let tile_f64 = new_err as f64 / weight.max(1) as f64;
                if tile_f64 > max_tile_bound {
                    return Ok(false);
                }
            }

            // Compute exact horizontal and vertical edge deltas without mutating pixels.
            let mut old_edge_sum = 0u64;
            let mut new_edge_sum = 0u64;
            let right_edge_end = (left + width + 1).min(stride);
            for row in 0..height {
                let y = top + row;
                let row_off = y * stride;
                let patch_row = row * width;
                for x in left.max(1)..right_edge_end {
                    let i = row_off + x;
                    let (oy_i, ny_i) = if x < left + width {
                        let p = patch_row + (x - left);
                        (old_y[p], new_y[p])
                    } else {
                        let yv = self.current_color(i)[0];
                        (yv, yv)
                    };
                    let (oy_p, ny_p) = if x > left {
                        let p = patch_row + (x - 1 - left);
                        (old_y[p], new_y[p])
                    } else {
                        let yv = self.current_color(i - 1)[0];
                        (yv, yv)
                    };
                    if oy_i == ny_i && oy_p == ny_p {
                        continue;
                    }
                    let src_diff =
                        self.analysis.colors.at(i)[0] - self.analysis.colors.at(i - 1)[0];
                    let od = src_diff - (oy_i - oy_p);
                    let nd = src_diff - (ny_i - ny_p);
                    let w = u64::from(self.analysis.weights[i]);
                    old_edge_sum += w * (i64::from(od) * i64::from(od)) as u64;
                    new_edge_sum += w * (i64::from(nd) * i64::from(nd)) as u64;
                }
            }
            let bottom_edge_end = (top + height + 1).min(img_height);
            for y in top.max(1)..bottom_edge_end {
                let row_off = y * stride;
                for col in 0..width {
                    let x = left + col;
                    let i = row_off + x;
                    let (oy_i, ny_i) = if y < top + height {
                        let p = (y - top) * width + col;
                        (old_y[p], new_y[p])
                    } else {
                        let yv = self.current_color(i)[0];
                        (yv, yv)
                    };
                    let (oy_p, ny_p) = if y > top {
                        let p = (y - 1 - top) * width + col;
                        (old_y[p], new_y[p])
                    } else {
                        let yv = self.current_color(i - stride)[0];
                        (yv, yv)
                    };
                    if oy_i == ny_i && oy_p == ny_p {
                        continue;
                    }
                    let src_diff =
                        self.analysis.colors.at(i)[0] - self.analysis.colors.at(i - stride)[0];
                    let od = src_diff - (oy_i - oy_p);
                    let nd = src_diff - (ny_i - ny_p);
                    let w = u64::from(self.analysis.weights[i]);
                    old_edge_sum += w * (i64::from(od) * i64::from(od)) as u64;
                    new_edge_sum += w * (i64::from(nd) * i64::from(nd)) as u64;
                }
            }

            let candidate_edge_error = self.edge_error - old_edge_sum + new_edge_sum;
            let weighted_edge_error = candidate_edge_error as f64 / self.total_weight.max(1) as f64;
            if weighted_edge_error > bounds.weighted_edge_error || weighted_edge_error > limits.edge
            {
                return Ok(false);
            }

            // Accepted! Commit once directly to pixels, error totals, tiles and segment tree.
            self.pixel_error = candidate_pixel_error;
            self.edge_error = candidate_edge_error;
            for row in 0..height {
                let first = (top + row) * stride + left;
                let patch_row = &bytes[row * width * 3..(row + 1) * width * 3];
                self.pixels.as_mut()[first * 3..(first + width) * 3].copy_from_slice(patch_row);
            }
            for k in 0..tile_count {
                let leaf = tile_ids[k];
                let (cur_err, weight) = &mut self.tiles[leaf];
                *cur_err = *cur_err - tile_old[k] + tile_new[k];
                let mut idx = self.tile_base + leaf;
                self.tile_max[idx] = *cur_err as f64 / (*weight).max(1) as f64;
                while idx > 1 {
                    idx /= 2;
                    self.tile_max[idx] = self.tile_max[2 * idx].max(self.tile_max[2 * idx + 1]);
                }
            }
            return Ok(true);
        }

        // All fallible work precedes mutation. No state-sized scratch copy is
        // needed: only the tightly packed old patch is retained for rollback.
        let mut small = [0u8; 972];
        let mut large = if bytes.len() > small.len() {
            reserved(bytes.len())?
        } else {
            Vec::new()
        };
        let old: &mut [u8] = if bytes.len() <= small.len() {
            &mut small[..bytes.len()]
        } else {
            large.resize(bytes.len(), 0);
            &mut large
        };
        let stride = self.pixels.width() as usize * 3;
        for (row, y) in (top..top + height).enumerate() {
            let start = y * stride + left * 3;
            old[row * width * 3..(row + 1) * width * 3]
                .copy_from_slice(&self.pixels.as_raw()[start..start + width * 3]);
        }
        self.replace(rect, bytes);
        let score = self.score();
        let accepted = score.weighted_pixel_error <= bounds.weighted_pixel_error
            && score.weighted_edge_error <= bounds.weighted_edge_error
            && score.worst_tile_error <= bounds.worst_tile_error
            && strength.limits().accepts(&score);
        if !accepted {
            self.replace(rect, old);
        }
        Ok(accepted)
    }

    fn replace(&mut self, rect: (usize, usize, usize, usize), bytes: &[u8]) {
        self.accumulate(rect, false);
        let (left, top, width, height) = rect;
        let stride = self.pixels.width() as usize;
        for row in 0..height {
            let first = (top + row) * stride + left;
            let patch_row = &bytes[row * width * 3..(row + 1) * width * 3];
            self.pixels.as_mut()[first * 3..(first + width) * 3].copy_from_slice(patch_row);
        }
        self.accumulate(rect, true);
        let tile_width = stride.div_ceil(16);
        for y in top / 16..(top + height).div_ceil(16) {
            for x in left / 16..(left + width).div_ceil(16) {
                let leaf = y * tile_width + x;
                let (error, weight) = self.tiles[leaf];
                let mut i = self.tile_base + leaf;
                self.tile_max[i] = error as f64 / weight.max(1) as f64;
                while i > 1 {
                    i /= 2;
                    self.tile_max[i] = self.tile_max[2 * i].max(self.tile_max[2 * i + 1]);
                }
            }
        }
    }

    fn accumulate(&mut self, rect: (usize, usize, usize, usize), add: bool) {
        let (left, top, width, height) = rect;
        let (right, bottom) = (left + width, top + height);
        let stride = self.pixels.width() as usize;
        let tile_width = stride.div_ceil(16);
        for y in top..bottom {
            for x in left..right {
                let i = y * stride + x;
                let error = u64::from(self.analysis.weights[i])
                    * distance(self.analysis.colors.at(i), self.current_color(i));
                adjust(&mut self.pixel_error, error, add);
                adjust(&mut self.tiles[y / 16 * tile_width + x / 16].0, error, add);
            }
        }
        // Edges are owned/weighted by their right or bottom endpoint, which
        // can lie one pixel outside the patch. Handle each direction once.
        let mut edges = 0;
        for y in top..bottom {
            for x in left.max(1)..right.saturating_add(1).min(stride) {
                let i = y * stride + x;
                edges += self.edge(i, i - 1);
            }
        }
        for y in top.max(1)..bottom.saturating_add(1).min(self.pixels.height() as usize) {
            for x in left..right {
                let i = y * stride + x;
                edges += self.edge(i, i - stride);
            }
        }
        adjust(&mut self.edge_error, edges, add);
    }

    fn current_color(&self, i: usize) -> [i32; 3] {
        let p = &self.pixels.as_raw()[i * 3..i * 3 + 3];
        color([p[0], p[1], p[2]])
    }

    #[cfg(test)]
    fn current_colors(&self) -> Vec<[i32; 3]> {
        (0..self.analysis.weights.len())
            .map(|i| self.current_color(i))
            .collect()
    }

    fn edge(&self, i: usize, previous: usize) -> u64 {
        let delta = (self.analysis.colors.at(i)[0] - self.analysis.colors.at(previous)[0])
            - (self.current_color(i)[0] - self.current_color(previous)[0]);
        u64::from(self.analysis.weights[i]) * (i64::from(delta) * i64::from(delta)) as u64
    }
}

fn adjust(total: &mut u64, value: u64, add: bool) {
    if add {
        *total += value;
    } else {
        *total -= value;
    }
}

fn allocation_bytes(count: usize, size: usize) -> Result<usize, Error> {
    count
        .checked_mul(size)
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or(Error::Limit("incremental allocation size overflow"))
}

fn reserved<T>(count: usize) -> Result<Vec<T>, Error> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| Error::Limit("incremental scoring allocation failed"))?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn same(actual: &Score, expected: &Score) {
        assert_eq!(
            actual.weighted_pixel_error.to_bits(),
            expected.weighted_pixel_error.to_bits()
        );
        assert_eq!(
            actual.weighted_edge_error.to_bits(),
            expected.weighted_edge_error.to_bits()
        );
        assert_eq!(
            actual.worst_tile_error.to_bits(),
            expected.worst_tile_error.to_bits()
        );
        assert_eq!(actual.loss.to_bits(), expected.loss.to_bits());
    }

    fn unbounded() -> Score {
        Score {
            weighted_pixel_error: f64::INFINITY,
            weighted_edge_error: f64::INFINITY,
            worst_tile_error: f64::INFINITY,
            loss: f64::NEG_INFINITY,
        }
    }

    fn next(seed: &mut u64) -> u32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (*seed >> 32) as u32
    }

    fn patched(image: &RgbImage, rect: (usize, usize, usize, usize), bytes: &[u8]) -> RgbImage {
        let mut result = image.clone();
        let (left, top, width, height) = rect;
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * 3;
                result.put_pixel(
                    (left + x) as u32,
                    (top + y) as u32,
                    Rgb([bytes[i], bytes[i + 1], bytes[i + 2]]),
                );
            }
        }
        result
    }

    fn assert_unchanged(
        state: &State<'_>,
        before: &RgbImage,
        colors: &[[i32; 3]],
        totals: (u64, u64, u64),
        tiles: &[(u64, u64)],
        max_tree: &[f64],
        score: &Score,
    ) {
        assert_eq!(state.pixels(), before);
        assert_eq!(state.current_colors(), colors);
        assert_eq!(state.tile_max, max_tree);
        assert_eq!(
            (state.pixel_error, state.edge_error, state.total_weight),
            totals
        );
        assert_eq!(state.tiles, tiles);
        same(&state.score(), score);
    }

    #[test]
    fn deterministic_patch_sequences_match_full_scores_bitwise() {
        let mut seed = 67;
        for (width, height) in [
            (1, 1),
            (1, 33),
            (35, 1),
            (15, 17),
            (16, 16),
            (17, 19),
            (37, 33),
        ] {
            let source = RgbImage::from_fn(width, height, |_, _| {
                Rgb(std::array::from_fn(|_| next(&mut seed) as u8))
            });
            let analysis = Analysis::new(&source).unwrap();
            let initial = RgbImage::from_fn(width, height, |x, y| {
                Rgb(source.get_pixel(x, y).0.map(|v| v.saturating_add(1)))
            });
            let mut state = State::new(&analysis, &initial).unwrap();
            same(&state.score(), &analysis.score(&initial).unwrap());
            let mut accepted_count = 0;
            let mut rejected_count = 0;
            for step in 0..128 {
                let left = next(&mut seed) as usize % width as usize;
                let top = next(&mut seed) as usize % height as usize;
                let patch_width = 1 + next(&mut seed) as usize % (width as usize - left);
                let patch_height = 1 + next(&mut seed) as usize % (height as usize - top);
                let rect = (left, top, patch_width, patch_height);
                let mut bytes = Vec::new();
                for y in top..top + patch_height {
                    for x in left..left + patch_width {
                        for value in source.get_pixel(x as u32, y as u32).0 {
                            bytes.push(if step % 4 == 0 {
                                next(&mut seed) as u8
                            } else {
                                value.saturating_add((next(&mut seed) % 3) as u8)
                            });
                        }
                    }
                }
                let before = state.pixels().clone();
                let colors = state.current_colors();
                let max_tree = state.tile_max.clone();
                let totals = (state.pixel_error, state.edge_error, state.total_weight);
                let tiles = state.tiles.clone();
                let old_score = state.score();
                let candidate = patched(&before, rect, &bytes);
                let full = analysis.score(&candidate).unwrap();
                let bounds = if step % 3 == 0 {
                    old_score.clone()
                } else {
                    unbounded()
                };
                let expected = full.weighted_pixel_error <= bounds.weighted_pixel_error
                    && full.weighted_edge_error <= bounds.weighted_edge_error
                    && full.worst_tile_error <= bounds.worst_tile_error
                    && Strength::default().limits().accepts(&full);
                assert_eq!(state.try_patch(rect, &bytes, &bounds).unwrap(), expected);
                if expected {
                    accepted_count += 1;
                    assert_eq!(state.pixels(), &candidate);
                    same(&state.score(), &full);
                } else {
                    rejected_count += 1;
                    assert_unchanged(
                        &state, &before, &colors, totals, &tiles, &max_tree, &old_score,
                    );
                }
                same(&state.score(), &analysis.score(state.pixels()).unwrap());
            }
            assert!(accepted_count > 0);
            assert!(rejected_count > 0);
        }
    }

    #[test]
    fn odd_thin_and_tile_boundary_edges_use_current_endpoint_weights() {
        for (width, height) in [(1, 33), (33, 1), (33, 35)] {
            let source = RgbImage::from_fn(width, height, |x, y| Rgb([(x * 37 + y * 53) as u8; 3]));
            let analysis = Analysis::new(&source).unwrap();
            let mut state = State::new(&analysis, &source).unwrap();
            for y in [0, 15.min(height - 1), 16.min(height - 1), height - 1] {
                for x in [0, 15.min(width - 1), 16.min(width - 1), width - 1] {
                    let old = state.pixels().get_pixel(x, y).0;
                    let bytes = old.map(|v| v.saturating_add(1));
                    let candidate = patched(state.pixels(), (x as usize, y as usize, 1, 1), &bytes);
                    assert!(
                        state
                            .try_patch((x as usize, y as usize, 1, 1), &bytes, &unbounded())
                            .unwrap()
                    );
                    same(&state.score(), &analysis.score(&candidate).unwrap());
                }
            }
        }
    }

    #[test]
    fn component_bounds_are_inclusive_and_loss_is_not_a_gate() {
        let source = RgbImage::new(17, 19);
        let analysis = Analysis::new(&source).unwrap();
        let bytes = [1; 3];
        let candidate = patched(&source, (15, 15, 1, 1), &bytes);
        let full = analysis.score(&candidate).unwrap();
        assert!(Strength::default().limits().accepts(&full));
        for component in 0..3 {
            let mut state = State::new(&analysis, &source).unwrap();
            let mut bounds = full.clone();
            let value = match component {
                0 => &mut bounds.weighted_pixel_error,
                1 => &mut bounds.weighted_edge_error,
                _ => &mut bounds.worst_tile_error,
            };
            assert!(*value > 0.0);
            *value = f64::from_bits(value.to_bits() - 1);
            assert!(!state.try_patch((15, 15, 1, 1), &bytes, &bounds).unwrap());
            assert_eq!(state.pixels(), &source);
            same(&state.score(), &analysis.score(&source).unwrap());
            let mut bounds = full.clone();
            bounds.loss = f64::NEG_INFINITY;
            assert!(state.try_patch((15, 15, 1, 1), &bytes, &bounds).unwrap());
            same(&state.score(), &full);
        }
    }

    #[test]
    fn each_frozen_strength_50_limit_still_applies_with_infinite_bounds() {
        let frozen = Strength::default().limits();
        assert_eq!(
            (frozen.pixel, frozen.edge, frozen.tile),
            (64.0, 16.0, 512.0)
        );
        // Respectively isolate pixel, edge, and tile rejection.
        for (width, height, damage) in [(32, 32, 0), (32, 32, 1), (64, 64, 2)] {
            let source = RgbImage::new(width, height);
            let analysis = Analysis::new(&source).unwrap();
            let candidate = RgbImage::from_fn(width, height, |x, y| {
                Rgb([match damage {
                    0 => 5,
                    1 => {
                        if (x + y) % 2 == 0 {
                            4
                        } else {
                            0
                        }
                    }
                    _ => {
                        if x < 16 && y < 16 {
                            12
                        } else {
                            0
                        }
                    }
                }; 3])
            });
            let score = analysis.score(&candidate).unwrap();
            let exceeds = [
                score.weighted_pixel_error > frozen.pixel,
                score.weighted_edge_error > frozen.edge,
                score.worst_tile_error > frozen.tile,
            ];
            assert_eq!(exceeds, [damage == 0, damage == 1, damage == 2]);
            let mut state = State::new(&analysis, &source).unwrap();
            assert!(
                !state
                    .try_patch(
                        (0, 0, width as usize, height as usize),
                        candidate.as_raw(),
                        &unbounded()
                    )
                    .unwrap()
            );
            assert_eq!(state.pixels(), &source);
            same(&state.score(), &analysis.score(&source).unwrap());
        }
    }

    #[test]
    fn invalid_patches_leave_every_state_component_unchanged() {
        let source = RgbImage::from_pixel(17, 19, Rgb([50, 100, 150]));
        let analysis = Analysis::new(&source).unwrap();
        let mut state = State::new(&analysis, &source).unwrap();
        let colors = state.current_colors();
        let max_tree = state.tile_max.clone();
        let totals = (state.pixel_error, state.edge_error, state.total_weight);
        let tiles = state.tiles.clone();
        let score = state.score();
        for (rect, bytes) in [
            ((0, 0, 0, 1), &[][..]),
            ((0, 0, 1, 0), &[][..]),
            ((17, 0, 1, 1), &[0; 3][..]),
            ((0, 19, 1, 1), &[0; 3][..]),
            ((16, 18, 2, 2), &[0; 12][..]),
            ((usize::MAX, 0, 1, 1), &[0; 3][..]),
            ((0, usize::MAX, 1, 1), &[0; 3][..]),
            ((0, 0, usize::MAX, usize::MAX), &[][..]),
            ((0, 0, 1, 1), &[0; 2][..]),
            ((0, 0, 1, 1), &[0; 4][..]),
        ] {
            assert!(state.try_patch(rect, bytes, &unbounded()).is_err());
            assert_unchanged(&state, &source, &colors, totals, &tiles, &max_tree, &score);
        }
    }

    #[test]
    fn constructor_rejects_alpha_dimensions_and_insufficient_or_overflowing_admission() {
        let source = RgbImage::new(17, 19);
        let mut analysis = Analysis::new(&source).unwrap();
        assert!(State::new(&analysis, &RgbImage::new(1, 1)).is_err());
        analysis.memory_budget_bytes = 1;
        assert!(State::new(&analysis, &source).is_err());
        let plan = resources::Plan::new(17, 19, 0, u64::MAX, 1).unwrap();
        analysis.memory_budget_bytes = plan.estimated_shared_bytes + plan.estimated_worker_bytes;
        assert!(State::new(&analysis, &source).is_err());
        assert!(allocation_bytes(usize::MAX, 12).is_err());
        assert!(allocation_bytes(isize::MAX as usize, 2).is_err());
        for alpha in [0, 128, 255] {
            let reference = crate::Reference {
                pixels: source.clone(),
                alpha: Some(image::GrayImage::from_pixel(17, 19, image::Luma([alpha]))),
                source_format: "png",
                jpeg_metadata: Default::default(),
            };
            let analysis =
                Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
            assert!(State::new(&analysis, &source).is_err());
        }
        let zero = RgbImage::new(0, 1);
        let mut analysis = Analysis::new(&source).unwrap();
        analysis.dimensions = zero.dimensions();
        assert!(State::new(&analysis, &zero).is_err());
    }
}
