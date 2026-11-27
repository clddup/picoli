//! Experimental quality-constrained compression. No target file size is used.
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    jpeg,
    optimize::{Candidate, Job, Parameters},
    palette, predictive,
};
use serde::Serialize;

pub const DEFAULT_STRENGTH: u8 = 50;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Strength(u8);

impl Default for Strength {
    fn default() -> Self {
        Self(DEFAULT_STRENGTH)
    }
}

impl Strength {
    pub fn new(percent: u8) -> Result<Self, Error> {
        if percent > 100 {
            return Err(Error::Argument("strength must be 0..=100 percent"));
        }
        Ok(Self(percent))
    }
    pub fn percent(self) -> u8 {
        self.0
    }
    pub fn limits(self) -> Limits {
        // Frozen experimental mapping, independent of competitor files. These
        // are proxy-error bounds, not calibrated claims about perceived quality.
        let factor = (f64::from(self.0) / 50.0).powi(4);
        Limits {
            pixel: 64.0 * factor,
            edge: 16.0 * factor,
            tile: 512.0 * factor,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Limits {
    pub pixel: f64,
    pub edge: f64,
    pub tile: f64,
}

impl Limits {
    pub fn accepts(self, score: &Score) -> bool {
        score.weighted_pixel_error <= self.pixel
            && score.weighted_edge_error <= self.edge
            && score.worst_tile_error <= self.tile
    }
}

pub fn best<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
    strength: Strength,
) -> Option<&'a Candidate> {
    candidates
        .filter(|c| strength.limits().accepts(&c.score))
        .min_by(|a, b| {
            a.bytes
                .cmp(&b.bytes)
                .then(a.score.loss.total_cmp(&b.score.loss))
                .then(a.id.cmp(&b.id))
        })
}

pub fn original(
    reference: &Reference,
    analysis: &Analysis,
    source: &[u8],
) -> Result<Candidate, Error> {
    Ok(Candidate {
        id: "original-preserved".into(),
        parameters: Parameters::Original {
            source_format: reference.source_format,
        },
        bytes: source.len(),
        data: source.to_vec(),
        score: analysis.score(&reference.pixels)?,
    })
}

pub fn run_job(
    reference: &Reference,
    analysis: &Analysis,
    job: Job,
    strength: Strength,
) -> Result<Option<Candidate>, Error> {
    if !reference.jpeg_metadata.is_empty() && !matches!(job, Job::Jpeg { .. }) {
        return Err(Error::Unsupported(
            "JPEG display metadata cannot be discarded by PNG candidates",
        ));
    }
    if analysis.dimensions != reference.pixels.dimensions() {
        return Err(Error::Argument("analysis and reference dimensions differ"));
    }
    let limits = strength.limits();
    match job {
        Job::Jpeg { shape, sampling } => {
            let mut prepared = jpeg::PreparedSampling::new(reference, sampling)?;
            crate::jpeg_search::run_prepared(
                &mut prepared,
                strength,
                |p, scale| jpeg::score_prepared(p, reference, analysis, shape, scale, limits),
                |p, scale, score| {
                    jpeg::encode_prepared(p, reference, analysis, shape, scale, score)
                },
            )
        }
        Job::PngPredictive => predictive::search_bounded(reference, analysis, limits),
        Job::PngLossless => {
            let data = crate::encode_png_lossless(reference)?;
            Ok(Some(Candidate {
                id: "png-lossless".into(),
                parameters: Parameters::PngLossless,
                bytes: data.len(),
                data,
                score: analysis.score(&reference.pixels)?,
            }))
        }
        Job::Png { colors } => {
            let hist = palette::Histogram::new(reference, analysis);
            Ok(
                match run_png_palette_job(reference, analysis, &hist, colors, limits, None)? {
                    PaletteOutcome::Candidate(c) => Some(c),
                    PaletteOutcome::RejectedQuality | PaletteOutcome::PrunedSize => None,
                },
            )
        }
    }
}

enum PaletteOutcome {
    Candidate(Candidate),
    RejectedQuality,
    PrunedSize,
}

fn run_png_palette_job(
    reference: &Reference,
    analysis: &Analysis,
    hist: &palette::Histogram,
    colors: usize,
    limits: Limits,
    best_png_bytes: Option<usize>,
) -> Result<PaletteOutcome, Error> {
    if !(2..=256).contains(&colors) {
        return Err(Error::Argument("palette size must be 2..=256"));
    }
    let palette = palette::train_from_histogram(hist, colors);
    let opaque = if reference.alpha.is_none() {
        crate::analysis::opaque::OpaqueContext::new(analysis, limits)
    } else {
        None
    };
    let pixels = reference.pixels.width() as usize * reference.pixels.height() as usize;
    let use_strip =
        reference.alpha.is_none() && pixels >= 1_048_576 && reference.pixels.height() >= 192;
    struct FastTrial {
        fast_bytes: usize,
        id: String,
        coherence: u32,
        exact: bool,
        indices: Vec<u8>,
        reconstructed: Option<image::RgbImage>,
        score: Score,
    }
    let exact = palette.len() >= 64;
    let mut mapper = palette::Mapper::with_histogram(reference, hist, &palette, exact);
    let mut eval_coherence =
        |coherence: u32, zero_nearest: &mut Option<Vec<u8>>| -> Result<Option<FastTrial>, Error> {
            let (indices, reconstructed, score) = if let Some(context) = &opaque {
                let nearest_ref = if exact && coherence > 0 {
                    zero_nearest.as_deref()
                } else {
                    None
                };
                let Some((indices, score)) =
                    mapper.map_scored_indices(reference, analysis, coherence, nearest_ref, context)
                else {
                    return Ok(None);
                };
                if exact && coherence == 0 {
                    *zero_nearest = Some(indices.clone());
                }
                (indices, None, score)
            } else {
                let (indices, reconstructed) = mapper.map(reference, analysis, coherence);
                let Some(score) = analysis.score_with_limits(&reconstructed, limits)? else {
                    return Ok(None);
                };
                (indices, Some(reconstructed), score)
            };
            let fast_bytes = if use_strip {
                palette::strip_price_indexed(
                    &indices,
                    &palette,
                    reference.pixels.width(),
                    reference.pixels.height(),
                )?
            } else {
                let (fast_data, _) = palette::encode_for_reference_with(
                    reference,
                    &indices,
                    &palette,
                    reconstructed.as_ref(),
                    true,
                )?;
                fast_data.len()
            };
            let id = if exact {
                format!("png-exact-{colors}-coherence-{coherence}")
            } else {
                format!("png-{colors}-coherence-{coherence}")
            };
            Ok(Some(FastTrial {
                fast_bytes,
                id,
                coherence,
                exact,
                indices,
                reconstructed,
                score,
            }))
        };

    let mut zero_nearest: Option<Vec<u8>> = None;
    let Some(base_trial) = eval_coherence(0, &mut zero_nearest)? else {
        return Ok(PaletteOutcome::RejectedQuality);
    };
    let mut best_trial = base_trial;
    if palette.len() > 1 {
        // Higher coherence monotonically shrinks PNG size while increasing color
        // error, so binary-search the largest accepted coherence (2-3 probes).
        const GRID: [u32; 6] = [64, 256, 1024, 2048, 4096, 8192];
        let mut low = 0usize;
        let mut high = GRID.len();
        while low < high {
            let mid = (low + high) / 2;
            if let Some(trial) = eval_coherence(GRID[mid], &mut zero_nearest)? {
                if (trial.fast_bytes, &trial.id) < (best_trial.fast_bytes, &best_trial.id) {
                    best_trial = trial;
                }
                low = mid + 1;
            } else {
                high = mid;
            }
        }
    }
    let trial = best_trial;
    if let Some(best_known) = best_png_bytes {
        let estimated_fast = if use_strip {
            trial
                .fast_bytes
                .saturating_mul(reference.pixels.height() as usize)
                .div_ceil(128)
        } else {
            trial.fast_bytes
        };
        if estimated_fast > best_known.saturating_mul(2) {
            return Ok(PaletteOutcome::PrunedSize);
        }
    }
    let (data, filter) = palette::encode_for_reference_with(
        reference,
        &trial.indices,
        &palette,
        trial.reconstructed.as_ref(),
        false,
    )?;
    Ok(PaletteOutcome::Candidate(Candidate {
        id: trial.id,
        parameters: Parameters::Png {
            colors: palette.len(),
            coherence: trial.coherence,
            filter,
            mapping: if trial.exact {
                "exact_cell_bounds"
            } else {
                "cell_two_nearest"
            },
            palette_order: "as_generated",
        },
        bytes: data.len(),
        data,
        score: trial.score,
    }))
}

/// Reorder only the winning filtered indexed PNG, retaining its exact pixels.
/// No extra quantization or strength adjustment occurs in this finishing pass.
pub fn refine_png_candidates(
    source: &[u8],
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 {
        return Ok(());
    }
    let Some(winner) = best(candidates.iter(), strength) else {
        return Ok(());
    };
    let Parameters::Png {
        colors,
        coherence,
        filter,
        mapping,
        palette_order,
    } = winner.parameters
    else {
        return Ok(());
    };
    if filter == "none" || palette_order != "as_generated" {
        return Ok(());
    }
    let Some(crate::reorder::Reordered {
        data,
        filter: new_filter,
        order,
    }) = crate::reorder::candidate(&winner.data)?
    else {
        return Ok(());
    };
    let mut refined = Candidate {
        id: format!("{}-order-{order}", winner.id),
        parameters: Parameters::Png {
            colors,
            coherence,
            filter: new_filter,
            mapping,
            palette_order: order,
        },
        bytes: data.len(),
        data,
        score: winner.score.clone(),
    };
    preserve_png_density(source, &mut refined)?;
    if refined.bytes < winner.bytes {
        candidates.push(refined);
    }
    Ok(())
}

/// Reuse encoder scan layouts without changing quantization, sampling or pixels.
/// This is lossless relative to the selected JPEG, not to the original image.
pub fn refine_jpeg_candidates(
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 {
        return Ok(());
    }
    let Some(winner) = best(candidates.iter(), strength) else {
        return Ok(());
    };
    let Parameters::Jpeg {
        shape,
        sampling,
        quantization_scale,
        scans_per_component: 4,
        eob_runs: false,
        scan_huffman: false,
        coefficient_rdo_lambda: 0,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
    } = winner.parameters
    else {
        return Ok(());
    };
    let data =
        crate::jpeg::encode_scans(reference, analysis, shape, sampling, quantization_scale, 2)?;
    if data.len() >= winner.bytes {
        return Ok(());
    }
    let before = crate::decode_with_budget(&winner.data, analysis.memory_budget_bytes)?;
    let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
    if decoded.pixels != before.pixels || decoded.alpha != before.alpha {
        return Err(Error::Encoding(
            "JPEG scan finishing changed decoded pixels".into(),
        ));
    }
    let refined = Candidate {
        id: format!("{}-scans2", winner.id),
        parameters: Parameters::Jpeg {
            shape,
            sampling,
            quantization_scale,
            scans_per_component: 2,
            eob_runs: false,
            scan_huffman: false,
            coefficient_rdo_lambda: 0,
            quantization_margin_percent: 100,
            coefficient_rdo_sampling_weights: false,
        },
        bytes: data.len(),
        data,
        score: winner.score.clone(),
    };
    candidates.push(refined);
    Ok(())
}

/// Compare standard progressive EOB-run coding for the selected JPEG only.
/// Coefficients, scan bands, RGB pixels and metadata must remain unchanged.
pub fn refine_jpeg_eob_candidates(
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 {
        return Ok(());
    }
    let Some(winner) = best(candidates.iter(), strength) else {
        return Ok(());
    };
    let Parameters::Jpeg {
        shape,
        sampling,
        quantization_scale,
        scans_per_component,
        eob_runs: false,
        scan_huffman: false,
        coefficient_rdo_lambda: 0,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
    } = winner.parameters
    else {
        return Ok(());
    };
    let data = crate::jpeg::encode_options(
        reference,
        analysis,
        shape,
        sampling,
        quantization_scale,
        scans_per_component,
        true,
    )?;
    if data.len() >= winner.bytes {
        return Ok(());
    }
    let before = crate::decode_with_budget(&winner.data, analysis.memory_budget_bytes)?;
    let after = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
    if before.pixels != after.pixels || before.alpha != after.alpha {
        return Err(Error::Encoding(
            "JPEG EOB-run coding changed decoded pixels".into(),
        ));
    }
    let refined = Candidate {
        id: format!("{}-eob-runs", winner.id),
        parameters: Parameters::Jpeg {
            shape,
            sampling,
            quantization_scale,
            scans_per_component,
            eob_runs: true,
            scan_huffman: false,
            coefficient_rdo_lambda: 0,
            quantization_margin_percent: 100,
            coefficient_rdo_sampling_weights: false,
        },
        bytes: data.len(),
        data,
        score: winner.score.clone(),
    };
    candidates.push(refined);
    Ok(())
}

/// Compare per-scan entropy tables jointly with the supported spectral layouts.
/// Quantized coefficients, display metadata and decoded pixels remain unchanged.
pub fn refine_jpeg_scan_huffman_candidates(
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 {
        return Ok(());
    }
    let Some(winner) = best(candidates.iter(), strength) else {
        return Ok(());
    };
    let Parameters::Jpeg {
        shape,
        sampling,
        quantization_scale,
        scan_huffman: false,
        coefficient_rdo_lambda: 0,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
        ..
    } = winner.parameters
    else {
        return Ok(());
    };
    let mut smallest = winner.bytes;
    let mut refined = None;
    #[cfg(debug_assertions)]
    let mut before = None;
    for scans in [2, 4] {
        let data = crate::jpeg::encode_entropy(
            reference,
            analysis,
            shape,
            sampling,
            quantization_scale,
            scans,
            crate::jpeg::Entropy::ScanHuffman,
        )?;
        if data.len() >= smallest {
            continue;
        }
        #[cfg(debug_assertions)]
        {
            if before.is_none() {
                before = Some(crate::decode_with_budget(
                    &winner.data,
                    analysis.memory_budget_bytes,
                )?);
            }
            let old = before
                .as_ref()
                .ok_or(Error::Encoding("missing JPEG entropy control".into()))?;
            let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
            if old.pixels != decoded.pixels || old.alpha != decoded.alpha {
                return Err(Error::Encoding(
                    "JPEG per-scan Huffman coding changed decoded pixels".into(),
                ));
            }
        }
        smallest = data.len();
        refined = Some(Candidate {
            id: format!(
                "jpeg-{shape:?}-{sampling}-scale{quantization_scale}-scans{scans}-eob-runs-scan-huffman"
            ),
            parameters: Parameters::Jpeg {
                shape,
                sampling,
                quantization_scale,
                scans_per_component: scans,
                eob_runs: true,
                scan_huffman: true,
                coefficient_rdo_lambda: 0,
                quantization_margin_percent: 100,
                coefficient_rdo_sampling_weights: false,
            },
            bytes: data.len(),
            data,
            score: winner.score.clone(),
        });
    }
    if let Some(candidate) = refined {
        candidates.push(candidate);
    }
    Ok(())
}

/// Preserve the two existing coefficient refinements and, only if neither
/// improves the control, try one complementary sampling-weighted model.
/// Accept only smaller complete JPEGs with no component regression.
pub fn refine_jpeg_coefficient_candidates(
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::jpeg_rdo::refine(reference, analysis, candidates, strength)
}

/// Preserve the frozen v81 global-table pass, then conditionally try one coupled
/// repair only for an actual quality rejection smaller than the post-v81 winner.
/// Each pass is independently budget-admitted and appends at most one candidate;
/// the complete existing/v81 prefix remains byte-for-byte unchanged.
pub fn refine_jpeg_table_candidates(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::jpeg_table::refine(source, reference, analysis, candidates, strength)
}

/// Fast table allocation pass for production compression.
pub fn refine_jpeg_table_fast(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::jpeg_table::refine_fast(source, reference, analysis, candidates, strength)
}

/// Independently budgeted chroma pass after the complete unchanged table prefix.
pub fn refine_jpeg_chroma_candidates(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::jpeg_table::refine_chroma(source, reference, analysis, candidates, strength)
}

/// Fast single-pass paired chroma refinement for production compression.
pub fn refine_jpeg_chroma_fast(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::jpeg_table::refine_chroma_fast(source, reference, analysis, candidates, strength)
}

/// One budget-admitted, exact-pixel PNG rate-filter finisher. No quality budget
/// is spent: complete bytes must shrink and current RGBA must match exactly.
pub fn refine_png_rate_candidates(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::png_rate::refine(source, reference, analysis, candidates, strength)
}

/// One budget-admitted libdeflate level12 PNG recompression attempt. Preserve
/// the current selected pixels/Alpha exactly and accept only a smaller file.
pub fn refine_png_deflate_candidates(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::png_deflate::refine(source, reference, analysis, candidates, strength)
}

/// Fast adaptive libdeflate PNG recompression (level 7 for large images, level 9 otherwise).
pub fn refine_png_deflate_fast(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    crate::png_deflate::refine_fast(source, reference, analysis, candidates, strength)
}

/// Preserve input format and fall back to the original rather than lower quality
/// bounds or return a larger file. Strength 0 is a byte-identical pass-through.
pub fn compress(source: &[u8], strength: Strength) -> Result<Candidate, Error> {
    compress_with_budget(source, strength, crate::resources::DEFAULT_MEMORY_BYTES)
}

pub fn compress_with_budget(
    source: &[u8],
    strength: Strength,
    memory_budget_bytes: u64,
) -> Result<Candidate, Error> {
    let reference = crate::decode_with_budget(source, memory_budget_bytes)?;
    let analysis = Analysis::for_reference(&reference, memory_budget_bytes)?;
    let pixels = u64::from(reference.pixels.width()) * u64::from(reference.pixels.height());
    let mut candidates = vec![original(&reference, &analysis, source)?];
    if strength.percent() > 0 {
        if reference.source_format == "jpeg" {
            let limits = strength.limits();
            // Probe 420 first: when 420 passes with comfortable quantization headroom
            // (scale >= 25), wider chroma samplings are much larger and skipped.
            // If 420 passes only at tiny scale (chroma-sensitive edges), skip 422 and
            // also check 444 so quality-constrained photos keep optimal compression.
            let mut skip_422 = false;
            for sampling in [420u16, 422, 444] {
                if sampling == 422 && skip_422 {
                    continue;
                }
                let mut prepared = jpeg::PreparedSampling::new(&reference, sampling)?;
                let mut max_scale = 0u16;
                let mut sampling_accepted = false;
                for shape in [
                    crate::optimize::Shape::Detail,
                    crate::optimize::Shape::Spectrum,
                    crate::optimize::Shape::Standard,
                ] {
                    if let Some(c) = crate::jpeg_search::run_prepared(
                        &mut prepared,
                        strength,
                        |p, scale| {
                            jpeg::score_prepared(p, &reference, &analysis, shape, scale, limits)
                        },
                        |p, scale, score| {
                            jpeg::encode_prepared(p, &reference, &analysis, shape, scale, score)
                        },
                    )? {
                        sampling_accepted = true;
                        if let Parameters::Jpeg {
                            quantization_scale, ..
                        } = c.parameters
                        {
                            max_scale = max_scale.max(quantization_scale);
                        }
                        candidates.push(c);
                    }
                }
                if sampling == 420 && sampling_accepted {
                    if max_scale >= 25 {
                        break;
                    }
                    skip_422 = true;
                } else if sampling == 422 && sampling_accepted && max_scale >= 25 {
                    break;
                }
            }
        } else {
            let limits = strength.limits();
            let hist = palette::Histogram::new(&reference, &analysis);
            // Track only the best generated lossy PNG (encoded at the same fast/mid
            // level) so a tightly pre-compressed source PNG does not prune level-1
            // predictive probes before final libdeflate packing.
            let mut best_png_bytes: Option<usize> = None;
            let mut covered_all_bins = false;
            let palette_sizes: &[usize] = if pixels >= 2_000_000 {
                &[256, 128, 64, 32, 16, 12, 8, 4]
            } else {
                &[256, 128, 64, 32, 16, 12, 8, 6, 5, 4]
            };
            // Probe palettes largest-to-smallest so once a palette size cannot
            // meet quality limits, smaller palettes are pruned immediately.
            for &colors in palette_sizes {
                if colors > hist.bin_count() && covered_all_bins {
                    continue;
                }
                if colors >= hist.bin_count() {
                    covered_all_bins = true;
                }
                match run_png_palette_job(
                    &reference,
                    &analysis,
                    &hist,
                    colors,
                    limits,
                    best_png_bytes,
                )? {
                    PaletteOutcome::Candidate(mut c) => {
                        preserve_png_density(source, &mut c)?;
                        best_png_bytes = Some(best_png_bytes.map_or(c.bytes, |b| b.min(c.bytes)));
                        candidates.push(c);
                    }
                    PaletteOutcome::PrunedSize => {}
                    PaletteOutcome::RejectedQuality => break,
                }
            }
            if let Some(mut c) = predictive::search_bounded_with_target(
                &reference,
                &analysis,
                limits,
                best_png_bytes,
            )? {
                preserve_png_density(source, &mut c)?;
                best_png_bytes = Some(best_png_bytes.map_or(c.bytes, |b| b.min(c.bytes)));
                candidates.push(c);
            }
            // Defer full lossless re-encode unless the image is small, low-color,
            // or lossy candidates did not beat the input file.
            if (pixels <= 262_144
                || hist.bin_count() <= 256
                || best_png_bytes.is_none_or(|b| b >= source.len()))
                && let Some(mut c) = run_job(&reference, &analysis, Job::PngLossless, strength)?
            {
                preserve_png_density(source, &mut c)?;
                candidates.push(c);
            }
        }
    }
    if reference.source_format == "jpeg" {
        refine_jpeg_scan_huffman_candidates(&reference, &analysis, &mut candidates, strength)?;
        refine_jpeg_table_fast(source, &reference, &analysis, &mut candidates, strength)?;
        let winner_sampling = best(candidates.iter(), strength).and_then(|c| match &c.parameters {
            Parameters::Jpeg { sampling, .. } => Some(*sampling),
            Parameters::JpegTableAllocated { control, .. } => match control.as_ref() {
                Parameters::Jpeg { sampling, .. } => Some(*sampling),
                _ => None,
            },
            _ => None,
        });
        if winner_sampling != Some(420) && pixels <= 2_500_000 {
            refine_jpeg_chroma_fast(source, &reference, &analysis, &mut candidates, strength)?;
        }
    } else {
        refine_png_candidates(source, &mut candidates, strength)?;
        if pixels <= 300_000 {
            refine_png_rate_candidates(source, &reference, &analysis, &mut candidates, strength)?;
        }
        refine_png_deflate_fast(source, &reference, &analysis, &mut candidates, strength)?;
    }
    let winner = best(candidates.iter(), strength)
        .ok_or(Error::Encoding("no quality-safe candidate".into()))?;
    let index = candidates
        .iter()
        .position(|c| std::ptr::eq(c, winner))
        .ok_or(Error::Encoding("missing selected candidate".into()))?;
    let selected = candidates.swap_remove(index);
    verify_transparency(&reference, &selected.data, memory_budget_bytes)?;
    Ok(selected)
}

/// Verify the encoded result, not only the in-memory quantization model.
pub fn verify_transparency(
    reference: &Reference,
    encoded: &[u8],
    memory_budget_bytes: u64,
) -> Result<(), Error> {
    let Some(alpha) = &reference.alpha else {
        return Ok(());
    };
    let decoded = crate::decode_with_budget(encoded, memory_budget_bytes)?;
    if decoded.pixels.dimensions() != reference.pixels.dimensions()
        || decoded.alpha != reference.alpha
    {
        return Err(Error::Encoding(
            "encoded PNG changed per-pixel transparency".into(),
        ));
    }
    for ((before, after), a) in reference
        .pixels
        .pixels()
        .zip(decoded.pixels.pixels())
        .zip(alpha.pixels())
    {
        if a[0] == 0 && before != after {
            return Err(Error::Encoding("encoded PNG changed hidden RGB".into()));
        }
    }
    Ok(())
}

// Keep physical pixel density (DPI) for PNG/Word use without re-encoding pixels.
// Color profiles require explicit color management and remain a separate input check.
pub fn preserve_png_density(source: &[u8], candidate: &mut Candidate) -> Result<(), Error> {
    if candidate.format() != "png" || matches!(&candidate.parameters, Parameters::Original { .. }) {
        return Ok(());
    }
    fn density_range(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return None;
        }
        let mut pos = 8usize;
        while pos.checked_add(12)? <= bytes.len() {
            let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
            let end = pos.checked_add(12)?.checked_add(len)?;
            if end > bytes.len() {
                return None;
            }
            if len == 9 && &bytes[pos + 4..pos + 8] == b"pHYs" {
                return Some(pos..end);
            }
            if &bytes[pos + 4..pos + 8] == b"IEND" {
                return None;
            }
            pos = end;
        }
        None
    }
    if let Some(range) = density_range(source) {
        if candidate.data.len() < 33 {
            return Err(Error::Encoding("truncated PNG candidate".into()));
        }
        let output_range = density_range(&candidate.data).unwrap_or(33..33);
        candidate
            .data
            .splice(output_range, source[range].iter().copied());
        candidate.bytes = candidate.data.len();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_strength_defaults_and_limits_are_monotonic() {
        assert_eq!(Strength::default().percent(), 50);
        assert!(Strength::new(101).is_err());
        for p in 0..100 {
            assert!(
                Strength::new(p).unwrap().limits().pixel
                    <= Strength::new(p + 1).unwrap().limits().pixel
            );
        }
    }
    #[test]
    fn zero_strength_returns_identical_input_and_preserves_format() {
        let reference = Reference {
            pixels: image::RgbImage::from_pixel(17, 13, image::Rgb([40, 80, 160])),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        for data in [
            crate::encode_png_lossless(&reference).unwrap(),
            crate::encode_jpeg_baseline(&reference, 80).unwrap(),
        ] {
            assert_eq!(
                compress(&data, Strength::new(0).unwrap()).unwrap().data,
                data
            );
        }
    }
    #[test]
    fn png_density_is_preserved_and_copy_is_idempotent() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_pixel_dims(Some(png::PixelDimensions {
                xppu: 3780,
                yppu: 3780,
                unit: png::Unit::Meter,
            }));
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[255, 0, 0, 255, 0, 100, 200, 255])
                .unwrap();
            writer.finish().unwrap();
        }
        let reference = crate::decode_baseline(&bytes).unwrap();
        let analysis = Analysis::new(&reference.pixels).unwrap();
        let mut c = run_job(&reference, &analysis, Job::PngLossless, Strength::default())
            .unwrap()
            .unwrap();
        preserve_png_density(&bytes, &mut c).unwrap();
        let once = c.data.clone();
        preserve_png_density(&bytes, &mut c).unwrap();
        assert_eq!(c.data, once);
        let reader = png::Decoder::new(std::io::Cursor::new(&c.data))
            .read_info()
            .unwrap();
        assert_eq!(reader.info().pixel_dims.unwrap().xppu, 3780);
        assert_eq!(
            crate::decode_baseline(&c.data).unwrap().pixels,
            reference.pixels
        );
    }
    #[test]
    fn default_png_cannot_grow_or_exceed_frozen_error_limits() {
        let reference = Reference {
            pixels: image::RgbImage::from_fn(23, 17, |x, y| {
                image::Rgb([(x * 11) as u8, (y * 13) as u8, ((x + y) * 5) as u8])
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let source = crate::encode_png_lossless(&reference).unwrap();
        let selected = compress(&source, Strength::default()).unwrap();
        assert!(selected.bytes <= source.len());
        assert!(Strength::default().limits().accepts(&selected.score));
        assert_eq!(selected.format(), "png");
    }
}
