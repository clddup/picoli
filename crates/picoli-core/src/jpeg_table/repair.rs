//! Conditional v84 coupled quality repair. Called only after a fully emitted
//! v81 quality rejection has a strict size opportunity over the post-v81 winner.
use super::{
    Captured, Coding, coefficient_io, coupled, encode_solution, generated_sampling, nonworse,
    projection, resources,
};
use crate::{
    Error, Reference,
    analysis::Analysis,
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};

pub(super) fn refine(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 || reference.source_format != "jpeg" || reference.alpha.is_some() {
        return Ok(());
    }
    let Some(current) = quality::best(candidates.iter(), strength) else {
        return Ok(());
    };
    // A successful v81 table candidate is a valid new control. Its sampling is
    // inherited from the generated root, but its OUTER coding flags govern OFF.
    let (scans_per_component, eob_runs, scan_huffman) = match &current.parameters {
        Parameters::Jpeg {
            scans_per_component,
            eob_runs,
            scan_huffman,
            ..
        }
        | Parameters::JpegTableAllocated {
            scans_per_component,
            eob_runs,
            scan_huffman,
            ..
        } => (*scans_per_component, *eob_runs, *scan_huffman),
        _ => return Ok(()),
    };
    let sampling = generated_sampling(&current.parameters)?;
    if analysis.dimensions != reference.pixels.dimensions() {
        return Err(Error::Argument("JPEG repair analysis dimensions differ"));
    }
    if resources::sampling_factors(sampling).is_none() || !matches!(scans_per_component, 2 | 4) {
        return Err(Error::Argument("JPEG repair unsupported generated coding"));
    }
    if current.bytes != current.data.len() {
        return Err(Error::Argument("JPEG repair control byte count mismatch"));
    }
    let Some(admission) =
        resources::preflight_coupled(source, reference, analysis, candidates, current, sampling)
    else {
        return Ok(());
    };
    let coding = Coding {
        scans: scans_per_component,
        eob_runs,
        scan_huffman,
    };
    let Captured { frame, pixels } = coefficient_io::capture_generated(
        &current.data,
        reference,
        sampling,
        admission.coefficients,
    )?;
    let identity =
        coefficient_io::serialize_bounded(reference, &frame, coding, admission.output_limit)?
            .ok_or(Error::Encoding(
                "coupled quantizer OFF exceeded current full file".into(),
            ))?;
    if identity != current.data {
        return Err(Error::Encoding(
            "coupled quantizer OFF does not reproduce current full file".into(),
        ));
    }
    drop(identity);
    let control_score = analysis.score(&pixels)?;
    let Some(model) = projection::build_bounded(
        reference,
        analysis,
        &pixels,
        &frame,
        coding.scans,
        admission.histogram_entries,
    )?
    else {
        return Ok(());
    };
    debug_assert!(model.histogram_entries <= admission.histogram_entries);
    let (single, single_guard) = coupled::solve(&model, &frame, analysis, &pixels, false)?;
    let (paired, paired_guard) = coupled::solve(&model, &frame, analysis, &pixels, true)?;
    // Each solve owns/releases its own factors, field and fallible step cache.
    // No coupled state, original RGB or original snapshot survives serialization.
    drop(frame);
    drop(pixels);
    for (solution, guard) in [(&single, &single_guard), (&paired, &paired_guard)] {
        if solution.total.pixel > 0
            || solution.total.edge > 0
            || solution.total.tile > 0
            || solution.total.rate > 0
            || guard.accepted != solution.exchanges
            || guard.accepted.checked_add(guard.rejected) != Some(guard.trials)
        {
            return Err(Error::Encoding(
                "coupled solver violated model budgets".into(),
            ));
        }
    }
    let plain = encode_solution(
        reference, analysis, current, &model, &single, &admission, coding,
    )?;
    let raw = encode_solution(
        reference, analysis, current, &model, &paired, &admission, coding,
    )?;
    let accepted = |a: &super::Alternative| {
        a.data.len() < current.data.len() && nonworse(&a.score, &control_score, strength)
    };
    let (selected, allocation, exchanges) = match (plain.filter(accepted), raw.filter(accepted)) {
        (Some(p), Some(r)) if r.data.len() < p.data.len() => {
            (r, "coupled_paired_luma_tables", paired.exchanges)
        }
        (Some(p), _) => (p, "coupled_single_luma_tables", single.exchanges),
        (None, Some(r)) => (r, "coupled_paired_luma_tables", paired.exchanges),
        (None, None) => return Ok(()),
    };
    let candidate = Candidate {
        id: format!("{}-repair-{allocation}", current.id),
        parameters: Parameters::JpegTableAllocated {
            control: Box::new(current.parameters.clone()),
            luma_quantization_table: selected.luma_table,
            rewritten_frequencies: selected.rewritten,
            scans_per_component,
            eob_runs,
            scan_huffman,
            model: "v84_current_rgb_jacobian_q14_joint_field_top8_original_dct_edge_full_legal_q",
            allocation,
            exchanges,
        },
        bytes: selected.data.len(),
        data: selected.data,
        score: selected.score,
    };
    drop(model);
    candidates.push(candidate);
    Ok(())
}
