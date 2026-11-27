use crate::{ColorType, Encoder, EncodingError, SamplingFactor};
use alloc::vec::Vec;
use jpeg_decoder::Decoder;

#[test]
fn per_scan_tables_preserve_exact_pixels_for_sparse_dense_and_restart_streams() {
    let (width, height) = (37u16, 29u16);
    for pattern in 0..3 {
        let rgb: Vec<_> = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| match pattern {
                    0 => [128; 3],
                    1 => [(x * 5) as u8, (y * 7) as u8, ((x + y) * 3) as u8],
                    _ => [
                        ((x * 71 + y * 29) % 256) as u8,
                        ((x * 13 + y * 97) % 256) as u8,
                        ((x * 43 + y * 17) % 256) as u8,
                    ],
                })
            })
            .collect();
        for sampling in [
            SamplingFactor::R_4_4_4,
            SamplingFactor::R_4_2_2,
            SamplingFactor::R_4_2_0,
        ] {
            for scans in [2, 3, 4, 16, 33, 34, 64] {
                for restart in [0, 1, 7] {
                    let encode = |per_scan| {
                        let mut out = Vec::new();
                        let mut encoder = Encoder::new(&mut out, 93);
                        encoder.set_sampling_factor(sampling);
                        encoder.set_progressive_scans(scans);
                        encoder.set_restart_interval(restart);
                        encoder.set_optimized_huffman_tables(true);
                        encoder.set_progressive_eob_runs(true);
                        encoder.set_progressive_scan_huffman(per_scan);
                        encoder.encode(&rgb, width, height, ColorType::Rgb).unwrap();
                        out
                    };
                    let before = encode(false);
                    let after = encode(true);
                    let mut a = Decoder::new(before.as_slice());
                    let mut b = Decoder::new(after.as_slice());
                    assert_eq!(
                        a.decode().unwrap(),
                        b.decode().unwrap(),
                        "pattern={pattern}, sampling={sampling:?}, scans={scans}, restart={restart}"
                    );
                    assert_eq!(a.info(), b.info());
                }
            }
        }
    }
}

#[test]
fn per_scan_tables_reject_incompatible_configuration_explicitly() {
    for (progressive, optimized, runs) in [
        (false, false, false),
        (true, true, false),
        (true, false, true),
        (false, true, true),
    ] {
        let mut out = Vec::new();
        let mut encoder = Encoder::new(&mut out, 90);
        encoder.set_progressive(progressive);
        encoder.set_optimized_huffman_tables(optimized);
        encoder.set_progressive_eob_runs(runs);
        encoder.set_progressive_scan_huffman(true);
        assert!(matches!(
            encoder.encode(&[128; 3], 1, 1, ColorType::Rgb),
            Err(EncodingError::InvalidConfiguration(_))
        ));
    }
}
