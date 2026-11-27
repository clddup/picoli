//! Constrained table shapes, actual JPEG encoding, and byte-budget search.
use crate::{
    Error, Reference,
    analysis::Analysis,
    optimize::{Candidate, Parameters, prefer},
};
use jpeg_encoder::{
    ChromaSubsamplingMethod, ColorType, Encoder, QuantizationTableType, SamplingFactor,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Standard,
    Detail,
    Spectrum,
}

const LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const CHROMA: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

fn tables(analysis: &Analysis, shape: Shape, scale: u16) -> ([u16; 64], [u16; 64]) {
    let scale = u32::from(scale);
    let mean = analysis.frequency_energy[1..].iter().sum::<u64>() / 63;
    let mut luma = [0; 64];
    let mut chroma = [0; 64];
    for i in 0..64 {
        let base = match shape {
            Shape::Standard => u32::from(LUMA[i]),
            Shape::Detail => {
                if i == 0 {
                    12
                } else {
                    12 + 2 * ((i / 8 + i % 8) as u32)
                }
            }
            Shape::Spectrum => {
                if i == 0 {
                    12
                } else {
                    (24 * (mean + 1).isqrt() / (analysis.frequency_energy[i] + 1).isqrt().max(1))
                        .clamp(8, 128) as u32
                }
            }
        };
        luma[i] = ((base * scale + 50) / 100).clamp(1, 255) as u16;
        chroma[i] = ((u32::from(CHROMA[i]) * scale + 50) / 100).clamp(1, 255) as u16;
    }
    (luma, chroma)
}

fn encode(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
) -> Result<Vec<u8>, Error> {
    encode_scans(reference, analysis, shape, sampling, scale, 4)
}

pub(crate) fn encode_scans(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
    scans: u8,
) -> Result<Vec<u8>, Error> {
    encode_options(reference, analysis, shape, sampling, scale, scans, false)
}

pub(crate) fn encode_options(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
    scans: u8,
    eob_runs: bool,
) -> Result<Vec<u8>, Error> {
    encode_entropy(
        reference,
        analysis,
        shape,
        sampling,
        scale,
        scans,
        if eob_runs {
            Entropy::EobRuns
        } else {
            Entropy::IndependentBlocks
        },
    )
}

#[derive(Clone, Copy)]
pub(crate) enum Entropy {
    IndependentBlocks,
    EobRuns,
    ScanHuffman,
}

#[derive(Clone, Copy)]
pub(crate) struct Encoding {
    pub shape: Shape,
    pub sampling: u16,
    pub scale: u16,
    pub scans: u8,
    pub entropy: Entropy,
    pub q_percent: u16,
    pub rdo_lambda: u32,
    pub weighted_sampling: bool,
}

pub(crate) fn encode_entropy(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
    scans: u8,
    entropy: Entropy,
) -> Result<Vec<u8>, Error> {
    encode_config(
        reference,
        analysis,
        Encoding {
            shape,
            sampling,
            scale,
            scans,
            entropy,
            q_percent: 100,
            rdo_lambda: 0,
            weighted_sampling: false,
        },
    )
}

pub(crate) fn encode_config(
    reference: &Reference,
    analysis: &Analysis,
    options: Encoding,
) -> Result<Vec<u8>, Error> {
    let Encoding {
        shape,
        sampling,
        scale,
        scans,
        entropy,
        q_percent,
        rdo_lambda,
        weighted_sampling,
    } = options;
    if !(1..=100).contains(&q_percent) {
        return Err(Error::Argument(
            "JPEG quantizer margin must be1..=100 percent",
        ));
    }
    if reference.alpha.is_some() {
        return Err(Error::Unsupported(
            "JPEG cannot preserve transparency without an explicit matte",
        ));
    }
    let width = u16::try_from(reference.pixels.width())
        .map_err(|_| Error::Limit("JPEG width exceeds 65535"))?;
    let height = u16::try_from(reference.pixels.height())
        .map_err(|_| Error::Limit("JPEG height exceeds 65535"))?;
    let (mut luma, mut chroma) = tables(analysis, shape, scale);
    if q_percent != 100 {
        for value in luma.iter_mut().chain(chroma.iter_mut()) {
            *value = ((u32::from(*value) * u32::from(q_percent) + 50) / 100).clamp(1, 255) as u16;
        }
    }
    let mut bytes = Vec::new();
    // Custom tables are already scaled; the encoder's quality setting is unused.
    let mut encoder = Encoder::new(&mut bytes, 100);
    encoder.set_quantization_tables(
        QuantizationTableType::Custom(Box::new(luma)),
        QuantizationTableType::Custom(Box::new(chroma)),
    );
    encoder.set_sampling_factor(match sampling {
        444 => SamplingFactor::R_4_4_4,
        422 => SamplingFactor::R_4_2_2,
        420 => SamplingFactor::R_4_2_0,
        _ => return Err(Error::Argument("JPEG sampling must be 444/422/420")),
    });
    encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_progressive_eob_runs(!matches!(entropy, Entropy::IndependentBlocks));
    encoder.set_progressive_scan_huffman(matches!(entropy, Entropy::ScanHuffman));
    encoder.set_experimental_ac_rdo_lambda(rdo_lambda);
    encoder.set_experimental_ac_rdo_sampling_weights(weighted_sampling);
    match scans {
        2 | 4 => encoder.set_progressive_scans(scans),
        _ => return Err(Error::Argument("JPEG finishing scans must be 2/4")),
    }
    encoder
        .encode(reference.pixels.as_raw(), width, height, ColorType::Rgb)
        .map_err(|e| Error::Encoding(e.to_string()))?;
    reference.jpeg_metadata.attach(bytes)
}

