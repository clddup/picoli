//! Shared candidate generation. Native callers may parallelize independent jobs.
pub use crate::jpeg::Shape;
use crate::{
    Error, Reference,
    analysis::{Analysis, Score},
    jpeg, palette, predictive,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub enum Job {
    PngLossless,
    PngPredictive,
    Png { colors: usize },
    Jpeg { shape: Shape, sampling: u16 },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum Parameters {
    Original {
        source_format: &'static str,
    },
    Png {
        /// RGB representative colors; transparent RGBA pair count can differ.
        colors: usize,
        coherence: u32,
        filter: &'static str,
        mapping: &'static str,
        palette_order: &'static str,
    },
    PngLossless,
    PngRateFiltered {
        policy: &'static str,
        groups: usize,
        zlib_bytes: usize,
        model_zlib_bytes: usize,
    },
    PngDeflatePacked {
        policy: &'static str,
        zlib_bytes: usize,
    },
    PngPredictive {
        step: u8,
        filter: &'static str,
    },
    JpegChromaRefined {
        control: Box<Parameters>,
        policy: &'static str,
        commits: usize,
        compensated_commits: usize,
    },
    JpegTableAllocated {
        /// Exact generated control provenance; scale/lambda are not new table values.
        control: Box<Parameters>,
        /// Literal natural-order DQT, including the unchanged DC quantizer.
        luma_quantization_table: Vec<u16>,
        rewritten_frequencies: Vec<usize>,
        scans_per_component: u8,
        eob_runs: bool,
        scan_huffman: bool,
        model: &'static str,
        allocation: &'static str,
        exchanges: usize,
    },
    Jpeg {
        shape: Shape,
        sampling: u16,
        quantization_scale: u16,
        /// Progressive scans per component; only 2 or 4 are supported.
        scans_per_component: u8,
        eob_runs: bool,
        scan_huffman: bool,
        /// Zero keeps the original coefficient quantizer.
        coefficient_rdo_lambda: u32,
        /// Percent of the base DQT values;100 leaves them unchanged.
        quantization_margin_percent: u16,
        /// Derive component lambdas from approximate color weights and sampling area.
        coefficient_rdo_sampling_weights: bool,
    },
}

#[derive(Debug, Serialize)]
pub struct Candidate {
    pub id: String,
    pub parameters: Parameters,
    pub bytes: usize,
    pub score: Score,
    #[serde(skip)]
    pub data: Vec<u8>,
}

impl Candidate {
    pub fn format(&self) -> &'static str {
        match self.parameters {
            Parameters::Jpeg { .. }
            | Parameters::JpegTableAllocated { .. }
            | Parameters::JpegChromaRefined { .. } => "jpeg",
            Parameters::Original { source_format } => source_format,
            _ => "png",
        }
    }
}

pub fn jobs(format: &str) -> Result<Vec<Job>, Error> {
    match format {
        "png" => Ok([Job::PngLossless, Job::PngPredictive]
            .into_iter()
            .chain(
                [4, 5, 6, 8, 12, 16, 32, 64, 128, 256]
                    .into_iter()
                    .map(|colors| Job::Png { colors }),
            )
            .collect()),
        "jpeg" => Ok([444u16, 422, 420]
            .into_iter()
            .flat_map(|sampling| {
                [Shape::Standard, Shape::Detail, Shape::Spectrum]
                    .into_iter()
                    .map(move |shape| Job::Jpeg { shape, sampling })
            })
            .collect()),
        _ => Err(Error::Argument("format must be jpeg or png")),
    }
}

pub fn prefer(a: &Candidate, b: &Candidate, budget: usize) -> bool {
    let a_fits = a.bytes <= budget;
    let b_fits = b.bytes <= budget;
    if a_fits != b_fits {
        return a_fits;
    }
    let ordering = if a_fits {
        a.score
            .loss
            .total_cmp(&b.score.loss)
            .then(a.bytes.cmp(&b.bytes))
    } else {
        a.bytes
            .cmp(&b.bytes)
            .then(a.score.loss.total_cmp(&b.score.loss))
    };
    ordering.then(a.id.cmp(&b.id)).is_lt()
}

pub fn best<'a>(
    candidates: impl Iterator<Item = &'a Candidate>,
    budget: usize,
) -> Option<&'a Candidate> {
    candidates.reduce(|a, b| if prefer(b, a, budget) { b } else { a })
}

