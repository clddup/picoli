//! Separable luma quantizer proposal curves from the actual source FDCT.
//! Conditioned independent-AC rates and within-block DCT errors are proposal
//! models, never replacements for complete JPEG size/decoded RGB constraints.
use super::solver::{self, Point};
use crate::{
    Error, Reference,
    analysis::{Analysis, color, distance},
};
use image::RgbImage;
use jpeg_encoder::{AlignedBlock, CoefficientRateModel, fdct, rgb_to_ycbcr};
use std::collections::BTreeMap;
use zune_jpeg::QuantizedCoefficients;
const EIGEN: [i128; 8] = [0, 9977, 38390, 80913, 131072, 181231, 223754, 252167];
#[derive(Default)]
struct Entry {
    weight: i128,
    critical: i128,
    rate: [i64; 11],
}
pub struct Model {
    pub raw: Vec<[i16; 64]>,
    pub curves: Vec<Vec<Point>>,
    pub histogram_entries: usize,
    pub critical_tile: (usize, usize),
    pub base_pixel: i128,
    pub base_edge: i128,
    pub base_tile: i128,
}
/// Shared codec scalar FDCT and integer color converter; only the luma block
/// extraction is performed here, with the same last-pixel edge clamping.
pub fn raw_luma(reference: &Reference) -> Result<Vec<[i16; 64]>, Error> {
    let (width, height) = reference.pixels.dimensions();
    if width == 0 || height == 0 {
        return Err(Error::Argument("zero luma model dimensions"));
    }
    let cols = (width as usize).div_ceil(8);
    let rows = (height as usize).div_ceil(8);
    let mut result = Vec::new();
    result
        .try_reserve_exact(
            cols.checked_mul(rows)
                .ok_or(Error::Limit("FDCT block count overflow"))?,
        )
        .map_err(|_| Error::Limit("FDCT model allocation failed"))?;
    for by in 0..rows {
        for bx in 0..cols {
            let mut block = AlignedBlock {
                data: core::array::from_fn(|i| {
                    let x = (bx * 8 + i % 8).min(width as usize - 1);
                    let y = (by * 8 + i / 8).min(height as usize - 1);
                    let p = reference.pixels.get_pixel(x as u32, y as u32);
                    let (luma, _, _) = rgb_to_ycbcr(p[0], p[1], p[2]);
                    i16::from(luma) - 128
                }),
            };
            fdct(&mut block);
            result.push(block.data);
        }
    }
    Ok(result)
}
pub fn quantize(value: i16, q: u16) -> Option<i16> {
    if !(1..=255).contains(&q) {
        return None;
    }
    let step = 8 * i32::from(q);
    let magnitude = (i32::from(value).abs() + step / 2) / step;
    i16::try_from(if value < 0 { -magnitude } else { magnitude }).ok()
}
pub(super) fn critical(analysis: &Analysis, current: &RgbImage) -> (usize, usize) {
    let (width, height) = current.dimensions();
    let mut best = (0u64, 1u64, 0usize, 0usize);
    for top in (0..height).step_by(16) {
        for left in (0..width).step_by(16) {
            let (mut error, mut weight) = (0u64, 0u64);
            for y in top..(top + 16).min(height) {
                for x in left..(left + 16).min(width) {
                    let i = y as usize * width as usize + x as usize;
                    let w = u64::from(analysis.weights[i]);
                    error += w * distance(analysis.colors.at(i), color(current.get_pixel(x, y).0));
                    weight += w;
                }
            }
            if u128::from(error) * u128::from(best.1)
                > u128::from(best.0) * u128::from(weight.max(1))
            {
                best = (error, weight.max(1), left as usize / 16, top as usize / 16);
            }
        }
    }
    (best.2, best.3)
}
#[cfg(test)]
pub fn build(
    reference: &Reference,
    analysis: &Analysis,
    current: &RgbImage,
    frame: &QuantizedCoefficients,
    scans: u8,
) -> Result<Model, Error> {
    build_bounded(reference, analysis, current, frame, scans, usize::MAX)?
        .ok_or(Error::Limit("unbounded test histogram exhausted"))
}

