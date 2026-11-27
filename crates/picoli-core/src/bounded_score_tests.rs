use crate::{
    analysis::Analysis,
    quality::{Limits, Strength},
    *,
};

fn assert_same(full: &analysis::Score, bounded: &analysis::Score) {
    assert_eq!(full.weighted_pixel_error, bounded.weighted_pixel_error);
    assert_eq!(full.weighted_edge_error, bounded.weighted_edge_error);
    assert_eq!(full.worst_tile_error, bounded.worst_tile_error);
    assert_eq!(full.loss, bounded.loss);
}

#[test]
fn bounded_scores_match_full_admission_and_exact_values_at_thresholds_with_alpha() {
    for (w, h) in [(1, 1), (17, 19), (37, 33), (64, 64)] {
        for alpha in [None, Some(false), Some(true)] {
            let reference = Reference {
                pixels: RgbImage::from_fn(w, h, |x, y| {
                    image::Rgb([
                        (x * 71 + y * 29) as u8,
                        (x * 17 + y * 97) as u8,
                        (x * 43 + y * 13) as u8,
                    ])
                }),
                alpha: alpha.map(|mixed| {
                    image::GrayImage::from_fn(w, h, |x, y| {
                        image::Luma([if mixed { ((x + y * w) % 256) as u8 } else { 0 }])
                    })
                }),
                source_format: "png",
                jpeg_metadata: Default::default(),
            };
            let analysis =
                Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
            for amount in [0u8, 1, 8, 64, 255] {
                let candidate = RgbImage::from_fn(w, h, |x, y| {
                    let p = reference.pixels.get_pixel(x, y);
                    image::Rgb([
                        p[0] ^ amount,
                        p[1].saturating_add(amount),
                        p[2].saturating_sub(amount),
                    ])
                });
                let full = analysis.score(&candidate).unwrap();
                for strength in [0, 1, 25, 50, 75, 100] {
                    let limits = Strength::new(strength).unwrap().limits();
                    let bounded = analysis.score_with_limits(&candidate, limits).unwrap();
                    assert_eq!(
                        bounded.is_some(),
                        limits.accepts(&full),
                        "{w}x{h}, alpha={alpha:?}, strength={strength}"
                    );
                    if let Some(bounded) = bounded {
                        assert_same(&full, &bounded);
                    }
                }
                let exact = Limits {
                    pixel: full.weighted_pixel_error,
                    edge: full.weighted_edge_error,
                    tile: full.worst_tile_error,
                };
                assert_same(
                    &full,
                    &analysis
                        .score_with_limits(&candidate, exact)
                        .unwrap()
                        .unwrap(),
                );
                for component in 0..3 {
                    let mut limits = Limits {
                        pixel: f64::INFINITY,
                        edge: f64::INFINITY,
                        tile: f64::INFINITY,
                    };
                    let slot = match component {
                        0 => &mut limits.pixel,
                        1 => &mut limits.edge,
                        _ => &mut limits.tile,
                    };
                    let value = match component {
                        0 => full.weighted_pixel_error,
                        1 => full.weighted_edge_error,
                        _ => full.worst_tile_error,
                    };
                    *slot = value - f64::EPSILON * value.abs().max(1.0);
                    assert!(
                        analysis
                            .score_with_limits(&candidate, limits)
                            .unwrap()
                            .is_none()
                    );
                }
            }
            assert!(
                analysis
                    .score_with_limits(&RgbImage::new(w + 1, h), Strength::default().limits())
                    .is_err()
            );
        }
    }
}

#[test]
fn partial_global_means_and_partial_tile_means_must_not_reject_valid_images() {
    let reference = RgbImage::from_pixel(32, 32, image::Rgb([0; 3]));
    let analysis = Analysis::new(&reference).unwrap();
    let mut candidate = reference.clone();
    candidate.put_pixel(0, 0, image::Rgb([200; 3]));
    let full = analysis.score(&candidate).unwrap();
    let limits = Limits {
        pixel: full.weighted_pixel_error,
        edge: full.weighted_edge_error,
        tile: full.worst_tile_error,
    };
    // After the first row, using the processed weight rather than final weight
    // would incorrectly reject this eventually-valid boundary case.
    assert_same(
        &full,
        &analysis
            .score_with_limits(&candidate, limits)
            .unwrap()
            .unwrap(),
    );
}
