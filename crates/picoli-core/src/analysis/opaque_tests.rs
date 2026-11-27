use super::opaque::OpaqueContext;
use super::{Analysis, Score, color};
use crate::{
    Reference, palette,
    quality::{Limits, Strength},
    resources,
};
use image::{Rgb, RgbImage};

fn same(a: &Score, b: &Score) {
    assert_eq!(
        a.weighted_pixel_error.to_bits(),
        b.weighted_pixel_error.to_bits()
    );
    assert_eq!(
        a.weighted_edge_error.to_bits(),
        b.weighted_edge_error.to_bits()
    );
    assert_eq!(a.worst_tile_error.to_bits(), b.worst_tile_error.to_bits());
    assert_eq!(a.loss.to_bits(), b.loss.to_bits());
}

fn streamed(analysis: &Analysis, candidate: &RgbImage, limits: Limits) -> Option<Score> {
    let context = OpaqueContext::new(analysis, limits).unwrap();
    let mut scorer = context.start();
    for (i, pixel) in candidate.pixels().enumerate() {
        if !scorer.push(i, color(pixel.0)) {
            return None;
        }
    }
    Some(scorer.finish())
}

#[test]
fn opaque_streaming_scores_match_full_and_exact_threshold_admission() {
    for (width, height) in [(1, 1), (17, 19), (37, 33), (64, 64)] {
        let source = RgbImage::from_fn(width, height, |x, y| {
            Rgb([
                (x * 71 + y * 23) as u8,
                (x * 7 + y * 19) as u8,
                (x * 43 + y * 13) as u8,
            ])
        });
        let analysis = Analysis::new(&source).unwrap();
        for change in [0u8, 1, 7, 31, 255] {
            let candidate = RgbImage::from_fn(width, height, |x, y| {
                let p = source.get_pixel(x, y).0;
                Rgb([
                    p[0].saturating_add(change),
                    p[1].saturating_sub(change),
                    p[2] ^ change,
                ])
            });
            let full = analysis.score(&candidate).unwrap();
            for strength in [0, 1, 25, 50, 75, 100] {
                let limits = Strength::new(strength).unwrap().limits();
                let result = streamed(&analysis, &candidate, limits);
                assert_eq!(result.is_some(), limits.accepts(&full));
                if let Some(score) = result {
                    same(&score, &full);
                }
            }
            let limits = Limits {
                pixel: full.weighted_pixel_error,
                edge: full.weighted_edge_error,
                tile: full.worst_tile_error,
            };
            same(&streamed(&analysis, &candidate, limits).unwrap(), &full);
            for component in 0..3 {
                let mut lower = limits;
                let value = match component {
                    0 => &mut lower.pixel,
                    1 => &mut lower.edge,
                    _ => &mut lower.tile,
                };
                if *value > 0.0 {
                    *value = f64::from_bits(value.to_bits() - 1);
                    assert!(streamed(&analysis, &candidate, lower).is_none());
                }
            }
        }
    }
}

#[test]
fn partial_global_or_tile_means_cannot_reject_a_valid_final_candidate() {
    let source = RgbImage::new(512, 16);
    let analysis = Analysis::new(&source).unwrap();
    let mut candidate = source.clone();
    candidate.put_pixel(0, 0, Rgb([64; 3]));
    let full = analysis.score(&candidate).unwrap();
    let limits = Strength::default().limits();
    assert!(limits.accepts(&full));
    same(&streamed(&analysis, &candidate, limits).unwrap(), &full);
    let source = RgbImage::new(16, 16);
    let analysis = Analysis::new(&source).unwrap();
    let candidate = RgbImage::from_fn(16, 16, |_, y| Rgb([if y == 0 { 32 } else { 0 }; 3]));
    let limits = Limits {
        pixel: 300.0,
        edge: 100.0,
        tile: 512.0,
    };
    let full = analysis.score(&candidate).unwrap();
    assert!(limits.accepts(&full));
    same(&streamed(&analysis, &candidate, limits).unwrap(), &full);
}

#[test]
fn alpha_analyses_never_use_the_opaque_shortcut() {
    let reference = Reference {
        pixels: RgbImage::from_pixel(17, 19, Rgb([80, 120, 170])),
        alpha: Some(image::GrayImage::from_fn(17, 19, |x, y| {
            image::Luma([(x * 71 + y * 29) as u8])
        })),
        source_format: "png",
        jpeg_metadata: Default::default(),
    };
    let analysis = Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
    assert!(OpaqueContext::new(&analysis, Strength::default().limits()).is_none());
}

#[test]
fn fused_mapping_matches_full_mapping_and_scoring_even_after_early_rejections() {
    for (width, height) in [(1, 1), (17, 19), (37, 33)] {
        for pattern in 0..2 {
            let reference = Reference {
                pixels: RgbImage::from_fn(width, height, |x, y| {
                    let p = [
                        (x * 71 + y * 23) as u8,
                        (x * 7 + y * 19) as u8,
                        (x * 43 + y * 13) as u8,
                    ];
                    Rgb(if pattern == 0 { p } else { [p[0]; 3] })
                }),
                alpha: None,
                source_format: "png",
                jpeg_metadata: Default::default(),
            };
            let analysis =
                Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
            for count in [4, 16, 64, 256] {
                let palette = palette::train(&reference, &analysis, count);
                for exact in [false, true] {
                    let mut old = palette::Mapper::new(&reference, &palette, exact);
                    let mut fused = palette::Mapper::new(&reference, &palette, exact);
                    for coherence in [0, 64, 256, 1024, 2048, 4096, 8192] {
                        let (indices, rgb) = old.map(&reference, &analysis, coherence);
                        let full = analysis.score(&rgb).unwrap();
                        for strength in [0, 1, 25, 50, 75, 100] {
                            let limits = Strength::new(strength).unwrap().limits();
                            let context = OpaqueContext::new(&analysis, limits).unwrap();
                            let result =
                                fused.map_scored(&reference, &analysis, coherence, &context);
                            assert_eq!(
                                result.is_some(),
                                limits.accepts(&full),
                                "{width}x{height}, pattern={pattern}, count={count}, exact={exact}, coherence={coherence}, strength={strength}"
                            );
                            if let Some((actual_indices, actual_rgb, score)) = result {
                                assert_eq!(actual_indices, indices);
                                assert_eq!(actual_rgb, rgb);
                                same(&score, &full);
                            }
                        }
                    }
                }
            }
        }
    }
}