/// None means optional histogram admission exhausted, never a partial model.
/// Arithmetic, curves and iteration order otherwise remain frozen from v80.
pub fn build_bounded(
    reference: &Reference,
    analysis: &Analysis,
    current: &RgbImage,
    frame: &QuantizedCoefficients,
    scans: u8,
    entry_allowance: usize,
) -> Result<Option<Model>, Error> {
    if reference.alpha.is_some()
        || current.dimensions() != reference.pixels.dimensions()
        || frame.components.is_empty()
    {
        return Err(Error::Argument("invalid quantizer model source"));
    }
    let raw = raw_luma(reference)?;
    let critical_tile = critical(analysis, current);
    let rate = CoefficientRateModel::new(scans)
        .ok_or(Error::Argument("quantizer model scans must be2/4"))?;
    let cols = (frame.width).div_ceil(8);
    let stride = frame.components[0].block_width;
    let mut hist: Vec<BTreeMap<u16, Entry>> = (0..64).map(|_| BTreeMap::new()).collect();
    let mut baseline = [(0i128, 0i128); 64];
    let mut remaining_entries = entry_allowance;
    for (index, source) in raw.iter().enumerate() {
        let (bx, by) = (index % cols, index / cols);
        let start = (by * stride + bx) * 64;
        let old: [i16; 64] = frame.components[0]
            .coefficients
            .get(start..start + 64)
            .ok_or(Error::Argument("model luma frame truncated"))?
            .try_into()
            .map_err(|_| Error::Argument("model block length"))?;
        let deltas = rate
            .category_replacement_deltas(&old, false)
            .ok_or(Error::Argument("invalid model AC categories"))?;
        let mut block_weight = 0i128;
        for y in by * 8..((by + 1) * 8).min(frame.height) {
            for x in bx * 8..((bx + 1) * 8).min(frame.width) {
                block_weight += i128::from(analysis.weights[y * frame.width + x]);
            }
        }
        let critical = critical_tile == (bx / 2, by / 2);
        for natural in 1..64 {
            let original_error = i128::from(source[natural])
                - 8 * i128::from(frame.components[0].quantization_table[natural])
                    * i128::from(old[natural]);
            let weighted = block_weight * original_error * original_error;
            baseline[natural].0 += weighted;
            if critical {
                baseline[natural].1 += weighted;
            }
            let key = source[natural].unsigned_abs();
            // Lookup allocates nothing. Charge BEFORE entry() can allocate a
            // root, split a node or grow the map; fixed map overhead is preflighted.
            if !hist[natural].contains_key(&key) {
                let Some(left) = remaining_entries.checked_sub(1) else {
                    return Ok(None);
                };
                remaining_entries = left;
            }
            let entry = hist[natural].entry(key).or_default();
            entry.weight += block_weight;
            if critical {
                entry.critical += block_weight;
            }
            for (total, &delta) in entry.rate.iter_mut().zip(&deltas[natural]) {
                *total += i64::from(delta);
            }
        }
    }
    let histogram_entries = hist.iter().map(BTreeMap::len).sum();
    let mut curves = Vec::new();
    let (mut base_pixel, mut base_edge, mut base_tile) = (0, 0, 0);
    for natural in 0..64 {
        let mut points = vec![Point::keep(frame.components[0].quantization_table[natural])];
        if natural != 0 {
            let eigen = EIGEN[natural / 8] + EIGEN[natural % 8];
            base_pixel += baseline[natural].0;
            base_edge += baseline[natural].0 * eigen;
            base_tile += baseline[natural].1;
            for quantizer in 1..=255u16 {
                let (mut pixel, mut tile, mut bits) = (0i128, 0i128, 0i64);
                let mut valid = true;
                for (&source, entry) in &hist[natural] {
                    let step = 8 * i32::from(quantizer);
                    let magnitude = (i32::from(source) + step / 2) / step;
                    let category = if magnitude == 0 {
                        0
                    } else {
                        32 - magnitude.leading_zeros()
                    } as usize;
                    if category > 10 {
                        valid = false;
                        break;
                    }
                    let error = i128::from(source) - i128::from(step) * i128::from(magnitude);
                    pixel += entry.weight * error * error;
                    tile += entry.critical * error * error;
                    bits += entry.rate[category];
                }
                if valid {
                    let pixel = pixel - baseline[natural].0;
                    points.push(Point {
                        quantizer,
                        retain: false,
                        pixel,
                        edge: pixel * eigen,
                        tile: tile - baseline[natural].1,
                        rate: bits,
                    });
                }
            }
        }
        curves.push(solver::pareto(&points));
    }
    Ok(Some(Model {
        raw,
        curves,
        histogram_entries,
        critical_tile,
        base_pixel,
        base_edge,
        base_tile,
    }))
}
/// Keep untouched frequencies exactly, including their current RDO decisions.
/// Chosen alternatives use the source FDCT and new literal quantizer directly.
pub fn apply(
    model: &Model,
    solution: &solver::Solution,
    frame: &mut QuantizedCoefficients,
) -> Result<usize, Error> {
    let cols = frame.width.div_ceil(8);
    let stride = frame.components[0].block_width;
    let mut changed = 0;
    for (natural, &choice) in solution.choices.iter().enumerate().skip(1) {
        let point = model.curves[natural][choice];
        if point.retain {
            continue;
        }
        frame.components[0].quantization_table[natural] = point.quantizer;
        changed += 1;
        for (index, source) in model.raw.iter().enumerate() {
            let first = (index / cols * stride + index % cols) * 64;
            frame.components[0].coefficients[first + natural] =
                quantize(source[natural], point.quantizer)
                    .ok_or(Error::Argument("invalid model quantizer"))?;
        }
    }
    Ok(changed)
}
#[cfg(test)]
mod tests;
