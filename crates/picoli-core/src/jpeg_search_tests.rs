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
