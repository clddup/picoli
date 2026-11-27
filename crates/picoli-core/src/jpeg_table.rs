//! Frozen v80 global luma quantizer proposals, admitted as an optional finisher.
//! Models propose only; full JPEG bytes and all three actual RGB errors decide.
mod chroma;
mod coefficient_io;
mod coupled;
mod incremental;
mod model;
mod projection;
mod repair;
mod resources;
mod solver;
#[cfg(test)]
mod test_oracle;
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};
use coefficient_io::{Captured, Coding};
use resources::Admission;

fn nonworse(a: &Score, b: &Score, strength: Strength) -> bool {
    a.weighted_pixel_error <= b.weighted_pixel_error
        && a.weighted_edge_error <= b.weighted_edge_error
        && a.worst_tile_error <= b.worst_tile_error
        && strength.limits().accepts(a)
}

struct Alternative {
    data: Vec<u8>,
    score: Score,
    luma_table: Vec<u16>,
    rewritten: Vec<usize>,
}
fn encode_solution(
    reference: &Reference,
    analysis: &Analysis,
    current: &Candidate,
    model: &model::Model,
    solution: &solver::Solution,
    admission: &Admission,
    coding: Coding,
) -> Result<Option<Alternative>, Error> {
    let sampling = generated_sampling(&current.parameters)?;
    let Captured { mut frame, pixels } = coefficient_io::capture_generated(
        &current.data,
        reference,
        sampling,
        admission.coefficients,
    )?;
    drop(pixels);
    model::apply(model, solution, &mut frame)?;
    let luma_table = frame.components[0].quantization_table.to_vec();
    let data =
        coefficient_io::serialize_bounded(reference, &frame, coding, admission.output_limit)?;
    // The original capture/model frame is already gone. Release this encoder
    // frame too, before metadata/ICC validation and the unrelated output decode.
    drop(frame);
    let Some(data) = data else {
        return Ok(None);
    };
    // Decode even a fully emitted non-smaller file: codec/geometry/score errors
    // are not admission failures and must propagate. Only the size cap is a no-op.
    let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
    if decoded.pixels.dimensions() != reference.pixels.dimensions()
        || decoded.alpha != reference.alpha
    {
        return Err(Error::Encoding(
            "quantizer allocation changed geometry/Alpha".into(),
        ));
    }
    let score = analysis.score(&decoded.pixels)?;
    let rewritten = solution
        .choices
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(f, &c)| (!model.curves[f][c].retain).then_some(f))
        .collect();
    Ok(Some(Alternative {
        data,
        score,
        luma_table,
        rewritten,
    }))
}

fn generated_sampling(parameters: &Parameters) -> Result<u16, Error> {
    match parameters {
        Parameters::Jpeg { sampling, .. } => Ok(*sampling),
        Parameters::JpegTableAllocated { control, .. } => generated_sampling(control),
        _ => Err(Error::Argument("JPEG table missing generated control")),
    }
}

pub(crate) fn refine(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    // The old function returns only a trigger, never model/RGB/frame storage.
    // Its whole transient working set is gone before repair admission begins.
    if refine_original(source, reference, analysis, candidates, strength)? {
        repair::refine(source, reference, analysis, candidates, strength)?;
    }
    Ok(())
}

fn repair_opportunity(rejected_bytes: Option<usize>, post_v81_best_bytes: usize) -> bool {
    rejected_bytes.is_some_and(|n| n < post_v81_best_bytes)
}

