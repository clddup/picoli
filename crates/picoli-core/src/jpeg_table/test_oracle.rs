//! Test-only frozen v84 driver, without its diagnostic1GiB resource gate.
//! Unbounded coefficient IO/model entry point remain independent of product
//! admission and the conditional wrapper. Never compiled into product builds.
use super::{
    coefficient_io::{self, Coding},
    coupled, model, nonworse, projection, solver,
};
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    quality::Strength,
};
pub struct SolutionInfo {
    pub changed_frequencies: usize,
    pub joint_guard: Option<coupled::Info>,
}
pub struct Stats {
    pub paired: SolutionInfo,
}
pub struct Output {
    pub plain: Vec<u8>,
    pub raw: Vec<u8>,
    pub control_score: Score,
    pub plain_score: Score,
    pub raw_score: Score,
    pub stats: Stats,
    pub selected_variant: &'static str,
    pub identity_file_exact: bool,
}
pub fn run_projected(
    reference: &Reference,
    current: &[u8],
    coding: Coding,
    budget: u64,
) -> Result<Output, Error> {
    run(reference, current, coding, budget, false)
}
pub fn run_coupled(
    reference: &Reference,
    current: &[u8],
    coding: Coding,
    budget: u64,
) -> Result<Output, Error> {
    run(reference, current, coding, budget, true)
}
fn run(
    reference: &Reference,
    current: &[u8],
    coding: Coding,
    budget: u64,
    joint: bool,
) -> Result<Output, Error> {
    let analysis = Analysis::for_reference(reference, budget)?;
    let captured = coefficient_io::capture(current, budget)?;
    let identity = coefficient_io::serialize(reference, &captured.frame, coding)?;
    if identity != current {
        return Err(Error::Encoding(
            "frozen v84 OFF does not reproduce current full file".into(),
        ));
    }
    let control_score = analysis.score(&captured.pixels)?;
    let model = projection::build(
        reference,
        &analysis,
        &captured.pixels,
        &captured.frame,
        coding.scans,
        usize::MAX,
    )?;
    let encode = |paired| -> Result<_, Error> {
        let (solution, joint_guard) = if joint {
            let (solution, info) =
                coupled::solve(&model, &captured.frame, &analysis, &captured.pixels, paired)?;
            (solution, Some(info))
        } else {
            (solver::solve(&model.curves, paired), None)
        };
        let mut frame = coefficient_io::capture(current, budget)?.frame;
        let changed_frequencies = model::apply(&model, &solution, &mut frame)?;
        let data = coefficient_io::serialize(reference, &frame, coding)?;
        let decoded = crate::decode_with_budget(&data, budget)?;
        if decoded.pixels.dimensions() != reference.pixels.dimensions()
            || decoded.alpha != reference.alpha
        {
            return Err(Error::Encoding("frozen v84 changed geometry/Alpha".into()));
        }
        Ok((
            data,
            analysis.score(&decoded.pixels)?,
            SolutionInfo {
                changed_frequencies,
                joint_guard,
            },
        ))
    };
    let (plain, plain_score, _) = encode(false)?;
    let (raw, raw_score, paired) = encode(true)?;
    let pa =
        plain.len() < current.len() && nonworse(&plain_score, &control_score, Strength::default());
    let ra = raw.len() < current.len() && nonworse(&raw_score, &control_score, Strength::default());
    let selected_variant = if ra && (!pa || raw.len() < plain.len()) {
        "paired_luma_tables"
    } else if pa {
        "single_luma_tables"
    } else {
        "current"
    };
    Ok(Output {
        plain,
        raw,
        control_score,
        plain_score,
        raw_score,
        stats: Stats { paired },
        selected_variant,
        identity_file_exact: true,
    })
}
