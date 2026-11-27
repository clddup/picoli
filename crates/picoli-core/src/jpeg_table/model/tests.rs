use super::*;
use crate::jpeg_table::coefficient_io;
use image::Rgb;
use jpeg_encoder::{
    ChromaSubsamplingMethod, ColorType, Encoder, QuantizationTableType, SamplingFactor,
};
fn reference(width: u32, height: u32) -> Reference {
    Reference {
        pixels: RgbImage::from_fn(width, height, |x, y| {
            Rgb([
                (x * 37 + y * 13) as u8,
                (x * 17 + y * 53) as u8,
                (x * y * 7 + 19) as u8,
            ])
        }),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    }
}
fn encoded(r: &Reference, sampling: SamplingFactor, q: u16, lambda: u32) -> Vec<u8> {
    let mut data = Vec::new();
    let mut e = Encoder::new(&mut data, 100);
    e.set_sampling_factor(sampling);
    e.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([q; 64])),
        QuantizationTableType::Custom(Box::new([q; 64])),
    );
    e.set_progressive_scans(4);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    e.set_experimental_ac_rdo_lambda(lambda);
    e.encode(
        r.pixels.as_raw(),
        r.pixels.width() as u16,
        r.pixels.height() as u16,
        ColorType::Rgb,
    )
    .unwrap();
    data
}
#[test]
fn reused_scalar_fdct_and_color_extraction_match_real_codec_on_odd_thin_and_all_sampling_layouts() {
    for (width, height) in [(1, 19), (19, 1), (19, 25), (16, 16)] {
        let r = reference(width, height);
        let raw = raw_luma(&r).unwrap();
        let cols = (width as usize).div_ceil(8);
        for sampling in [
            SamplingFactor::R_4_4_4,
            SamplingFactor::R_4_2_2,
            SamplingFactor::R_4_2_0,
        ] {
            let frame = coefficient_io::capture(&encoded(&r, sampling, 1, 0), 1024 * 1024 * 1024)
                .unwrap()
                .frame;
            for (index, block) in raw.iter().enumerate() {
                let first = (index / cols * frame.components[0].block_width + index % cols) * 64;
                for (natural, &coefficient) in block.iter().enumerate() {
                    assert_eq!(
                        quantize(coefficient, 1).unwrap(),
                        frame.components[0].coefficients[first + natural]
                    );
                }
            }
        }
    }
}
#[test]
fn histogram_curve_points_match_direct_weighted_fdct_error_and_complete_ac_rate_oracles() {
    let r = reference(19, 25);
    let budget = 1024 * 1024 * 1024;
    let a = Analysis::for_reference(&r, budget).unwrap();
    for lambda in [0, 256] {
        let current = encoded(&r, SamplingFactor::R_4_2_0, 7, lambda);
        let captured = coefficient_io::capture(&current, budget).unwrap();
        let model = build(&r, &a, &captured.pixels, &captured.frame, 4).unwrap();
        let rate = CoefficientRateModel::new(4).unwrap();
        let mut tested = 0;
        for natural in [1, 17, 63] {
            for point in &model.curves[natural] {
                if point.retain {
                    continue;
                }
                let (mut pixel, mut tile, mut bits) = (0i128, 0i128, 0i64);
                for (index, source) in model.raw.iter().enumerate() {
                    let (bx, by) = (index % 3, index / 3);
                    let start = (by * captured.frame.components[0].block_width + bx) * 64;
                    let old: [i16; 64] = captured.frame.components[0].coefficients
                        [start..start + 64]
                        .try_into()
                        .unwrap();
                    let mut new = old;
                    new[natural] = quantize(source[natural], point.quantizer).unwrap();
                    bits += i64::from(rate.bits(&new, false).unwrap())
                        - i64::from(rate.bits(&old, false).unwrap());
                    let mut weight = 0i128;
                    for y in by * 8..((by + 1) * 8).min(25) {
                        for x in bx * 8..((bx + 1) * 8).min(19) {
                            weight += i128::from(a.weights[y * 19 + x]);
                        }
                    }
                    let before = i128::from(source[natural]) - 8 * 7 * i128::from(old[natural]);
                    let after = i128::from(source[natural])
                        - 8 * i128::from(point.quantizer) * i128::from(new[natural]);
                    let delta = weight * (after * after - before * before);
                    pixel += delta;
                    if model.critical_tile == (bx / 2, by / 2) {
                        tile += delta;
                    }
                }
                assert_eq!(
                    (point.pixel, point.edge, point.tile, point.rate),
                    (
                        pixel,
                        pixel * (EIGEN[natural / 8] + EIGEN[natural % 8]),
                        tile,
                        bits
                    )
                );
                tested += 1;
            }
        }
        assert!(tested > 0);
    }
}
