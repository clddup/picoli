use super::*;
use crate::jpeg_table::coefficient_io::{self, Coding};
use image::Rgb;
use jpeg_encoder::{ColorType, Encoder, QuantizationTableType};
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
fn encoded(r: &Reference) -> Vec<u8> {
    let mut data = Vec::new();
    let mut e = Encoder::new(&mut data, 100);
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([7; 64])),
        QuantizationTableType::Custom(Box::new([7; 64])),
    );
    e.set_progressive_scans(4);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    e.set_experimental_ac_rdo_lambda(256);
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
fn offline_basis_is_dc_exact_and_orthonormal_within_the_derived_rounding_error_bound() {
    assert_eq!(BASIS[0], [2048; 64]);
    // Each rounded vector differs from its exact unit-scaled vector by norm<=4.
    let bound = 2 * (1i64 << 14) * 4 + 16;
    for (i, a) in BASIS.iter().enumerate() {
        for (j, b) in BASIS.iter().enumerate() {
            let dot: i64 = a
                .iter()
                .zip(b)
                .map(|(&x, &y)| i64::from(x) * i64::from(y))
                .sum();
            let expected = if i == j { 1 << 28 } else { 0 };
            assert!((dot - expected).abs() <= bound, "basis {i} {j} dot={dot}");
        }
    }
}
#[test]
fn spatial_moment_curves_equal_independent_pixel_jacobian_and_complete_ac_bit_oracles() {
    let r = reference(19, 17);
    let budget = 1024 * 1024 * 1024;
    let a = Analysis::for_reference(&r, budget).unwrap();
    let current = encoded(&r);
    let captured = coefficient_io::capture(&current, budget).unwrap();
    let model = build(&r, &a, &captured.pixels, &captured.frame, 4, usize::MAX).unwrap();
    let rate = CoefficientRateModel::new(4).unwrap();
    let cols = 3;
    let mut tested = 0;
    for natural in [1, 17, 63] {
        for point in &model.curves[natural] {
            if point.retain {
                continue;
            }
            let (mut pixel, mut tile, mut edge, mut bits) = (0i128, 0i128, 0i128, 0i64);
            for (index, source) in model.raw.iter().enumerate() {
                let (bx, by) = (index % cols, index / cols);
                let first = (by * captured.frame.components[0].block_width + bx) * 64;
                let old: [i16; 64] = captured.frame.components[0].coefficients[first..first + 64]
                    .try_into()
                    .unwrap();
                let mut new = old;
                new[natural] = model::quantize(source[natural], point.quantizer).unwrap();
                bits += i64::from(rate.bits(&new, false).unwrap())
                    - i64::from(rate.bits(&old, false).unwrap());
                let old_dc = i128::from(captured.frame.components[0].quantization_table[natural])
                    * i128::from(old[natural]);
                let new_dc = i128::from(point.quantizer) * i128::from(new[natural]);
                let delta = new_dc - old_dc;
                for (t, &basis_value) in BASIS[natural].iter().enumerate() {
                    let (x, y) = (bx * 8 + t % 8, by * 8 + t / 8);
                    if x >= 19 || y >= 17 {
                        continue;
                    }
                    let i = y * 19 + x;
                    let w = i128::from(a.weights[i]);
                    let rgb = captured.pixels.get_pixel(x as u32, y as u32).0;
                    let c = color(rgb);
                    let m = rgb.map(|v| i128::from(v > 0 && v < 255));
                    let jac = [
                        77 * m[0] + 150 * m[1] + 29 * m[2],
                        256 * (m[0] - m[2]),
                        256 * m[1] - 128 * (m[0] + m[2]),
                    ];
                    let basis = i128::from(basis_value);
                    let mut local = 0;
                    for (channel, &j) in jac.iter().enumerate() {
                        let before = i128::from(a.colors.at(i)[channel] - c[channel]) * Q;
                        let after = before - delta * basis * j;
                        local +=
                            (if channel == 0 { 4 } else { 1 }) * (after * after - before * before);
                    }
                    pixel += w * local;
                    if model.critical_tile == (x / 16, y / 16) {
                        tile += w * local;
                    }
                    let before = i128::from(source[natural]) - 8 * old_dc;
                    let after = i128::from(source[natural]) - 8 * new_dc;
                    edge += w
                        * (after * after - before * before)
                        * (EIGEN[natural / 8] + EIGEN[natural % 8]);
                }
            }
            assert_eq!(
                (point.pixel, point.edge, point.tile, point.rate),
                (pixel, edge, tile, bits)
            );
            tested += 1;
        }
    }
    assert!(tested > 0);
    assert!(matches!(
        build(&r, &a, &captured.pixels, &captured.frame, 4, 0),
        Err(Error::Limit(_))
    ));
}
#[test]
fn projected_actual_codec_path_removes_injected_error_without_dc_or_chroma_change() {
    let template = Reference {
        pixels: RgbImage::from_pixel(33, 35, Rgb([97, 127, 167])),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    };
    let budget = 1024 * 1024 * 1024;
    let coding = Coding {
        scans: 4,
        eob_runs: true,
        scan_huffman: true,
    };
    let mut frame = coefficient_io::capture(&encoded(&template), budget)
        .unwrap()
        .frame;
    let canonical = coefficient_io::serialize(&template, &frame, coding).unwrap();
    let reference = crate::decode_with_budget(&canonical, budget).unwrap();
    for by in 0..35usize.div_ceil(8) {
        for bx in 0..33usize.div_ceil(8) {
            let first = (by * frame.components[0].block_width + bx) * 64;
            assert_eq!(frame.components[0].coefficients[first + 1], 0);
            frame.components[0].coefficients[first + 1] = 1;
        }
    }
    let current = coefficient_io::serialize(&reference, &frame, coding).unwrap();
    let result =
        super::super::test_oracle::run_projected(&reference, &current, coding, budget).unwrap();
    assert!(result.identity_file_exact);
    assert!(result.stats.paired.changed_frequencies > 0);
    assert!(result.raw.len() < current.len());
    assert!(super::super::nonworse(
        &result.raw_score,
        &result.control_score,
        crate::quality::Strength::default()
    ));
    assert_ne!(result.selected_variant, "current");
    let after = coefficient_io::capture(&result.raw, budget).unwrap().frame;
    assert_ne!(
        frame.components[0].quantization_table,
        after.components[0].quantization_table
    );
    assert_eq!(
        frame.components[0].quantization_table[0],
        after.components[0].quantization_table[0]
    );
    for (index, (a, b)) in frame.components.iter().zip(&after.components).enumerate() {
        assert_eq!(a.horizontal_sampling_factor, b.horizontal_sampling_factor);
        assert_eq!(a.vertical_sampling_factor, b.vertical_sampling_factor);
        for (x, y) in a
            .coefficients
            .chunks_exact(64)
            .zip(b.coefficients.chunks_exact(64))
        {
            assert_eq!(x[0], y[0]);
            if index > 0 {
                assert_eq!(x, y);
            }
        }
        if index > 0 {
            assert_eq!(a.quantization_table, b.quantization_table);
        }
    }
    assert_eq!(
        crate::decode_with_budget(&result.raw, budget)
            .unwrap()
            .pixels,
        reference.pixels
    );
}
