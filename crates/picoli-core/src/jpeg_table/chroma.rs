//! Bounded optional chroma refinement after the unchanged v85 prefix.
use super::{
    coefficient_io::{self, Captured, Coding},
    incremental::State,
    nonworse, resources,
};
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};
use jpeg_encoder::CoefficientRateModel;
use serde::Serialize;
use zune_jpeg::ChromaRenderer;
#[cfg(test)]
mod tests;
#[derive(Default, Serialize)]
pub struct PassStats {
    pub chroma_blocks: usize,
    pub nonzero_positions: usize,
    pub category_proposals: usize,
    pub prior_rate_rejections: usize,
    pub exact_rgb_previews: usize,
    pub committed: usize,
    pub compensated_commits: usize,
    pub rejected_quality: usize,
    pub rejected_safety: usize,
    pub modeled_bits_saved: u64,
}
fn same(a: &Score, b: &Score) -> bool {
    a.weighted_pixel_error.to_bits() == b.weighted_pixel_error.to_bits()
        && a.weighted_edge_error.to_bits() == b.weighted_edge_error.to_bits()
        && a.worst_tile_error.to_bits() == b.worst_tile_error.to_bits()
        && a.loss.to_bits() == b.loss.to_bits()
}
fn descend(value: i16) -> Option<i16> {
    let m = value.unsigned_abs();
    if m == 0 || m > 1023 {
        return None;
    }
    let category = 16 - m.leading_zeros();
    Some((((1u16 << (category - 1)) - 1) as i16) * value.signum())
}
fn bits(rate: &CoefficientRateModel, b: &[i16; 64]) -> Result<u32, Error> {
    rate.bits(b, true)
        .ok_or(Error::Argument("chroma block outside legal AC domain"))
}
fn preview(
    renderer: &ChromaRenderer,
    component: usize,
    bx: usize,
    by: usize,
    block: &[i16; 64],
    rgb: &mut [u8; 972],
    stats: &mut PassStats,
) -> Result<Option<(usize, usize, usize, usize)>, Error> {
    match renderer.render_block(component, bx, by, block, rgb) {
        Ok(rect) => {
            stats.exact_rgb_previews += 1;
            Ok(Some(rect))
        }
        Err(zune_jpeg::errors::DecodeErrors::FormatStatic(
            "Luma renderer: coefficient arithmetic exceeds safe IDCT bounds",
        )) => {
            stats.rejected_safety += 1;
            Ok(None)
        }
        Err(e) => Err(Error::InvalidJpeg(e.to_string())),
    }
}