pub(crate) fn candidate(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
) -> Result<Candidate, Error> {
    candidate_impl(reference, analysis, shape, sampling, scale, None)?
        .ok_or_else(|| Error::Encoding("unbounded JPEG candidate unexpectedly rejected".into()))
}

pub(crate) struct PreparedSampling {
    sampling: u16,
    fdct: jpeg_encoder::PreparedFdct,
    reconstructor: zune_jpeg::StreamReconstructor,
    coeffs: [Vec<i16>; 3],
    last_quantized: Option<(Shape, u16)>,
}

impl PreparedSampling {
    pub(crate) fn new(reference: &Reference, sampling: u16) -> Result<Self, Error> {
        if reference.alpha.is_some() {
            return Err(Error::Unsupported(
                "JPEG cannot preserve transparency without an explicit matte",
            ));
        }
        let width = u16::try_from(reference.pixels.width())
            .map_err(|_| Error::Limit("JPEG width exceeds 65535"))?;
        let height = u16::try_from(reference.pixels.height())
            .map_err(|_| Error::Limit("JPEG height exceeds 65535"))?;
        let factor = match sampling {
            444 => SamplingFactor::R_4_4_4,
            422 => SamplingFactor::R_4_2_2,
            420 => SamplingFactor::R_4_2_0,
            _ => return Err(Error::Argument("JPEG sampling must be 444/422/420")),
        };
        let fdct = jpeg_encoder::PreparedFdct::new(
            reference.pixels.as_raw(),
            width,
            height,
            factor,
            ChromaSubsamplingMethod::Average,
        )
        .map_err(|e| Error::Encoding(e.to_string()))?;
        let reconstructor = zune_jpeg::StreamReconstructor::new(
            fdct.width,
            fdct.height,
            fdct.h_max as u8,
            fdct.v_max as u8,
        )
        .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
        let coeffs = fdct.allocate_coeffs();
        Ok(Self {
            sampling,
            fdct,
            reconstructor,
            coeffs,
            last_quantized: None,
        })
    }

    fn ensure_quantized(
        &mut self,
        analysis: &Analysis,
        shape: Shape,
        scale: u16,
    ) -> ([u16; 64], [u16; 64]) {
        let (luma, chroma) = tables(analysis, shape, scale);
        if self.last_quantized != Some((shape, scale)) {
            self.fdct.quantize_into(&luma, &chroma, &mut self.coeffs);
            self.last_quantized = Some((shape, scale));
        }
        (luma, chroma)
    }
}

pub(crate) fn score_prepared(
    prepared: &mut PreparedSampling,
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    scale: u16,
    limits: crate::quality::Limits,
) -> Result<Option<crate::analysis::Score>, Error> {
    let Some(context) = crate::analysis::opaque::OpaqueContext::new(analysis, limits) else {
        return Ok(candidate_bounded(
            reference,
            analysis,
            shape,
            prepared.sampling,
            scale,
            limits,
        )?
        .map(|c| c.score));
    };
    let (luma, chroma) = tables(analysis, shape, scale);
    let quantizers = jpeg_encoder::PreparedQuantizers::new(&luma, &chroma);
    // Early-rejected probes only quantize the prefix of MCU rows needed to fail.
    prepared.last_quantized = None;

    let mut scorer = context.start();
    let mut pixel_index = 0usize;
    let fdct = &prepared.fdct;
    let accepted = prepared
        .reconstructor
        .reconstruct_rows(
            &luma,
            &chroma,
            &mut prepared.coeffs,
            |mcu_row, coeffs| {
                fdct.quantize_mcu_row(mcu_row, &quantizers, coeffs);
            },
            |row_rgb| {
                if !scorer.push_rgb_rows(pixel_index, row_rgb) {
                    return false;
                }
                pixel_index += row_rgb.len() / 3;
                true
            },
        )
        .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
    if !accepted {
        return Ok(None);
    }
    prepared.last_quantized = Some((shape, scale));
    let score = scorer.finish();
    debug_assert_eq!(
        pixel_index,
        usize::from(prepared.fdct.width) * usize::from(prepared.fdct.height)
    );
    Ok(Some(score))
}

