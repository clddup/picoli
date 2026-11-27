use crate::{ChromaSubsamplingMethod, ColorType, Encoder, QuantizationTableType, SamplingFactor};
use alloc::{boxed::Box, vec::Vec};
use std::io::Cursor;

#[test]
fn invalid_rdo_configuration_returns_error_before_writing_headers() {
    for (progressive, step) in [(false, 7), (true, 256), (true, 8191)] {
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, 100);
        if progressive {
            encoder.set_progressive_scans(2);
        }
        encoder.set_quantization_tables(
            QuantizationTableType::Custom(Box::new([step; 64])),
            QuantizationTableType::Custom(Box::new([step; 64])),
        );
        encoder.set_experimental_ac_rdo_lambda(256);
        assert!(matches!(
            encoder.encode(&[128; 3], 1, 1, ColorType::Rgb),
            Err(crate::EncodingError::InvalidConfiguration(_))
        ));
        assert!(bytes.is_empty());
    }
}

#[test]
fn nonzero_rdo_encodes_with_debug_assertions_and_independent_decoder() {
    let width = 17;
    let height = 19;
    let raw: Vec<u8> = (0..width * height)
        .flat_map(|i| [(i * 37) as u8, (i * 71) as u8, (i * 13) as u8])
        .collect();
    for scans in [2, 4] {
        for sampling in [SamplingFactor::R_4_4_4, SamplingFactor::R_4_2_0] {
            for restart in [false, true] {
                let encode = |lambda: Option<u32>, weighted: bool| {
                    let mut bytes = Vec::new();
                    let mut encoder = Encoder::new(&mut bytes, 100);
                    encoder.set_quantization_tables(
                        QuantizationTableType::Custom(Box::new([7; 64])),
                        QuantizationTableType::Custom(Box::new([11; 64])),
                    );
                    encoder.set_sampling_factor(sampling);
                    encoder.set_chroma_subsampling_method(ChromaSubsamplingMethod::Average);
                    encoder.set_optimized_huffman_tables(true);
                    encoder.set_progressive_scans(scans);
                    encoder.set_progressive_eob_runs(true);
                    encoder.set_progressive_scan_huffman(true);
                    encoder.set_experimental_ac_rdo_sampling_weights(weighted);
                    if restart {
                        encoder.set_restart_interval(1);
                    }
                    if let Some(lambda) = lambda {
                        encoder.set_experimental_ac_rdo_lambda(lambda);
                    }
                    encoder
                        .encode(&raw, width as u16, height as u16, ColorType::Rgb)
                        .unwrap();
                    bytes
                };
                assert_eq!(encode(None, false), encode(Some(0), false));
                assert_eq!(encode(Some(0), false), encode(Some(0), true));
                for lambda in [16, 64, 256, 1024] {
                    for weighted in [false, true] {
                        let bytes = encode(Some(lambda), weighted);
                        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&bytes));
                        let decoded = decoder.decode().unwrap();
                        let info = decoder.info().unwrap();
                        assert_eq!((info.width, info.height), (width as u16, height as u16));
                        assert_eq!(decoded.len(), width * height * 3);
                    }
                }
            }
        }
    }
}