struct Admission {
    coefficients: u64,
    output_limit: usize,
    renderer_bytes: usize,
}
fn settings(p: &Parameters) -> Option<(u16, Coding)> {
    match p {
        Parameters::Jpeg {
            sampling,
            scans_per_component,
            eob_runs,
            scan_huffman,
            ..
        } => Some((
            *sampling,
            Coding {
                scans: *scans_per_component,
                eob_runs: *eob_runs,
                scan_huffman: *scan_huffman,
            },
        )),
        Parameters::JpegTableAllocated { control, .. } => settings(control),
        _ => None,
    }
}
fn admission(
    source: &[u8],
    r: &Reference,
    a: &Analysis,
    cs: &Vec<Candidate>,
    current: &Candidate,
    sampling: u16,
) -> Option<Admission> {
    let (w, h) = r.pixels.dimensions();
    if w == 0 || h == 0 || w > 65535 || h > 65535 {
        return None;
    }
    let (sx, sy) = resources::sampling_factors(sampling)?;
    let pixels = u64::from(w).checked_mul(u64::from(h))?;
    let rgb = pixels.checked_mul(3)?;
    let coefficient_bytes = resources::payload(w, h, sampling)?;
    // An i16 IDCT plane and its i16 coefficient plane have identical payloads.
    let renderer_bytes = coefficient_bytes.checked_add(rgb)?;
    let tiles = u64::from(w)
        .div_ceil(16)
        .checked_mul(u64::from(h).div_ceil(16))?;
    let tree = tiles.checked_next_power_of_two()?.checked_mul(16)?;
    let tile_bytes = tiles.checked_mul(16)?.checked_add(tree)?;
    let plan = crate::resources::Plan::new(w, h, 0, a.memory_budget_bytes, 1).ok()?;
    // Also meet State's separately enforced conservative common-plan admission.
    let state_estimate = plan
        .estimated_shared_bytes
        .checked_add(plan.estimated_worker_bytes)?
        .checked_add(rgb.checked_mul(2)?)?
        .checked_add(tile_bytes)?;
    let retained = cs.iter().try_fold(0u64, |n, c| {
        n.checked_add(c.data.capacity() as u64)?
            .checked_add(c.id.capacity() as u64)?
            .checked_add(resources::parameter_heap(&c.parameters)?)
    })?;
    let resident = (source.len() as u64)
        .checked_mul(2)?
        .checked_add(r.pixels.as_raw().capacity() as u64)?
        .checked_add(r.jpeg_metadata.capacity() as u64)?
        .checked_add(a.weights.capacity() as u64)?
        // Keep the old color reserve so compaction does not change which
        // optional refinements are admitted at the same budget.
        .checked_add((a.colors.capacity() as u64).checked_mul(12)?)?
        .checked_add(retained)?
        .checked_add(
            (cs.capacity().max(1) as u64)
                .checked_mul(3 * std::mem::size_of::<Candidate>() as u64)?,
        )?;
    let output_limit = current.data.len().checked_add(32)?;
    let attached = (output_limit as u64).checked_add(r.jpeg_metadata.len() as u64)?;
    // Conservative simultaneous superset: owned frame plus writer's zigzag
    // blocks; captured/state/final RGB; raw renderer+initial RGB; tile tree;
    // previous alternative, bounded output and metadata attachment; decoder,
    // ICC/parser reserve and score scratch. Sequential branches never coexist
    // as coefficient/RGB states. Actual renderer/frame drop before final decode.
    let peak = resident
        .checked_add(coefficient_bytes.checked_mul(2)?)?
        .checked_add(rgb.checked_mul(3)?)?
        .checked_add(renderer_bytes)?
        .checked_add(tile_bytes)?
        .checked_add(attached.checked_mul(3)?)?
        .checked_add(resources::decoder_working(w, sampling)?)?
        .checked_add((r.jpeg_metadata.len() as u64).checked_mul(32)?)?
        .checked_add(u64::from(w).checked_mul(12)?)?
        .checked_add(4 * 1024 * 1024)?;
    let _ = (sx, sy);
    if peak.max(state_estimate) > a.memory_budget_bytes {
        return None;
    }
    Some(Admission {
        coefficients: coefficient_bytes,
        output_limit,
        renderer_bytes: usize::try_from(renderer_bytes).ok()?,
    })
}
struct Alternative {
    data: Vec<u8>,
    score: Score,
    stats: PassStats,
    paired: bool,
}
pub(super) fn refine(
    source: &[u8],
    r: &Reference,
    a: &Analysis,
    cs: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 || r.source_format != "jpeg" || r.alpha.is_some() {
        return Ok(());
    }
    let Some(current) = quality::best(cs.iter(), strength) else {
        return Ok(());
    };
    let Some((sampling, coding)) = settings(&current.parameters) else {
        return Ok(());
    };
    if !matches!(coding.scans, 2 | 4) {
        return Err(Error::Argument("chroma generated scans must2/4"));
    }
    if current.bytes != current.data.len() || a.dimensions != r.pixels.dimensions() {
        return Err(Error::Argument(
            "chroma generated byte count/analysis mismatch",
        ));
    }
    let Some(admit) = admission(source, r, a, cs, current, sampling) else {
        return Ok(());
    };
    let mut best = None;
    for paired in [false, true] {
        if let Some(alt) = pass(r, a, current, sampling, coding, &admit, strength, paired)?
            && alt.data.len() < current.bytes
            && nonworse(&alt.score, &current.score, strength)
            && best
                .as_ref()
                .is_none_or(|b: &Alternative| alt.data.len() < b.data.len())
        {
            best = Some(alt);
        }
    }
    if let Some(alt) = best {
        let policy = if alt.paired {
            "cross_chroma_cofrequency"
        } else {
            "individual_chroma_categories"
        };
        let candidate = Candidate {
            id: format!("{}-chroma-{policy}", current.id),
            parameters: Parameters::JpegChromaRefined {
                control: Box::new(current.parameters.clone()),
                policy,
                commits: alt.stats.committed,
                compensated_commits: alt.stats.compensated_commits,
            },
            bytes: alt.data.len(),
            data: alt.data,
            score: alt.score,
        };
        cs.push(candidate);
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn pass(
    reference: &Reference,
    a: &Analysis,
    current: &Candidate,
    sampling: u16,
    coding: Coding,
    admit: &Admission,
    strength: Strength,
    paired: bool,
) -> Result<Option<Alternative>, Error> {
    let Captured { mut frame, pixels } =
        coefficient_io::capture_generated(&current.data, reference, sampling, admit.coefficients)?;
    let identity =
        coefficient_io::serialize_bounded(reference, &frame, coding, admit.output_limit)?
            .ok_or(Error::Encoding("chroma OFF exceeded full control".into()))?;
    if identity != current.data {
        return Err(Error::Encoding(
            "chroma OFF differs from complete generated control".into(),
        ));
    }
    drop(identity);
    let bounds = &current.score;
    let control_score = a.score(&pixels)?;
    if !same(&control_score, bounds) {
        return Err(Error::Encoding(
            "chroma generated control score drift".into(),
        ));
    }
    let (mut renderer, initial) = ChromaRenderer::new(&frame, admit.renderer_bytes)
        .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
    if initial.as_slice() != pixels.as_raw().as_slice() {
        return Err(Error::Encoding(
            "chroma initial renderer RGB differs".into(),
        ));
    }
    drop(initial);
    let mut state = State::new(a, &pixels)?;
    drop(pixels);
    if !same(&state.score(), bounds) {
        return Err(Error::Encoding("chroma initial score bits differ".into()));
    }
    let rate = CoefficientRateModel::new(coding.scans)
        .ok_or(Error::Argument("chroma requires2/4 scans"))?;
    let mut stats = PassStats::default();
    let mut rgb = [0u8; 972];
    // Complete raster traversal, then SOF-order Cb/Cr and one forward zigzag.
    let cols = frame.components[1].block_width;
    let rows = frame.components[1].block_height;
    for by in 0..rows {
        for bx in 0..cols {
            for component in 1..=2 {
                stats.chroma_blocks += 1;
                let other = 3 - component;
                let start = (by * cols + bx) * 64;
                let mut block: [i16; 64] = frame.components[component].coefficients
                    [start..start + 64]
                    .try_into()
                    .map_err(|_| Error::Argument("bad chroma block"))?;
                for zigzag in 1..64 {
                    let f = CoefficientRateModel::natural_index(zigzag)
                        .ok_or(Error::Argument("bad chroma zigzag"))?;
                    let Some(value) = descend(block[f]) else {
                        continue;
                    };
                    stats.nonzero_positions += 1;
                    stats.category_proposals += 1;
                    let before_bits = bits(&rate, &block)?;
                    let mut trial = block;
                    trial[f] = value;
                    let after_bits = bits(&rate, &trial)?;
                    let mut accepted = false;
                    if after_bits < before_bits {
                        if let Some(rect) =
                            preview(&renderer, component, bx, by, &trial, &mut rgb, &mut stats)?
                        {
                            if state.try_patch_at_strength(
                                rect,
                                &rgb[..rect.2 * rect.3 * 3],
                                bounds,
                                strength,
                            )? {
                                renderer
                                    .commit_block(component, bx, by, &trial)
                                    .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
                                block = trial;
                                accepted = true;
                                stats.committed += 1;
                                stats.modeled_bits_saved += u64::from(before_bits - after_bits);
                            } else {
                                stats.rejected_quality += 1;
                            }
                        }
                    } else {
                        stats.prior_rate_rejections += 1;
                    }
                    if !accepted && paired {
                        let other_block: [i16; 64] = frame.components[other].coefficients
                            [start..start + 64]
                            .try_into()
                            .map_err(|_| Error::Argument("bad compensating chroma block"))?;
                        let other_bits = bits(&rate, &other_block)?;
                        // The pending primary edit never enters the score state alone.
                        // A rejected joint trial restores the renderer before continuing.
                        let mut pending = false;
                        for delta in [-1i16, 1] {
                            let Some(g) = other_block[f]
                                .checked_add(delta)
                                .filter(|x| x.unsigned_abs() <= 1023)
                            else {
                                continue;
                            };
                            let mut pair = other_block;
                            pair[f] = g;
                            let pair_bits = bits(&rate, &pair)?;
                            let before = u64::from(before_bits) + u64::from(other_bits);
                            let after = u64::from(after_bits) + u64::from(pair_bits);
                            if after >= before {
                                stats.prior_rate_rejections += 1;
                                continue;
                            }
                            // Check primary safety without changing either retained plane.
                            if !pending {
                                if preview(
                                    &renderer, component, bx, by, &trial, &mut rgb, &mut stats,
                                )?
                                .is_none()
                                {
                                    break;
                                }
                                renderer
                                    .commit_block(component, bx, by, &trial)
                                    .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
                                pending = true;
                            }
                            if let Some(rect) =
                                preview(&renderer, other, bx, by, &pair, &mut rgb, &mut stats)?
                            {
                                if state.try_patch_at_strength(
                                    rect,
                                    &rgb[..rect.2 * rect.3 * 3],
                                    bounds,
                                    strength,
                                )? {
                                    renderer
                                        .commit_block(other, bx, by, &pair)
                                        .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
                                    frame.components[other].coefficients[start..start + 64]
                                        .copy_from_slice(&pair);
                                    block = trial;
                                    accepted = true;
                                    stats.committed += 1;
                                    stats.compensated_commits += 1;
                                    stats.modeled_bits_saved += before - after;
                                    break;
                                } else {
                                    stats.rejected_quality += 1;
                                }
                            }
                        }
                        if pending && !accepted {
                            renderer
                                .commit_block(component, bx, by, &block)
                                .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
                        }
                    }
                }
                frame.components[component].coefficients[start..start + 64].copy_from_slice(&block);
            }
        }
    }
    drop(renderer);
    let data = coefficient_io::serialize_bounded(reference, &frame, coding, admit.output_limit)?;
    drop(frame);
    let Some(data) = data else { return Ok(None) };
    let decoded = crate::decode_with_budget(&data, a.memory_budget_bytes)?;
    if decoded.pixels.dimensions() != reference.pixels.dimensions()
        || decoded.alpha != reference.alpha
    {
        return Err(Error::Encoding("chroma changed geometry/Alpha".into()));
    }
    let score = a.score(&decoded.pixels)?;
    if decoded.pixels != *state.pixels() || !same(&score, &state.score()) {
        return Err(Error::Encoding(
            "chroma final full decode RGB/score differs from incremental state".into(),
        ));
    }
    if !nonworse(&score, bounds, strength) {
        return Err(Error::Encoding(
            "chroma escaped original component/strength limits".into(),
        ));
    }
    Ok(Some(Alternative {
        data,
        score,
        stats,
        paired,
    }))
}