pub(crate) fn encode_prepared(
    prepared: &mut PreparedSampling,
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    scale: u16,
    score: crate::analysis::Score,
) -> Result<Candidate, Error> {
    let (luma, chroma) = prepared.ensure_quantized(analysis, shape, scale);
    let components = [
        jpeg_encoder::QuantizedComponent {
            horizontal_sampling_factor: prepared.fdct.components[0].h_sample,
            vertical_sampling_factor: prepared.fdct.components[0].v_sample,
            quantization_table: &luma,
            block_width: prepared.fdct.components[0].block_width,
            block_height: prepared.fdct.components[0].block_height,
            coefficients: &prepared.coeffs[0],
        },
        jpeg_encoder::QuantizedComponent {
            horizontal_sampling_factor: prepared.fdct.components[1].h_sample,
            vertical_sampling_factor: prepared.fdct.components[1].v_sample,
            quantization_table: &chroma,
            block_width: prepared.fdct.components[1].block_width,
            block_height: prepared.fdct.components[1].block_height,
            coefficients: &prepared.coeffs[1],
        },
        jpeg_encoder::QuantizedComponent {
            horizontal_sampling_factor: prepared.fdct.components[2].h_sample,
            vertical_sampling_factor: prepared.fdct.components[2].v_sample,
            quantization_table: &chroma,
            block_width: prepared.fdct.components[2].block_width,
            block_height: prepared.fdct.components[2].block_height,
            coefficients: &prepared.coeffs[2],
        },
    ];
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 100);
    encoder.set_progressive_scans(4);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_progressive_eob_runs(false);
    encoder.set_progressive_scan_huffman(false);
    let data = match encoder.encode_quantized(jpeg_encoder::QuantizedImage {
        width: prepared.fdct.width,
        height: prepared.fdct.height,
        color_type: jpeg_encoder::JpegColorType::Ycbcr,
        components: &components,
    }) {
        Ok(()) => reference.jpeg_metadata.attach(bytes)?,
        // Rare 8-bit DQT/category overflows fall back to the full encoder path.
        Err(_) => encode(reference, analysis, shape, prepared.sampling, scale)?,
    };

    Ok(Candidate {
        id: format!("jpeg-{shape:?}-{}-scale{scale}", prepared.sampling),
        bytes: data.len(),
        data,
        score,
        parameters: Parameters::Jpeg {
            shape,
            sampling: prepared.sampling,
            quantization_scale: scale,
            scans_per_component: 4,
            eob_runs: false,
            scan_huffman: false,
            coefficient_rdo_lambda: 0,
            quantization_margin_percent: 100,
            coefficient_rdo_sampling_weights: false,
        },
    })
}

#[cfg(test)]
pub(crate) fn candidate_prepared(
    prepared: &mut PreparedSampling,
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    scale: u16,
    limits: crate::quality::Limits,
) -> Result<Option<Candidate>, Error> {
    let Some(score) = score_prepared(prepared, reference, analysis, shape, scale, limits)? else {
        return Ok(None);
    };
    Ok(Some(encode_prepared(
        prepared, reference, analysis, shape, scale, score,
    )?))
}

pub(crate) fn candidate_bounded(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
    limits: crate::quality::Limits,
) -> Result<Option<Candidate>, Error> {
    candidate_impl(reference, analysis, shape, sampling, scale, Some(limits))
}

fn candidate_impl(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    scale: u16,
    limits: Option<crate::quality::Limits>,
) -> Result<Option<Candidate>, Error> {
    let data = encode(reference, analysis, shape, sampling, scale)?;
    let reconstructed = crate::decode_with_budget(&data, analysis.memory_budget_bytes)?;
    let score = match limits {
        Some(limits) => analysis.score_with_limits(&reconstructed.pixels, limits)?,
        None => Some(analysis.score(&reconstructed.pixels)?),
    };
    let Some(score) = score else {
        return Ok(None);
    };
    Ok(Some(Candidate {
        id: format!("jpeg-{shape:?}-{sampling}-scale{scale}"),
        bytes: data.len(),
        data,
        score,
        parameters: Parameters::Jpeg {
            shape,
            sampling,
            quantization_scale: scale,
            scans_per_component: 4,
            eob_runs: false,
            scan_huffman: false,
            coefficient_rdo_lambda: 0,
            quantization_margin_percent: 100,
            coefficient_rdo_sampling_weights: false,
        },
    }))
}

