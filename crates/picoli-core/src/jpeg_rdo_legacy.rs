//! Frozen v32 production finisher used only as an actual-codec test oracle.
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
    // Frozen global policy, validated separately from its development cohort.
    // No image identity, fixed image coordinates or changed strength is used.
    for (q_percent, lambda) in [(95, 256), (90, 1024)] {
        let data = jpeg::encode_config(
            reference,
            analysis,
            Encoding {
                q_percent,
                rdo_lambda: lambda,
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
            id: format!("{}-qmargin{q_percent}-rdo{lambda}", winner.id),
            parameters: Parameters::Jpeg {
                shape,
                sampling,
                quantization_scale,
                scans_per_component,
                eob_runs,
                scan_huffman,
                coefficient_rdo_lambda: lambda,
                quantization_margin_percent: q_percent,
                coefficient_rdo_sampling_weights: false,
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