pub fn run_job(
    reference: &Reference,
    analysis: &Analysis,
    job: Job,
    budget: usize,
) -> Result<Candidate, Error> {
    if budget == 0 {
        return Err(Error::Argument("byte budget must be positive"));
    }
    if !reference.jpeg_metadata.is_empty() && !matches!(job, Job::Jpeg { .. }) {
        return Err(Error::Unsupported(
            "JPEG display metadata cannot be discarded by PNG candidates",
        ));
    }
    if analysis.dimensions != reference.pixels.dimensions() {
        return Err(Error::Argument("analysis and reference dimensions differ"));
    }
    match job {
        Job::Jpeg { shape, sampling } => jpeg::search(reference, analysis, shape, sampling, budget),
        Job::PngPredictive => {
            let mut best: Option<Candidate> = None;
            for &step in predictive::steps(reference) {
                if let Some(candidate) = predictive::candidate(reference, analysis, step, |_| true)?
                    && best
                        .as_ref()
                        .is_none_or(|old| prefer(&candidate, old, budget))
                {
                    best = Some(candidate);
                }
            }
            best.ok_or(Error::Encoding("no predictive PNG candidates".into()))
        }
        Job::PngLossless => {
            let data = crate::encode_png_lossless(reference)?;
            Ok(Candidate {
                id: "png-lossless".into(),
                parameters: Parameters::PngLossless,
                score: analysis.score(&reference.pixels)?,
                bytes: data.len(),
                data,
            })
        }
        Job::Png { colors } => {
            if !(2..=256).contains(&colors) {
                return Err(Error::Argument("palette size must be 2..=256"));
            }
            let palette = palette::train(reference, analysis, colors);
            let mut best: Option<Candidate> = None;
            // Higher coherence penalizes index transitions only when the pixel's
            // weighted color error is small enough. Strong edges cost more.
            for coherence in [0, 64, 256, 1024, 2048, 4096, 8192] {
                let (indices, reconstructed) =
                    palette::map(reference, analysis, &palette, coherence);
                let (data, filter) =
                    palette::encode_for_reference(reference, &indices, &palette, &reconstructed)?;
                let candidate = Candidate {
                    id: format!("png-{colors}-coherence-{coherence}"),
                    parameters: Parameters::Png {
                        colors: palette.len(),
                        coherence,
                        filter,
                        mapping: "cell_two_nearest",
                        palette_order: "as_generated",
                    },
                    score: analysis.score(&reconstructed)?,
                    bytes: data.len(),
                    data,
                };
                if best
                    .as_ref()
                    .is_none_or(|old| prefer(&candidate, old, budget))
                {
                    best = Some(candidate);
                }
            }
            best.ok_or(Error::Encoding("no PNG candidates".into()))
        }
    }
}

/// Same jobs and ordering as the native CLI, evaluated sequentially for WASM.
pub fn compress(reference: &Reference, format: &str, budget: usize) -> Result<Candidate, Error> {
    let analysis = Analysis::for_reference(reference, crate::resources::DEFAULT_MEMORY_BYTES)?;
    let mut result: Option<Candidate> = None;
    for job in jobs(format)? {
        let candidate = run_job(reference, &analysis, job, budget)?;
        if result
            .as_ref()
            .is_none_or(|old| prefer(&candidate, old, budget))
        {
            result = Some(candidate);
        }
    }
    result.ok_or(Error::Encoding("no optimization jobs".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_palettes_roundtrip_for_odd_widths_and_all_index_depths() {
        let image = image::RgbImage::from_fn(19, 11, |x, y| {
            image::Rgb([
                ((x * 37) % 256) as u8,
                ((y * 71) % 256) as u8,
                ((x + y) * 7) as u8,
            ])
        });
        let reference = Reference {
            pixels: image,
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        for colors in [2, 4, 16, 32] {
            let palette = palette::train(&reference, &analysis, colors);
            let (indices, mapped) = palette::map(&reference, &analysis, &palette, 64);
            let (bytes, _) = palette::encode(&indices, &palette, 19, 11).unwrap();
            let decoded = crate::decode_baseline(&bytes).unwrap();
            assert_eq!(decoded.pixels, mapped);
        }
    }
    #[test]
    fn small_palette_keeps_rare_saturated_colors() {
        let source = Reference {
            pixels: image::RgbImage::from_fn(64, 64, |x, y| {
                image::Rgb(if x == 10 && y == 10 {
                    [255, 0, 0]
                } else if x == 20 {
                    [0; 3]
                } else if y > 50 {
                    [210, 190, 140]
                } else {
                    [255; 3]
                })
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&source.pixels).unwrap();
        let colors = palette::train(&source, &analysis, 4);
        assert!(colors.contains(&[255, 0, 0]));
    }
    #[test]
    fn palette_is_deterministic_and_does_not_mutate_source() {
        let source = Reference {
            pixels: image::RgbImage::from_fn(17, 13, |x, _| {
                image::Rgb(if x < 8 { [255, 0, 0] } else { [255; 3] })
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let copy = source.pixels.clone();
        let analysis = Analysis::new(&source.pixels).unwrap();
        let job = Job::Png { colors: 4 };
        let a = run_job(&source, &analysis, job, 65000).unwrap();
        let b = run_job(&source, &analysis, job, 65000).unwrap();
        assert_eq!(a.data, b.data);
        let other_job = run_job(&source, &analysis, Job::Png { colors: 8 }, 65000).unwrap();
        assert_ne!(a.id, other_job.id); // Distinct job files even if palettes collapse.
        assert_eq!(a.score.loss, 0.0);
        assert_eq!(source.pixels, copy);
        assert!(run_job(&source, &analysis, job, 0).is_err());
    }
}