pub(crate) fn search(
    reference: &Reference,
    analysis: &Analysis,
    shape: Shape,
    sampling: u16,
    budget: usize,
) -> Result<Candidate, Error> {
    // Search integer quantization scales directly. Integer quality values leave
    // large rate gaps at low quality; direct scale search uses the byte budget
    // more closely. Probe the local neighborhood because sizes can wobble.
    let (mut low, mut high, mut best_scale) = (1i32, 5000i32, 5000i32);
    while low <= high {
        let mid = (low + high) / 2;
        let bytes = encode(reference, analysis, shape, sampling, mid as u16)?;
        if bytes.len() <= budget {
            best_scale = mid;
            high = mid - 1;
        } else {
            low = mid + 1;
        }
    }
    let mut best: Option<Candidate> = None;
    let start = (best_scale - 8).max(1);
    let end = (best_scale + 8).min(5000);
    for scale in start..=end {
        let candidate = candidate(reference, analysis, shape, sampling, scale as u16)?;
        if best
            .as_ref()
            .is_none_or(|old| prefer(&candidate, old, budget))
        {
            best = Some(candidate);
        }
    }
    best.ok_or(Error::Encoding("no JPEG candidates".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn table_search_changes_actual_dqt_and_preserves_dimensions() {
        let reference = Reference {
            pixels: image::RgbImage::from_fn(33, 17, |x, y| {
                image::Rgb([((x * 71) % 256) as u8, ((y * 53) % 256) as u8, 180])
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        let standard = encode(&reference, &analysis, Shape::Standard, 444, 60).unwrap();
        let detail = encode(&reference, &analysis, Shape::Detail, 444, 60).unwrap();
        assert_ne!(
            tables(&analysis, Shape::Standard, 60).0,
            tables(&analysis, Shape::Detail, 60).0
        );
        assert_ne!(standard, detail);
        for sampling in [444, 422, 420] {
            let bytes = encode(&reference, &analysis, Shape::Detail, sampling, 60).unwrap();
            assert_eq!(
                crate::decode_baseline(&bytes).unwrap().pixels.dimensions(),
                (33, 17)
            );
        }
    }

    #[test]
    fn prepared_sampling_matches_candidate_bounded_bit_for_bit() {
        for (width, height) in [(1, 1), (7, 19), (17, 13), (35, 17)] {
            let reference = Reference {
                pixels: image::RgbImage::from_fn(width, height, |x, y| {
                    image::Rgb([
                        ((x * 73 + y * 29) % 256) as u8,
                        ((x * 41 + y * 97) % 256) as u8,
                        ((x * 17 + y * 53 + 120) % 256) as u8,
                    ])
                }),
                alpha: None,
                source_format: "jpeg",
                jpeg_metadata: Default::default(),
            };
            let analysis = Analysis::new(&reference.pixels).unwrap();
            let high_limits = crate::quality::Strength::new(95).unwrap().limits();
            let strict_limits = crate::quality::Strength::new(20).unwrap().limits();
            for sampling in [444, 422, 420] {
                let mut prepared = PreparedSampling::new(&reference, sampling).unwrap();
                for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
                    for limits in [high_limits, strict_limits] {
                        for scale in [25, 80, 200] {
                            let actual = candidate_prepared(
                                &mut prepared,
                                &reference,
                                &analysis,
                                shape,
                                scale,
                                limits,
                            )
                            .unwrap();
                            let expected = candidate_bounded(
                                &reference, &analysis, shape, sampling, scale, limits,
                            )
                            .unwrap();
                            match (actual, expected) {
                                (None, None) => {}
                                (Some(a), Some(e)) => {
                                    assert_eq!(a.id, e.id);
                                    assert_eq!(
                                        format!("{:?}", a.parameters),
                                        format!("{:?}", e.parameters)
                                    );
                                    assert_eq!(a.data, e.data);
                                    assert_eq!(
                                        a.score.weighted_pixel_error.to_bits(),
                                        e.score.weighted_pixel_error.to_bits()
                                    );
                                    assert_eq!(
                                        a.score.weighted_edge_error.to_bits(),
                                        e.score.weighted_edge_error.to_bits()
                                    );
                                    assert_eq!(
                                        a.score.worst_tile_error.to_bits(),
                                        e.score.worst_tile_error.to_bits()
                                    );
                                }
                                (a, e) => panic!(
                                    "mismatch for {width}x{height} sampling={sampling} {shape:?} scale={scale}: actual={} expected={}",
                                    a.is_some(),
                                    e.is_some()
                                ),
                            }
                        }
                    }
                }
            }
        }
    }
}
