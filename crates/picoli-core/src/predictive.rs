//! Near-lossless predictive RGB quantization without a 256-color ceiling.
//!
//! Quantize the residual against reconstructed neighbors, rather than force
//! every image into a global palette. Alpha and invisible RGB remain exact.
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    optimize::{Candidate, Parameters},
    palette,
};
use image::RgbImage;

pub(crate) const STEPS: [u8; 7] = [2, 3, 4, 6, 8, 12, 16];

pub(crate) fn steps(reference: &Reference) -> &'static [u8] {
    let mut colors = std::collections::BTreeSet::new();
    for p in reference.pixels.pixels() {
        colors.insert(p.0);
        if colors.len() > 256 {
            return &STEPS;
        }
    }
    // The palette branch already handles discrete-color inputs. Do not invent
    // new RGB values along otherwise exact antialiased alpha edges here.
    &[1]
}

fn paeth(left: u8, up: u8, upper_left: u8) -> i16 {
    let (a, b, c) = (i16::from(left), i16::from(up), i16::from(upper_left));
    let p = a + b - c;
    let (da, db, dc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
    if da <= db && da <= dc {
        a
    } else if db <= dc {
        b
    } else {
        c
    }
}

fn map(reference: &Reference, step: u8) -> Result<RgbImage, Error> {
    if !(1..=16).contains(&step) {
        return Err(Error::Argument("predictive step must be 1..=16"));
    }
    if reference
        .alpha
        .as_ref()
        .is_some_and(|a| a.dimensions() != reference.pixels.dimensions())
    {
        return Err(Error::Argument("alpha dimensions differ from RGB"));
    }
    let source = &reference.pixels;
    let width = source.width() as usize;
    let height = source.height() as usize;
    let stride = width * 3;
    let src_raw = source.as_raw();
    let alpha_raw = reference.alpha.as_ref().map(|a| a.as_raw().as_slice());
    let mut dst_raw = vec![0u8; src_raw.len()];
    let step = i16::from(step);
    for y in 0..height {
        let row_off = y * stride;
        let prev_off = row_off.saturating_sub(stride);
        for x in 0..width {
            let p_idx = y * width + x;
            let px = row_off + x * 3;
            if alpha_raw.is_some_and(|a| a[p_idx] == 0) {
                dst_raw[px..px + 3].copy_from_slice(&src_raw[px..px + 3]);
                continue;
            }
            for c in 0..3 {
                let left = if x > 0 { dst_raw[px - 3 + c] } else { 0 };
                let up = if y > 0 {
                    dst_raw[prev_off + x * 3 + c]
                } else {
                    0
                };
                let upper_left = if x > 0 && y > 0 {
                    dst_raw[prev_off + (x - 1) * 3 + c]
                } else {
                    0
                };
                let predicted = paeth(left, up, upper_left);
                let residual = i16::from(src_raw[px + c]) - predicted;
                let quantized = residual.signum() * ((residual.abs() + step / 2) / step) * step;
                dst_raw[px + c] = (predicted + quantized).clamp(0, 255) as u8;
            }
        }
    }
    RgbImage::from_raw(source.width(), source.height(), dst_raw)
        .ok_or(Error::Encoding("predictive RGB buffer mismatch".into()))
}

pub(crate) fn candidate(
    reference: &Reference,
    analysis: &Analysis,
    step: u8,
    accepts: impl FnOnce(&Score) -> bool,
) -> Result<Option<Candidate>, Error> {
    let mapped = map(reference, step)?;
    let score = analysis.score(&mapped)?;
    if !accepts(&score) {
        return Ok(None);
    }
    let (data, filter) = palette::encode_truecolor(reference, &mapped)?;
    Ok(Some(Candidate {
        id: format!("png-predictive-step-{step}"),
        parameters: Parameters::PngPredictive { step, filter },
        bytes: data.len(),
        data,
        score,
    }))
}

pub(crate) fn search_bounded(
    reference: &Reference,
    analysis: &Analysis,
    limits: crate::quality::Limits,
) -> Result<Option<Candidate>, Error> {
    let steps = steps(reference);
    let mut top: Vec<(usize, u8, RgbImage, Score)> = Vec::with_capacity(3);
    for &step in steps {
        let mapped = map(reference, step)?;
        let Some(score) = analysis.score_with_limits(&mapped, limits)? else {
            continue;
        };
        let (fast_data, _) = palette::encode_truecolor_with(reference, &mapped, true)?;
        top.push((fast_data.len(), step, mapped, score));
        top.sort_by_key(|t| (t.0, t.1));
        top.truncate(2);
    }
    if top.is_empty() {
        return Ok(None);
    }
    let best_fast = top[0].0;
    let threshold = best_fast + best_fast / 50;
    let mut best: Option<Candidate> = None;
    for (rank, (fast_bytes, step, mapped, score)) in top.into_iter().enumerate() {
        if rank > 0 && fast_bytes > threshold {
            break;
        }
        let (data, filter) = palette::encode_truecolor(reference, &mapped)?;
        let candidate = Candidate {
            id: format!("png-predictive-step-{step}"),
            parameters: Parameters::PngPredictive { step, filter },
            bytes: data.len(),
            data,
            score,
        };
        if best.as_ref().is_none_or(|old| {
            candidate
                .bytes
                .cmp(&old.bytes)
                .then(candidate.score.loss.total_cmp(&old.score.loss))
                .then(candidate.id.cmp(&old.id))
                .is_lt()
        }) {
            best = Some(candidate);
        }
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::{self, Strength};

    fn reference() -> Reference {
        Reference {
            pixels: RgbImage::from_fn(37, 29, |x, y| {
                let mut n = (x + y * 37)
                    .wrapping_mul(747796405)
                    .wrapping_add(2891336453);
                n ^= n >> 16;
                image::Rgb([n as u8, (n >> 8) as u8, (n >> 16) as u8])
            }),
            alpha: Some(image::GrayImage::from_fn(37, 29, |x, y| {
                image::Luma([((x + y * 37) % 256) as u8])
            })),
            source_format: "png",
            jpeg_metadata: Default::default(),
        }
    }

    #[test]
    fn reconstructed_prediction_has_bounded_error_and_preserves_hidden_rgb() {
        let reference = reference();
        assert_eq!(map(&reference, 1).unwrap(), reference.pixels);
        for step in STEPS {
            let mapped = map(&reference, step).unwrap();
            for ((before, after), a) in reference
                .pixels
                .pixels()
                .zip(mapped.pixels())
                .zip(reference.alpha.as_ref().unwrap().pixels())
            {
                if a[0] == 0 {
                    assert_eq!(before, after);
                }
                for c in 0..3 {
                    assert!(before[c].abs_diff(after[c]) <= step / 2);
                }
            }
        }
        assert!(map(&reference, 0).is_err());
        assert!(map(&reference, 17).is_err());
    }

    #[test]
    fn encoded_truecolor_matches_mapping_with_all_alpha_levels() {
        let reference = reference();
        let analysis =
            Analysis::for_reference(&reference, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
        for step in STEPS {
            let result = candidate(&reference, &analysis, step, |_| true)
                .unwrap()
                .unwrap();
            let decoded = crate::decode_baseline(&result.data).unwrap();
            assert_eq!(decoded.pixels, map(&reference, step).unwrap());
            assert_eq!(decoded.alpha, reference.alpha);
            quality::verify_transparency(
                &reference,
                &result.data,
                crate::resources::DEFAULT_MEMORY_BYTES,
            )
            .unwrap();
            assert_eq!(
                analysis.score(&decoded.pixels).unwrap().loss,
                result.score.loss
            );
        }
    }

    #[test]
    fn exact_ramp_keeps_lossless_fallback_when_quantization_is_larger() {
        let reference = Reference {
            pixels: RgbImage::from_fn(64, 64, |x, y| {
                image::Rgb([
                    (x * 4 + 1) as u8,
                    (y * 4 + 1) as u8,
                    ((x + y) * 2 + 1) as u8,
                ])
            }),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let source = crate::encode_png_lossless(&reference).unwrap();
        let selected = quality::compress(&source, Strength::default()).unwrap();
        assert!(selected.bytes <= source.len());
        assert_eq!(
            crate::decode_baseline(&selected.data).unwrap().pixels,
            reference.pixels
        );
    }

    #[test]
    fn high_color_noisy_gradient_can_compress_without_a_palette() {
        let reference = Reference {
            pixels: RgbImage::from_fn(64, 64, |x, y| {
                // Exact affine ramps already compress well losslessly. Include
                // bounded irregular residuals to exercise precision reduction.
                let mut n = (x + y * 64)
                    .wrapping_mul(747796405)
                    .wrapping_add(2891336453);
                n ^= n >> 16;
                image::Rgb([
                    (x * 4 + (n & 3)) as u8,
                    (y * 4 + ((n >> 2) & 3)) as u8,
                    ((x + y) * 2 + ((n >> 4) & 3)) as u8,
                ])
            }),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        let result = candidate(&reference, &analysis, 4, |s| {
            Strength::default().limits().accepts(s)
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            result.data[25], 2,
            "RGB PNG must not be forced into a palette"
        );
        let decoded = crate::decode_baseline(&result.data).unwrap();
        let distinct: std::collections::BTreeSet<_> =
            decoded.pixels.pixels().map(|p| p.0).collect();
        assert!(distinct.len() > 256);
        assert!(Strength::default().limits().accepts(&result.score));
        assert!(result.bytes < crate::encode_png_lossless(&reference).unwrap().len());
    }
}
