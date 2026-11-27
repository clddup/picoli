use super::super::{ChromaSubsamplingMethod, DefaultOperations};
use super::*;
use crate::{ColorType, image_buffer::RgbImage};
use alloc::vec;
use std::io::Cursor;
fn configure<W: JfifWrite>(
    e: &mut Encoder<W>,
    sampling: SamplingFactor,
    scans: u8,
    entropy: u8,
    restart: bool,
) {
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([7; 64])),
        QuantizationTableType::Custom(Box::new([11; 64])),
    );
    e.set_sampling_factor(sampling);
    e.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
    e.set_progressive_scans(scans);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(entropy >= 1);
    e.set_progressive_scan_huffman(entropy >= 2);
    if restart {
        e.set_restart_interval(1);
    }
}
#[test]
fn coefficient_serializer_matches_pixel_path_exactly_with_ignored_padding() {
    for (width, height) in [(17u16, 19u16), (1, 257), (257, 1)] {
        let raw: Vec<u8> = (0..usize::from(width) * usize::from(height))
            .flat_map(|i| [(i * 37) as u8, (i * 71) as u8, (i * 13) as u8])
            .collect();
        for sampling in [
            SamplingFactor::R_4_4_4,
            SamplingFactor::R_4_2_2,
            SamplingFactor::R_4_2_0,
        ] {
            let mut unused = Vec::new();
            let mut prep = Encoder::new(&mut unused, 100);
            configure(&mut prep, sampling, 2, 2, false);
            prep.init_components(JpegColorType::Ycbcr);
            let qvalues = [[7u16; 64], [11; 64]];
            let qtables = [
                QuantizationTable::new_with_quality(
                    &QuantizationTableType::Custom(Box::new(qvalues[0])),
                    100,
                    true,
                ),
                QuantizationTable::new_with_quality(
                    &QuantizationTableType::Custom(Box::new(qvalues[1])),
                    100,
                    false,
                ),
            ];
            let blocks = prep
                .encode_blocks::<_, DefaultOperations>(&RgbImage(&raw, width, height), &qtables);
            let (mh, mv) = prep.get_max_sampling_size();
            let mut planes = Vec::new();
            let mut geometry = Vec::new();
            for (i, c) in prep.components.iter().enumerate() {
                let cols = usize::from(width)
                    .div_ceil(8)
                    .div_ceil(mh / usize::from(c.horizontal_sampling_factor));
                let rows = usize::from(height)
                    .div_ceil(8)
                    .div_ceil(mv / usize::from(c.vertical_sampling_factor));
                let stride = cols + 1;
                // Invalid coding magnitudes in UNUSED padding must not be serialized.
                let mut natural = vec![i16::MAX; stride * (rows + 1) * 64];
                for by in 0..rows {
                    for bx in 0..cols {
                        for z in 0..64 {
                            natural[(by * stride + bx) * 64 + (ZIGZAG[z] as usize & 0x3f)] =
                                blocks[i][by * cols + bx].data[z];
                        }
                    }
                }
                planes.push(natural);
                geometry.push((
                    c.horizontal_sampling_factor,
                    c.vertical_sampling_factor,
                    stride,
                    rows + 1,
                ));
            }
            let components: Vec<_> = planes
                .iter()
                .enumerate()
                .map(|(i, p)| QuantizedComponent {
                    horizontal_sampling_factor: geometry[i].0,
                    vertical_sampling_factor: geometry[i].1,
                    quantization_table: &qvalues[usize::from(i > 0)],
                    block_width: geometry[i].2,
                    block_height: geometry[i].3,
                    coefficients: p,
                })
                .collect();
            for scans in [2, 4] {
                for entropy in [0, 1, 2] {
                    for restart in [false, true] {
                        let mut old = Vec::new();
                        let mut e = Encoder::new(&mut old, 100);
                        configure(&mut e, sampling, scans, entropy, restart);
                        e.encode(&raw, width, height, ColorType::Rgb).unwrap();
                        let mut new = Vec::new();
                        let mut e = Encoder::new(&mut new, 100);
                        configure(&mut e, sampling, scans, entropy, restart);
                        e.encode_quantized(QuantizedImage {
                            width,
                            height,
                            color_type: JpegColorType::Ycbcr,
                            components: &components,
                        })
                        .unwrap();
                        assert_eq!(old, new);
                        let mut d = jpeg_decoder::Decoder::new(Cursor::new(new));
                        assert_eq!(d.decode().unwrap().len(), raw.len());
                    }
                }
            }
        }
    }
}
#[test]
fn malformed_coefficients_and_configuration_fail_before_any_header() {
    let table = [7u16; 64];
    for (mode, length) in [(0, 63), (1, 64), (2, 64)] {
        let mut q = vec![0i16; length];
        if mode == 1 {
            q[0] = 2048;
        }
        if mode == 2 {
            q[1] = 1024;
        }
        let comp = QuantizedComponent {
            horizontal_sampling_factor: 1,
            vertical_sampling_factor: 1,
            quantization_table: &table,
            block_width: 1,
            block_height: 1,
            coefficients: &q,
        };
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes, 100);
        e.set_progressive_scans(2);
        e.set_optimized_huffman_tables(true);
        assert!(
            e.encode_quantized(QuantizedImage {
                width: 1,
                height: 1,
                color_type: JpegColorType::Luma,
                components: &[comp]
            })
            .is_err()
        );
        assert!(bytes.is_empty());
    }
    let q = [0i16; 64];
    for progressive in [false, true] {
        let comp = QuantizedComponent {
            horizontal_sampling_factor: 1,
            vertical_sampling_factor: 1,
            quantization_table: &table,
            block_width: 1,
            block_height: 1,
            coefficients: &q,
        };
        let mut bytes = Vec::new();
        let mut e = Encoder::new(&mut bytes, 100);
        if progressive {
            e.set_progressive_scans(2);
            e.set_experimental_ac_rdo_lambda(256);
        }
        assert!(
            e.encode_quantized(QuantizedImage {
                width: 1,
                height: 1,
                color_type: JpegColorType::Luma,
                components: &[comp]
            })
            .is_err()
        );
        assert!(bytes.is_empty());
    }
}
#[test]
fn grayscale_coefficient_input_decodes_as_grayscale_without_extra_color_planes() {
    let qtable = [8u16; 64];
    let mut q = [0i16; 64];
    q[0] = 16;
    let comp = QuantizedComponent {
        horizontal_sampling_factor: 1,
        vertical_sampling_factor: 1,
        quantization_table: &qtable,
        block_width: 1,
        block_height: 1,
        coefficients: &q,
    };
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes, 100);
    e.set_progressive_scans(2);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    e.encode_quantized(QuantizedImage {
        width: 5,
        height: 7,
        color_type: JpegColorType::Luma,
        components: &[comp],
    })
    .unwrap();
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    let pixels = d.decode().unwrap();
    assert_eq!(pixels.len(), 35);
    assert!(pixels.iter().all(|&x| x == 144));
}