/// Run the complete unchanged v81 pass, reporting only a fully serialized
/// actual-quality rejection strictly smaller than the POST-v81 best winner.
fn refine_original(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<bool, Error> {
    if strength.percent() == 0 || reference.source_format != "jpeg" || reference.alpha.is_some() {
        return Ok(false);
    }
    let Some(current) = quality::best(candidates.iter(), strength) else {
        return Ok(false);
    };
    // Original, PNG and already allocated/finalized variants are never rewritten.
    let Parameters::Jpeg {
        sampling,
        scans_per_component,
        eob_runs,
        scan_huffman,
        ..
    } = current.parameters
    else {
        return Ok(false);
    };
    if analysis.dimensions != reference.pixels.dimensions() {
        return Err(Error::Argument("JPEG table analysis dimensions differ"));
    }
    if resources::sampling_factors(sampling).is_none() || !matches!(scans_per_component, 2 | 4) {
        return Err(Error::Argument("JPEG table unsupported generated coding"));
    }
    if current.bytes != current.data.len() {
        return Err(Error::Argument("JPEG table control byte count mismatch"));
    }
    let Some(admission) =
        resources::preflight(source, reference, analysis, candidates, current, sampling)
    else {
        return Ok(false);
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
                "global quantizer OFF exceeded current full file".into(),
            ))?;
    if identity != current.data {
        return Err(Error::Encoding(
            "global quantizer OFF does not reproduce current full file".into(),
        ));
    }
    drop(identity);
    let control_score = analysis.score(&pixels)?;
    let model = model::build_bounded(
        reference,
        analysis,
        &pixels,
        &frame,
        coding.scans,
        admission.histogram_entries,
    )?;
    drop(frame);
    drop(pixels);
    let Some(model) = model else {
        return Ok(false);
    };
    debug_assert!(model.histogram_entries <= admission.histogram_entries);
    debug_assert!(model.critical_tile.0 < (reference.pixels.width() as usize).div_ceil(16));
    debug_assert!(model.critical_tile.1 < (reference.pixels.height() as usize).div_ceil(16));
    debug_assert!(model.base_pixel >= 0 && model.base_edge >= 0 && model.base_tile >= 0);
    let single = solver::solve(&model.curves, false);
    let paired = solver::solve(&model.curves, true);
    for solution in [&single, &paired] {
        if solution.total.pixel > 0
            || solution.total.edge > 0
            || solution.total.tile > 0
            || solution.total.rate > 0
        {
            return Err(Error::Encoding(
                "quantizer solver violated model budgets".into(),
            ));
        }
    }
    let plain = encode_solution(
        reference, analysis, current, &model, &single, &admission, coding,
    )?;
    let raw = encode_solution(
        reference, analysis, current, &model, &paired, &admission, coding,
    )?;
    let accepted = |a: &Alternative| {
        a.data.len() < current.data.len() && nonworse(&a.score, &control_score, strength)
    };
    let rejected_bytes = [&plain, &raw]
        .into_iter()
        .filter_map(|a| {
            a.as_ref()
                .filter(|a| !nonworse(&a.score, &control_score, strength))
                .map(|a| a.data.len())
        })
        .min();
    let plain = plain.filter(accepted);
    let raw = raw.filter(accepted);
    // Frozen v80 tie order: the single solution wins equal complete lengths,
    // irrespective of score/lambda/model rate or the paired solution's provenance.
    let (selected, allocation, exchanges) = match (plain, raw) {
        (Some(p), Some(r)) if r.data.len() < p.data.len() => {
            (r, "paired_luma_tables", paired.exchanges)
        }
        (Some(p), _) => (p, "single_luma_tables", single.exchanges),
        (None, Some(r)) => (r, "paired_luma_tables", paired.exchanges),
        (None, None) => return Ok(repair_opportunity(rejected_bytes, current.bytes)),
    };
    let candidate = Candidate {
        id: format!("{}-table-{allocation}", current.id),
        parameters: Parameters::JpegTableAllocated {
            control: Box::new(current.parameters.clone()),
            luma_quantization_table: selected.luma_table,
            rewritten_frequencies: selected.rewritten,
            scans_per_component,
            eob_runs,
            scan_huffman,
            model: "v80_source_fdct_histogram_independent_ac_critical16_full_legal_q",
            allocation,
            exchanges,
        },
        bytes: selected.data.len(),
        data: selected.data,
        score: selected.score,
    };
    drop(model);
    let repair = repair_opportunity(rejected_bytes, candidate.bytes);
    candidates.push(candidate);
    Ok(repair)
}

#[cfg(test)]
mod tests;

/// Separate post-prefix pass: the original table API and old tests stay exact.
pub(crate) fn refine_chroma(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    chroma::refine(source, reference, analysis, candidates, strength)
}
