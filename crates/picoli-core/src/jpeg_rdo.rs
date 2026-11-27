//! Bounded JPEG coefficient refinement with unchanged image-domain limits.
//! Standard rate-distortion technique, not an original codec or human model.
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    jpeg::{self, Encoding, Entropy},
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};

fn nonworse(candidate: &Score, control: &Score) -> bool {
    candidate.weighted_pixel_error <= control.weighted_pixel_error
        && candidate.weighted_edge_error <= control.weighted_edge_error
        && candidate.worst_tile_error <= control.worst_tile_error
}

pub(crate) fn refine(
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 {
        return Ok(());
    }
    let Some(winner) = quality::best(candidates.iter(), strength) else {
        return Ok(());
    };
    let Parameters::Jpeg {
        shape,
        sampling,
        quantization_scale,
        scans_per_component,
        eob_runs,
        scan_huffman,
        coefficient_rdo_lambda: 0,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
    } = winner.parameters
    else {
        return Ok(());
    };
    if analysis.dimensions != reference.pixels.dimensions() {
        return Err(Error::Argument("analysis and reference dimensions differ"));
    }
    let entropy = match (eob_runs, scan_huffman) {
        (false, false) => Entropy::IndependentBlocks,
        (true, false) => Entropy::EobRuns,
        (true, true) => Entropy::ScanHuffman,
        (false, true) => return Err(Error::Argument("scan Huffman requires EOB runs")),
    };
    let base = Encoding {
        shape,
        sampling,
        scale: quantization_scale,
        scans: scans_per_component,
        entropy,
        q_percent: 100,
        rdo_lambda: 0,
        weighted_sampling: false,
    };
    let zero = jpeg::encode_config(reference, analysis, base)?;
    if zero != winner.data {
        return Err(Error::Encoding(
            "JPEG coefficient zero-bypass differs from selected control".into(),
        ));
    }
    drop(zero);
    let decoded_control = crate::decode_with_budget(&winner.data, analysis.memory_budget_bytes)?;
    let control_score = analysis.score(&decoded_control.pixels)?;
    drop(decoded_control);
    let limits = strength.limits();
    let mut result: Option<Candidate> = None;
    // Preserve both old candidates. A single complementary model is tried
    // ONLY when neither old candidate improved the zero-control. This cannot
    // discard the old q90 winner as the rejected two-trial heuristic did.
    for (q_percent, lambda, weighted_sampling) in
        [(95, 256, false), (90, 1024, false), (95, 256, true)]
    {
        if weighted_sampling && result.is_some() {
            break;
        }
        let data = jpeg::encode_config(
            reference,
            analysis,
            Encoding {
                q_percent,
                rdo_lambda: lambda,
                weighted_sampling,
                ..base
            },
        )?;
        if data.len() >= winner.bytes || result.as_ref().is_some_and(|old| data.len() > old.bytes) {
            continue;
        }
        let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
        if decoded.pixels.dimensions() != reference.pixels.dimensions()
            || decoded.alpha != reference.alpha
        {
            return Err(Error::Encoding(
                "JPEG coefficient refinement changed dimensions or alpha".into(),
            ));
        }
        let Some(score) = analysis.score_with_limits(&decoded.pixels, limits)? else {
            continue;
        };
        if !nonworse(&score, &control_score) {
            continue;
        }
        let candidate = Candidate {
            id: format!(
                "{}-qmargin{q_percent}-rdo{lambda}{}",
                winner.id,
                if weighted_sampling {
                    "-sampling-weighted"
                } else {
                    ""
                }
            ),
            parameters: Parameters::Jpeg {
                shape,
                sampling,
                quantization_scale,
                scans_per_component,
                eob_runs,
                scan_huffman,
                coefficient_rdo_lambda: lambda,
                quantization_margin_percent: q_percent,
                coefficient_rdo_sampling_weights: weighted_sampling,
            },
            bytes: data.len(),
            data,
            score,
        };
        if result.as_ref().is_none_or(|old| {
            candidate.bytes < old.bytes
                || (candidate.bytes == old.bytes
                    && (candidate
                        .score
                        .loss
                        .total_cmp(&old.score.loss)
                        .then(candidate.id.cmp(&old.id))
                        .is_lt()))
        }) {
            result = Some(candidate);
        }
    }
    if let Some(candidate) = result {
        candidates.push(candidate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jpeg::Shape;

    fn reference() -> Reference {
        Reference {
            pixels: image::RgbImage::from_fn(37, 29, |x, y| {
                image::Rgb([
                    (x * 5 + y * 2) as u8,
                    (x * 3 + y * 4) as u8,
                    ((x / 3 + y / 5) * 19) as u8,
                ])
            }),
            alpha: None,
            source_format: "jpeg",
            jpeg_metadata: Default::default(),
        }
    }
    fn candidate(
        reference: &Reference,
        analysis: &Analysis,
        q_percent: u16,
        lambda: u32,
    ) -> Candidate {
        let data = jpeg::encode_config(
            reference,
            analysis,
            Encoding {
                shape: Shape::Detail,
                sampling: 444,
                scale: 30,
                scans: 2,
                entropy: Entropy::ScanHuffman,
                q_percent,
                rdo_lambda: lambda,
                weighted_sampling: false,
            },
        )
        .unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        Candidate {
            id: format!("control-{q_percent}-{lambda}"),
            bytes: data.len(),
            score: analysis.score(&decoded.pixels).unwrap(),
            data,
            parameters: Parameters::Jpeg {
                shape: Shape::Detail,
                sampling: 444,
                quantization_scale: 30,
                scans_per_component: 2,
                eob_runs: true,
                scan_huffman: true,
                coefficient_rdo_lambda: lambda,
                quantization_margin_percent: q_percent,
                coefficient_rdo_sampling_weights: false,
            },
        }
    }

    #[test]
    fn complement_never_discards_actual_legacy_candidates_or_spends_their_quality() {
        let strength = Strength::new(100).unwrap();
        for seed in 0..8u8 {
            let mut raw = reference();
            for p in raw.pixels.pixels_mut() {
                p[0] = p[0].wrapping_add(seed * 17);
                p[1] = p[1].wrapping_add(seed * 11);
            }
            let input = crate::encode_jpeg_baseline(&raw, 95).unwrap();
            let r = crate::decode_baseline(&input).unwrap();
            let a = Analysis::for_reference(&r, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
            let mut legacy = vec![candidate(&r, &a, 100, 0)];
            let mut combined = vec![candidate(&r, &a, 100, 0)];
            assert!(strength.limits().accepts(&legacy[0].score));
            crate::jpeg_rdo_legacy::refine(&r, &a, &mut legacy, strength).unwrap();
            refine(&r, &a, &mut combined, strength).unwrap();
            let before = quality::best(legacy.iter(), strength).unwrap();
            let after = quality::best(combined.iter(), strength).unwrap();
            assert!(after.bytes <= before.bytes);
            assert!(nonworse(&after.score, &before.score));
            if matches!(
                before.parameters,
                Parameters::Jpeg {
                    coefficient_rdo_lambda: 1..,
                    ..
                }
            ) {
                assert_eq!(after.data, before.data);
                assert_eq!(after.id, before.id);
            }
        }
    }

    #[test]
    fn actual_codec_refinement_obeys_component_guards_and_is_idempotent() {
        let reference = reference();
        let snapshot = reference.pixels.clone();
        let analysis =
            Analysis::for_reference(&reference, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
        let strength = Strength::new(100).unwrap();
        let control = candidate(&reference, &analysis, 100, 0);
        assert!(strength.limits().accepts(&control.score));
        let control_bytes = control.bytes;
        let control_score = control.score.clone();
        let mut candidates = vec![control];
        refine(&reference, &analysis, &mut candidates, strength).unwrap();
        assert!(candidates.len() <= 2);
        let selected = quality::best(candidates.iter(), strength).unwrap();
        assert!(selected.bytes <= control_bytes);
        assert!(nonworse(&selected.score, &control_score));
        assert!(strength.limits().accepts(&selected.score));
        let bytes = selected.data.clone();
        let count = candidates.len();
        refine(&reference, &analysis, &mut candidates, strength).unwrap();
        assert_eq!(candidates.len(), count);
        assert_eq!(
            quality::best(candidates.iter(), strength).unwrap().data,
            bytes
        );
        assert_eq!(reference.pixels, snapshot);
    }

    #[test]
    fn existing_finishers_do_not_requantize_rdo_signals_and_skip_guards_are_real() {
        let reference = reference();
        let analysis =
            Analysis::for_reference(&reference, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
        let strength = Strength::new(100).unwrap();
        let rdo = candidate(&reference, &analysis, 95, 256);
        assert!(strength.limits().accepts(&rdo.score));
        let bytes = rdo.data.clone();
        let mut candidates = vec![rdo];
        quality::refine_jpeg_candidates(&reference, &analysis, &mut candidates, strength).unwrap();
        quality::refine_jpeg_eob_candidates(&reference, &analysis, &mut candidates, strength)
            .unwrap();
        quality::refine_jpeg_scan_huffman_candidates(
            &reference,
            &analysis,
            &mut candidates,
            strength,
        )
        .unwrap();
        refine(&reference, &analysis, &mut candidates, strength).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].data, bytes);
        let mut zero = vec![candidate(&reference, &analysis, 100, 0)];
        let bytes = zero[0].data.clone();
        refine(&reference, &analysis, &mut zero, Strength::new(0).unwrap()).unwrap();
        assert_eq!(zero.len(), 1);
        assert_eq!(zero[0].data, bytes);
        let mut original = vec![quality::original(&reference, &analysis, &bytes).unwrap()];
        refine(&reference, &analysis, &mut original, strength).unwrap();
        assert_eq!(original.len(), 1);
        assert_eq!(original[0].data, bytes);
        let png_data = crate::encode_png_lossless(&reference).unwrap();
        let mut png = vec![Candidate {
            id: "png".into(),
            parameters: Parameters::PngLossless,
            bytes: png_data.len(),
            data: png_data.clone(),
            score: analysis.score(&reference.pixels).unwrap(),
        }];
        refine(&reference, &analysis, &mut png, strength).unwrap();
        assert_eq!(png.len(), 1);
        assert_eq!(png[0].data, png_data);
    }
}
