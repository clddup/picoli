//! Actual-current RGB error and spatial-weight proposal moments. The fixed IDCT
//! basis/interior clipping Jacobian ignore integer rounding, clipping crossings,
//! cross-frequency terms and block-edge gradients. Actual full RGB still decides.
pub(super) mod basis;
use super::{
    model::{self, Model},
    solver::{self, Point},
};
use crate::{
    Error, Reference,
    analysis::{Analysis, color, distance},
};
use basis::BASIS;
use image::RgbImage;
use jpeg_encoder::CoefficientRateModel;
use std::collections::BTreeMap;
use zune_jpeg::QuantizedCoefficients;
const Q: i128 = 256 * (1 << 14);
const EIGEN: [i128; 8] = [0, 9977, 38390, 80913, 131072, 181231, 223754, 252167];
#[derive(Default)]
struct Entry {
    weight: i128,
    pixel_a: i128,
    pixel_b: i128,
    tile_a: i128,
    tile_b: i128,
    rate: [i64; 11],
}
pub(super) fn factors(source: [i32; 3], current: [u8; 3], weight: u8) -> (i64, i64) {
    let m = current.map(|c| i64::from(c > 0 && c < 255));
    let jac = [
        77 * m[0] + 150 * m[1] + 29 * m[2],
        256 * (m[0] - m[2]),
        256 * m[1] - 128 * (m[0] + m[2]),
    ];
    let actual = color(current);
    let e = core::array::from_fn::<_, 3, _>(|i| i64::from(source[i] - actual[i]));
    let a = 4 * jac[0] * jac[0] + jac[1] * jac[1] + jac[2] * jac[2];
    let b = 4 * e[0] * jac[0] + e[1] * jac[1] + e[2] * jac[2];
    (i64::from(weight) * a, i64::from(weight) * b)
}
/// Keep the frozen research entry point and its assertions in source tests.
#[cfg(test)]
pub fn build(
    reference: &Reference,
    analysis: &Analysis,
    current: &RgbImage,
    frame: &QuantizedCoefficients,
    scans: u8,
    allowance: usize,
) -> Result<Model, Error> {
    build_bounded(reference, analysis, current, frame, scans, allowance)?
        .ok_or(Error::Limit("projected histogram admission exhausted"))
}

