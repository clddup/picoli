use super::*;
use crate::quality::Limits;
use image::{GrayImage, Luma, Rgb};

// Independent oracle retains the exact old three complete background passes.
fn legacy(
    a: &Analysis,
    candidate: &RgbImage,
    limits: Option<Limits>,
) -> Result<Option<Score>, Error> {
    let Some(mut score) = a.score_on_background(candidate, 0, limits)? else {
        return Ok(None);
    };
    for background in [128, 255] {
        let Some(other) = a.score_on_background(candidate, background, limits)? else {
            return Ok(None);
        };
        score.weighted_pixel_error = score.weighted_pixel_error.max(other.weighted_pixel_error);
        score.weighted_edge_error = score.weighted_edge_error.max(other.weighted_edge_error);
        score.worst_tile_error = score.worst_tile_error.max(other.worst_tile_error);
    }
    score.loss =
        score.weighted_pixel_error + 2.0 * score.weighted_edge_error + 0.1 * score.worst_tile_error;
    Ok(Some(score))
}
fn bits(s: &Score) -> [u64; 4] {
    [
        s.weighted_pixel_error.to_bits(),
        s.weighted_edge_error.to_bits(),
        s.worst_tile_error.to_bits(),
        s.loss.to_bits(),
    ]
}
fn same(a: Option<Score>, b: Option<Score>) {
    assert_eq!(a.as_ref().map(bits), b.as_ref().map(bits));
}
fn mix(x: u32, y: u32, seed: u32) -> u32 {
    let n = x.wrapping_mul(0x9e3779b9) ^ y.wrapping_mul(0x85ebca6b) ^ seed;
    (n ^ (n >> 16)).wrapping_mul(0xc2b2ae35)
}
fn analysis(w: u32, h: u32, mode: u32) -> (Analysis, RgbImage) {
    let pixels = RgbImage::from_fn(w, h, |x, y| {
        let n = mix(x, y, 17);
        Rgb([n as u8, (n >> 8) as u8, (n >> 16) as u8])
    });
    let alpha = GrayImage::from_fn(w, h, |x, y| {
        Luma([match mode {
            0 => 0,
            1 => 255,
            2 => [0, 1, 2, 127, 128, 254, 255][((x + 3 * y) % 7) as usize],
            _ => mix(x, y, 97) as u8,
        }])
    });
    let reference = crate::Reference {
        pixels: pixels.clone(),
        alpha: Some(alpha),
        source_format: "png",
        jpeg_metadata: crate::JpegMetadata::default(),
    };
    (
        Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap(),
        pixels,
    )
}
#[test]
fn every_alpha_and_luma_preserves_exact_white_and_shared_chroma_identity() {
    let (mut a, _) = analysis(1, 1, 0);
    for alpha in 0..=255u8 {
        a.alpha.as_mut().unwrap().as_mut()[0] = alpha;
        for y in 0..=255 {
            let color = [y, y - 128, 128 - y];
            let black = a.composite(color, 0, 0);
            let white = a.composite(color, 0, 255);
            let gray = a.composite(color, 0, 128);
            assert_eq!(white[0], black[0] + 255 - i32::from(alpha));
            assert_eq!([white[1], white[2]], [black[1], black[2]]);
            assert_eq!([gray[1], gray[2]], [black[1], black[2]]);
        }
    }
}
#[test]
fn full_and_bounded_three_background_scores_match_legacy_bit_for_bit() {
    for (w, h) in [
        (1, 1),
        (1, 33),
        (33, 1),
        (15, 15),
        (16, 16),
        (17, 19),
        (33, 35),
        (65, 33),
    ] {
        for mode in 0..4 {
            let (a, source) = analysis(w, h, mode);
            for variant in 0..3 {
                let candidate = RgbImage::from_fn(w, h, |x, y| match variant {
                    0 => *source.get_pixel(x, y),
                    1 => Rgb(source.get_pixel(x, y).0.map(|v| v.saturating_add(1))),
                    _ => {
                        let n = mix(x, y, 42);
                        Rgb([n as u8, (n >> 8) as u8, (n >> 16) as u8])
                    }
                });
                let old = legacy(&a, &candidate, None).unwrap().unwrap();
                assert_eq!(bits(&a.score(&candidate).unwrap()), bits(&old));
                for strength in [0.0f64, 25.0, 50.0, 75.0, 100.0] {
                    let f = (strength / 50.0).powi(4);
                    let l = Limits {
                        pixel: 64.0 * f,
                        edge: 16.0 * f,
                        tile: 512.0 * f,
                    };
                    same(
                        a.score_with_limits(&candidate, l).unwrap(),
                        legacy(&a, &candidate, Some(l)).unwrap(),
                    );
                }
                for l in [
                    Limits {
                        pixel: old.weighted_pixel_error,
                        edge: old.weighted_edge_error,
                        tile: old.worst_tile_error,
                    },
                    Limits {
                        pixel: old.weighted_pixel_error - 1e-9,
                        edge: f64::INFINITY,
                        tile: f64::INFINITY,
                    },
                    Limits {
                        pixel: f64::INFINITY,
                        edge: old.weighted_edge_error - 1e-9,
                        tile: f64::INFINITY,
                    },
                    Limits {
                        pixel: f64::INFINITY,
                        edge: f64::INFINITY,
                        tile: old.worst_tile_error - 1e-9,
                    },
                    Limits {
                        pixel: f64::NAN,
                        edge: f64::INFINITY,
                        tile: f64::INFINITY,
                    },
                ] {
                    same(
                        a.score_with_limits(&candidate, l).unwrap(),
                        legacy(&a, &candidate, Some(l)).unwrap(),
                    );
                }
            }
        }
    }
}
#[test]
fn dimension_errors_and_opaque_fallback_remain_unchanged() {
    let (a, source) = analysis(17, 19, 3);
    let wrong = RgbImage::new(19, 17);
    assert!(a.score_transparent_fused(&wrong, None).is_err());
    assert!(legacy(&a, &wrong, None).is_err());
    let opaque = Analysis::new(&source).unwrap();
    same(
        opaque.score_transparent_fused(&source, None).unwrap(),
        opaque.score_on_background(&source, 0, None).unwrap(),
    );
}
