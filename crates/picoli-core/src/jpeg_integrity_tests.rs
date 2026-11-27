use crate::{Error, Reference, decode_baseline};
use image::RgbImage;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

fn fixture(width: u32, height: u32, progressive: bool, sampling: u16, restart: u16) -> Vec<u8> {
    let pixels = RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            ((x * 37 + y * 29) % 256) as u8,
            ((x * 71 + y * 13) % 256) as u8,
            ((x * 17 + y * 43) % 256) as u8,
        ])
    });
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 80);
    encoder.set_sampling_factor(match sampling {
        444 => SamplingFactor::R_4_4_4,
        422 => SamplingFactor::R_4_2_2,
        420 => SamplingFactor::R_4_2_0,
        _ => panic!("test sampling"),
    });
    encoder.set_progressive(progressive);
    encoder.set_restart_interval(restart);
    // Use standard Huffman tables so decoder fixtures do not depend on the
    // encoder's separate entropy-table optimization.
    encoder
        .encode(pixels.as_raw(), width as u16, height as u16, ColorType::Rgb)
        .unwrap();
    bytes
}

// Test fixture parser only: identify entropy ranges while preserving stuffed FF
// bytes and restart markers. No production decoder logic is duplicated here.
fn entropy_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut pos = 2;
    while pos < bytes.len() {
        assert_eq!(bytes[pos], 0xff);
        while bytes[pos] == 0xff {
            pos += 1;
        }
        let marker = bytes[pos];
        pos += 1;
        if marker == 0xd9 {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd8) {
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[pos], bytes[pos + 1]]));
        pos += length;
        if marker == 0xda {
            let start = pos;
            while pos < bytes.len() {
                if bytes[pos] == 0xff {
                    let next = bytes[pos + 1];
                    if next == 0x00 || (0xd0..=0xd7).contains(&next) {
                        pos += 2;
                        continue;
                    }
                    break;
                }
                pos += 1;
            }
            ranges.push((start, pos));
        }
    }
    ranges
}

#[test]
fn valid_small_jpegs_with_sampling_and_restarts_still_decode() {
    for (width, height) in [(1, 1), (7, 9), (17, 13)] {
        for progressive in [false, true] {
            for sampling in [444, 422, 420] {
                for restart in [0, 3] {
                    let bytes = fixture(width, height, progressive, sampling, restart);
                    let decoded = decode_baseline(&bytes).unwrap_or_else(|e| panic!(
                        "valid {width}x{height} progressive={progressive} sampling={sampling} restart={restart}: {e}"));
                    assert_eq!(decoded.pixels.dimensions(), (width, height));
                }
            }
        }
    }
    // The separate grayscale decoder path must also tolerate valid end padding.
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, 80)
        .encode(&[128; 9], 3, 3, ColorType::Luma)
        .unwrap();
    let Reference { pixels, .. } = decode_baseline(&bytes).unwrap();
    assert_eq!(pixels.dimensions(), (3, 3));
}

#[test]
fn truncation_inside_every_scan_is_rejected_in_both_jpeg_modes() {
    for progressive in [false, true] {
        for sampling in [444, 422, 420] {
            for restart in [0, 3] {
                let bytes = fixture(128, 128, progressive, sampling, restart);
                decode_baseline(&bytes).unwrap_or_else(|e| panic!(
                    "valid 128x128 progressive={progressive} sampling={sampling} restart={restart}: {e}"));
                let ranges = entropy_ranges(&bytes);
                assert!(!ranges.is_empty());
                for (scan, &(start, end)) in ranges.iter().enumerate() {
                    assert!(
                        end - start >= 4,
                        "fixture scan must contain truncatable entropy"
                    );
                    for fraction in [1, 2, 3] {
                        let mut cut = start + (end - start) * fraction / 4;
                        // Do not accidentally create a stuffed marker prefix at the cut.
                        if bytes[cut - 1] == 0xff {
                            cut -= 1;
                        }
                        let mut damaged = bytes[..cut].to_vec();
                        damaged.extend_from_slice(&[0xff, 0xd9]);
                        assert!(
                            matches!(decode_baseline(&damaged), Err(Error::InvalidJpeg(_))),
                            "accepted truncated progressive={progressive} sampling={sampling} restart={restart} scan={scan} quarter={fraction}"
                        );
                    }
                }
            }
        }
    }
}