/// Dynamic estimated576B/entry allowance is checked before any new-key
/// allocation. Exhaustion returns no model, never partial curves or an error
/// that would abort otherwise valid full compression.
pub fn build_bounded(
    reference: &Reference,
    analysis: &Analysis,
    current: &RgbImage,
    frame: &QuantizedCoefficients,
    scans: u8,
    allowance: usize,
) -> Result<Option<Model>, Error> {
    if reference.alpha.is_some()
        || current.dimensions() != reference.pixels.dimensions()
        || frame.components.is_empty()
    {
        return Err(Error::Argument("invalid projected quantizer source"));
    }
    let raw = model::raw_luma(reference)?;
    let critical_tile = model::critical(analysis, current);
    let rate =
        CoefficientRateModel::new(scans).ok_or(Error::Argument("projection scans must be2/4"))?;
    let cols = frame.width.div_ceil(8);
    let stride = frame.components[0].block_width;
    let mut hist: Vec<BTreeMap<(i16, i32), Entry>> = (0..64).map(|_| BTreeMap::new()).collect();
    let mut baseline_edge = [0i128; 64];
    let mut count = 0usize;
    let (mut base_pixel, mut base_tile) = (0i128, 0i128);
    for (index, source) in raw.iter().enumerate() {
        let (bx, by) = (index % cols, index / cols);
        let start = (by * stride + bx) * 64;
        let old: [i16; 64] = frame.components[0]
            .coefficients
            .get(start..start + 64)
            .ok_or(Error::Argument("projection frame truncated"))?
            .try_into()
            .map_err(|_| Error::Argument("projection block length"))?;
        let deltas = rate
            .category_replacement_deltas(&old, false)
            .ok_or(Error::Argument("projection invalid AC categories"))?;
        let critical = critical_tile == (bx / 2, by / 2);
        let mut a = [0i64; 64];
        let mut b = [0i64; 64];
        let mut block_weight = 0i128;
        for t in 0..64 {
            let (x, y) = (bx * 8 + t % 8, by * 8 + t / 8);
            if x >= frame.width || y >= frame.height {
                continue;
            }
            let i = y * frame.width + x;
            let w = analysis.weights[i];
            let p = current.get_pixel(x as u32, y as u32).0;
            (a[t], b[t]) = factors(analysis.colors[i], p, w);
            let error = i128::from(w) * i128::from(distance(analysis.colors[i], color(p))) * Q * Q;
            base_pixel += error;
            if critical {
                base_tile += error;
            }
            block_weight += i128::from(w);
        }
        for natural in 1..64 {
            let old_dequant = i32::from(frame.components[0].quantization_table[natural])
                * i32::from(old[natural]);
            let source_error = i128::from(source[natural]) - 8 * i128::from(old_dequant);
            baseline_edge[natural] += block_weight * source_error * source_error;
            // The two local dot products fit i64 for 8-bit RGB/weights1..8;
            // scale linear moments only after summing, then use wide curve math.
            let (mut pa, mut pb) = (0i64, 0i64);
            for ((&basis, &a), &b) in BASIS[natural].iter().zip(&a).zip(&b) {
                let basis = i64::from(basis);
                pa += basis * basis * a;
                pb += basis * b;
            }
            let key = (source[natural], old_dequant);
            // Lookup is allocation-free; charge before entry() can allocate the
            // first/root node or split a partially filled node.
            if !hist[natural].contains_key(&key) {
                if count == allowance {
                    return Ok(None);
                }
                count += 1;
            }
            let entry = hist[natural].entry(key).or_default();
            entry.weight += block_weight;
            entry.pixel_a += i128::from(pa);
            entry.pixel_b -= 2 * Q * i128::from(pb);
            if critical {
                entry.tile_a += i128::from(pa);
                entry.tile_b -= 2 * Q * i128::from(pb);
            }
            for (total, &delta) in entry.rate.iter_mut().zip(&deltas[natural]) {
                *total += i64::from(delta);
            }
        }
    }
    let mut curves = Vec::new();
    let mut base_edge = 0i128;
    for natural in 0..64 {
        let mut points = vec![Point::keep(frame.components[0].quantization_table[natural])];
        if natural > 0 {
            let eigen = EIGEN[natural / 8] + EIGEN[natural % 8];
            base_edge += baseline_edge[natural] * eigen;
            for quantizer in 1..=255u16 {
                let (mut pixel, mut tile, mut edge, mut bits) = (0i128, 0i128, 0i128, 0i64);
                let mut valid = true;
                for (&(source, old), entry) in &hist[natural] {
                    let coefficient = model::quantize(source, quantizer)
                        .ok_or(Error::Argument("projection quantizer invalid"))?;
                    let category = if coefficient == 0 {
                        0
                    } else {
                        16 - coefficient.unsigned_abs().leading_zeros()
                    } as usize;
                    if category > 10 {
                        valid = false;
                        break;
                    }
                    let new = i128::from(quantizer) * i128::from(coefficient);
                    let delta = new - i128::from(old);
                    pixel += entry.pixel_a * delta * delta + entry.pixel_b * delta;
                    tile += entry.tile_a * delta * delta + entry.tile_b * delta;
                    let error = i128::from(source) - 8 * new;
                    edge += entry.weight * error * error;
                    bits += entry.rate[category];
                }
                if valid {
                    points.push(Point {
                        quantizer,
                        retain: false,
                        pixel,
                        edge: (edge - baseline_edge[natural]) * eigen,
                        tile,
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
        histogram_entries: count,
        critical_tile,
        base_pixel,
        base_edge,
        base_tile,
    }))
}
#[cfg(test)]
mod tests;
