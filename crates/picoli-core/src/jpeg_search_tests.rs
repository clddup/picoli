use crate::{
    analysis::Analysis,
    optimize::{Candidate, Shape},
    quality::Strength,
    *,
};

// Frozen pre-memoization control: every trial really encodes/decodes a JPEG.
fn original(
    strength: Strength,
    mut evaluate: impl FnMut(u16) -> Result<Candidate, Error>,
) -> Result<Option<Candidate>, Error> {
    let limits = strength.limits();
    let finest = evaluate(1)?;
    if !limits.accepts(&finest.score) {
        return Ok(None);
    }
    let (mut low, mut high, mut boundary) = (1i32, 5000i32, 1i32);
    let mut result = finest;
    while low <= high {
        let scale = (low + high) / 2;
        let c = evaluate(scale as u16)?;
        if limits.accepts(&c.score) {
            boundary = scale;
            low = scale + 1;
            if c.bytes < result.bytes {
                result = c;
            }
        } else {
            high = scale - 1;
        }
    }
    for scale in (boundary - 4).max(1)..=(boundary + 4).min(5000) {
        let c = evaluate(scale as u16)?;
        if limits.accepts(&c.score) && (c.bytes, &c.id) < (result.bytes, &result.id) {
            result = c;
        }
    }
    Ok(Some(result))
}

#[test]
fn repeated_trial_elimination_matches_real_codec_control_including_ties_and_rejections() {
    let images = [
        RgbImage::from_pixel(17, 13, image::Rgb([40, 80, 160])),
        RgbImage::from_fn(37, 23, |x, y| {
            image::Rgb([(x * 7) as u8, (y * 11) as u8, (x + y * 13) as u8])
        }),
        RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 4) as u8, (x + y) as u8])
        }),
    ];
    let (mut old_total, mut new_total) = (0usize, 0usize);
    for pixels in images {
        let reference = Reference {
            pixels,
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let analysis =
            Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
        for strength in [0, 50, 100] {
            let strength = Strength::new(strength).unwrap();
            for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
                for sampling in [444, 422, 420] {
                    let (mut old_calls, mut new_calls) = (Vec::new(), Vec::new());
                    let before = original(strength, |s| {
                        old_calls.push(s);
                        jpeg::candidate(&reference, &analysis, shape, sampling, s)
                    })
                    .unwrap();
                    let after = jpeg_search::run(strength, |s| {
                        new_calls.push(s);
                        jpeg::candidate(&reference, &analysis, shape, sampling, s)
                    })
                    .unwrap();
                    assert!(new_calls.len() <= old_calls.len());
                    old_total += old_calls.len();
                    new_total += new_calls.len();
                    match (before, after) {
                        (None, None) => {}
                        (Some(a), Some(b)) => {
                            assert_eq!(a.id, b.id);
                            assert_eq!(a.bytes, b.bytes);
                            assert_eq!(a.data, b.data);
                            assert_eq!(a.score.loss, b.score.loss);
                        }
                        _ => panic!("memoization changed candidate admission"),
                    }
                }
            }
        }
    }
    assert!(new_total < old_total);
    eprintln!("real JPEG encodes across 81 control pairs: {old_total} -> {new_total}");
}

fn assert_candidate_exact(a: &Candidate, b: &Candidate) {
    assert_eq!(a.id, b.id);
    assert_eq!(a.bytes, b.bytes);
    assert_eq!(a.data, b.data);
    assert_eq!(format!("{:?}", a.parameters), format!("{:?}", b.parameters));
    for (a, b) in [
        (a.score.weighted_pixel_error, b.score.weighted_pixel_error),
        (a.score.weighted_edge_error, b.score.weighted_edge_error),
        (a.score.worst_tile_error, b.score.worst_tile_error),
        (a.score.loss, b.score.loss),
    ] {
        assert_eq!(a.to_bits(), b.to_bits());
    }
}

#[test]
fn bounded_jpeg_trials_and_search_match_full_scoring_bit_for_bit() {
    let mut rejected = 0;
    let mut accepted = 0;
    for (width, height) in [(1, 1), (7, 19), (35, 17), (64, 33)] {
        let reference = Reference {
            pixels: RgbImage::from_fn(width, height, |x, y| {
                if x < width / 2 {
                    image::Rgb([((x * 13 + y * 3) % 256) as u8, ((y * 7) % 256) as u8, 180])
                } else {
                    let v = if (x / 2 + y / 3) % 2 == 0 { 15 } else { 240 };
                    image::Rgb([v, v, v])
                }
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        };
        let analysis =
            Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
        for value in [0, 50, 80, 100] {
            let strength = Strength::new(value).unwrap();
            let limits = strength.limits();
            for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
                for sampling in [444, 422, 420] {
                    let before = original(strength, |scale| {
                        jpeg::candidate(&reference, &analysis, shape, sampling, scale)
                    })
                    .unwrap();
                    let mut full_calls = Vec::new();
                    let full = jpeg_search::run(strength, |scale| {
                        full_calls.push(scale);
                        jpeg::candidate(&reference, &analysis, shape, sampling, scale)
                    })
                    .unwrap();
                    let mut bounded_calls = Vec::new();
                    let bounded = jpeg_search::run_bounded(strength, |scale| {
                        bounded_calls.push(scale);
                        let full = jpeg::candidate(&reference, &analysis, shape, sampling, scale)?;
                        let bounded = jpeg::candidate_bounded(
                            &reference, &analysis, shape, sampling, scale, limits,
                        )?;
                        assert_eq!(bounded.is_some(), limits.accepts(&full.score));
                        if let Some(candidate) = &bounded {
                            accepted += 1;
                            assert_candidate_exact(&full, candidate);
                        } else {
                            rejected += 1;
                        }
                        Ok(bounded)
                    })
                    .unwrap();
                    assert_eq!(full_calls, bounded_calls);
                    match (before, full, bounded) {
                        (None, None, None) => {}
                        (Some(a), Some(b), Some(c)) => {
                            assert_candidate_exact(&a, &b);
                            assert_candidate_exact(&a, &c);
                        }
                        _ => panic!("bounded scoring changed JPEG search admission"),
                    }
                }
            }
        }
    }
    assert!(rejected > 0);
    assert!(accepted > 0);
}
