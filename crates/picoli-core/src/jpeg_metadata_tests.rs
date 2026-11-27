use crate::{analysis::Analysis, jpeg::Shape, optimize::Job, quality::Strength, *};

fn plain() -> Vec<u8> {
    encode_jpeg_baseline(
        &Reference {
            pixels: RgbImage::from_fn(37, 23, |x, y| {
                image::Rgb([(x * 7) as u8, (y * 11) as u8, (x + y * 13) as u8])
            }),
            alpha: None,
            source_format: "test",
            jpeg_metadata: Default::default(),
        },
        95,
    )
    .unwrap()
}
fn app(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xff, marker];
    out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    out.extend_from_slice(payload);
    out
}
fn insert(plain: &[u8], segments: &[u8]) -> Vec<u8> {
    [&plain[..2], segments, &plain[2..]].concat()
}
fn exif(orientation: u16) -> Vec<u8> {
    let mut p = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
    p.extend_from_slice(&orientation.to_le_bytes());
    p.extend_from_slice(&[0; 6]);
    app(0xe1, &p)
}
fn icc(index: u8, count: u8, data: &[u8]) -> Vec<u8> {
    let mut p = b"ICC_PROFILE\0".to_vec();
    p.extend_from_slice(&[index, count]);
    p.extend_from_slice(data);
    app(0xe2, &p)
}
fn metadata_source() -> Vec<u8> {
    let profile = include_bytes!("../tests/fixtures/srgb.icc");
    let headers = [
        exif(1),
        icc(1, 2, &profile[..300]),
        icc(2, 2, &profile[300..]),
    ]
    .concat();
    insert(&plain(), &headers)
}

#[test]
fn rgb_icc_identity_exif_survive_baseline_and_all_jpeg_search_families() {
    let source = metadata_source();
    let reference = decode_baseline(&source).unwrap();
    let original = decode_baseline(&plain()).unwrap();
    assert_eq!(reference.pixels, original.pixels);
    let analysis = Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
    let expected = format!("{:?}", reference.jpeg_metadata);
    assert!(!expected.contains("ICC_PROFILE"));
    let baseline = encode_jpeg_baseline(&reference, 80).unwrap();
    let check = |data: &[u8]| {
        let decoded = decode_baseline(data).unwrap();
        assert_eq!(decoded.pixels.dimensions(), (37, 23));
        // Byte-exact EXIF and ICC segments, including chunk boundaries/order.
        let once = reference.jpeg_metadata.attach(plain()).unwrap();
        let twice = decoded.jpeg_metadata.attach(plain()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(decoded.jpeg_metadata.attach(twice.clone()).unwrap(), twice);
    };
    check(&baseline);
    for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
        for sampling in [444, 422, 420] {
            let candidate = jpeg::candidate(&reference, &analysis, shape, sampling, 60).unwrap();
            assert_eq!(candidate.bytes, candidate.data.len());
            check(&candidate.data);
        }
    }
    let result = quality::compress(&source, Strength::default()).unwrap();
    assert!(result.bytes <= source.len());
    check(&result.data);
    assert_eq!(
        quality::compress(&source, Strength::new(0).unwrap())
            .unwrap()
            .data,
        source
    );
    assert!(encode_png_lossless(&reference).is_err());
    for job in [
        Job::PngLossless,
        Job::PngPredictive,
        Job::Png { colors: 16 },
    ] {
        assert!(quality::run_job(&reference, &analysis, job, Strength::default()).is_err());
        assert!(optimize::run_job(&reference, &analysis, job, 10000).is_err());
    }
}

#[test]
fn physical_density_and_nonsquare_aspect_are_preserved_without_duplicate_jfif() {
    let p = plain();
    // Remove the original canonical JFIF header before installing a display JFIF.
    assert_eq!(&p[2..4], &[0xff, 0xe0]);
    let end = 4 + u16::from_be_bytes([p[4], p[5]]) as usize;
    for (unit, x, y) in [(1u8, 144u16, 72u16), (2, 300, 300), (0, 2, 1)] {
        let mut jfif = b"JFIF\0\x01\x02".to_vec();
        jfif.push(unit);
        jfif.extend_from_slice(&x.to_be_bytes());
        jfif.extend_from_slice(&y.to_be_bytes());
        jfif.extend_from_slice(&[0, 0]);
        let source = [&p[..2], &app(0xe0, &jfif), &p[end..]].concat();
        let reference = decode_baseline(&source).unwrap();
        let output = encode_jpeg_baseline(&reference, 80).unwrap();
        assert_eq!(output.windows(5).filter(|w| *w == b"JFIF\0").count(), 1);
        assert_eq!(&output[6..20], &jfif);
        assert_eq!(
            reference.jpeg_metadata.attach(output.clone()).unwrap(),
            output
        );
    }
}

#[test]
fn coefficient_refinement_preserves_display_metadata_and_zero_bypass() {
    let mut source = metadata_source();
    assert_eq!(&source[2..4], &[0xff, 0xe1]);
    // Install a nondefault, nonsquare JFIF density as well as split ICC/EXIF.
    let jfif_offset = source.windows(5).position(|w| w == b"JFIF\0").unwrap();
    source[jfif_offset + 7] = 1;
    source[jfif_offset + 8..jfif_offset + 10].copy_from_slice(&144u16.to_be_bytes());
    source[jfif_offset + 10..jfif_offset + 12].copy_from_slice(&72u16.to_be_bytes());
    let snapshot = source.clone();
    let reference = decode_baseline(&source).unwrap();
    let analysis = Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
    let metadata = reference.jpeg_metadata.attach(plain()).unwrap();
    for sampling in [444, 422, 420] {
        for scans in [2, 4] {
            let options = jpeg::Encoding {
                shape: Shape::Standard,
                sampling,
                scale: 60,
                scans,
                entropy: jpeg::Entropy::ScanHuffman,
                q_percent: 100,
                rdo_lambda: 0,
                weighted_sampling: false,
            };
            let control = jpeg::encode_entropy(
                &reference,
                &analysis,
                Shape::Standard,
                sampling,
                60,
                scans,
                jpeg::Entropy::ScanHuffman,
            )
            .unwrap();
            assert_eq!(
                jpeg::encode_config(&reference, &analysis, options).unwrap(),
                control
            );
            for (q_percent, rdo_lambda, weighted_sampling) in
                [(95, 256, false), (90, 1024, false), (95, 256, true)]
            {
                let data = jpeg::encode_config(
                    &reference,
                    &analysis,
                    jpeg::Encoding {
                        q_percent,
                        rdo_lambda,
                        weighted_sampling,
                        ..options
                    },
                )
                .unwrap();
                let decoded = decode_baseline(&data).unwrap();
                assert_eq!(decoded.pixels.dimensions(), reference.pixels.dimensions());
                assert_eq!(decoded.alpha, reference.alpha);
                assert_eq!(decoded.jpeg_metadata.attach(plain()).unwrap(), metadata);
            }
            for q_percent in [0, 101] {
                assert!(
                    jpeg::encode_config(
                        &reference,
                        &analysis,
                        jpeg::Encoding {
                            q_percent,
                            ..options
                        }
                    )
                    .is_err()
                );
            }
        }
    }
    assert_eq!(source, snapshot);
    assert_eq!(
        quality::compress(&source, Strength::new(0).unwrap())
            .unwrap()
            .data,
        source
    );
}

#[test]
fn malformed_rotated_duplicate_late_and_incomplete_metadata_are_rejected() {
    let p = plain();
    for orientation in [0, 2, 3, 4, 5, 6, 7, 8, 9, 256] {
        assert!(decode_baseline(&insert(&p, &exif(orientation))).is_err());
    }
    for data in [
        app(0xe1, b"Exif\0\0bad"),
        [exif(1), exif(1)].concat(),
        icc(1, 2, b"incomplete"),
        icc(0, 1, b"bad index"),
        [icc(1, 2, b"first"), icc(1, 2, b"duplicate")].concat(),
        [icc(1, 2, b"first"), icc(2, 3, b"inconsistent")].concat(),
        icc(1, 1, b"invalid ICC bytes"),
        app(0xe2, b"MPF\0unsupported gain map"),
    ] {
        assert!(decode_baseline(&insert(&p, &data)).is_err());
    }
    let late = [&p[..p.len() - 2], &exif(1), &p[p.len() - 2..]].concat();
    assert!(decode_baseline(&late).is_err());
    let valid = metadata_source();
    for length in 0..valid.len() {
        assert!(
            decode_baseline(&valid[..length]).is_err(),
            "truncation at {length}"
        );
    }
    let mut cmyk = include_bytes!("../tests/fixtures/srgb.icc").to_vec();
    cmyk[16..20].copy_from_slice(b"CMYK");
    assert!(decode_baseline(&insert(&p, &icc(1, 1, &cmyk))).is_err());
}

#[test]
fn scan_layouts_preserve_pixels_metadata_and_finishing_is_idempotent() {
    for source in [plain(), metadata_source()] {
        let reference = decode_baseline(&source).unwrap();
        let analysis =
            Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
        for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
            for sampling in [444, 422, 420] {
                for scale in [1, 60, 1500] {
                    let before =
                        jpeg::encode_scans(&reference, &analysis, shape, sampling, scale, 4)
                            .unwrap();
                    let expected = decode_baseline(&before).unwrap();
                    for scans in [2, 4] {
                        let after = jpeg::encode_scans(
                            &reference, &analysis, shape, sampling, scale, scans,
                        )
                        .unwrap();
                        let decoded = decode_baseline(&after).unwrap();
                        assert_eq!(
                            decoded.pixels, expected.pixels,
                            "sampling={sampling}, scale={scale}, scans={scans}"
                        );
                        assert_eq!(
                            decoded.jpeg_metadata.attach(plain()).unwrap(),
                            expected.jpeg_metadata.attach(plain()).unwrap()
                        );
                    }
                }
            }
        }
        assert!(jpeg::encode_scans(&reference, &analysis, Shape::Standard, 444, 60, 1).is_err());
        // Sequential vs progressive failed exact pixel identity at this scale.
        // That feature is withdrawn, not admitted by relaxing the pixel assertion.
        assert!(jpeg::encode_scans(&reference, &analysis, Shape::Standard, 444, 1500, 0).is_err());
        // A finishing pass must not make an inadmissible-only list eligible.
        let rejected = jpeg::candidate(&reference, &analysis, Shape::Detail, 420, 60).unwrap();
        let strength = Strength::new(100).unwrap();
        assert!(!strength.limits().accepts(&rejected.score));
        let mut rejected_only = vec![rejected];
        quality::refine_jpeg_candidates(&reference, &analysis, &mut rejected_only, strength)
            .unwrap();
        assert_eq!(rejected_only.len(), 1);
        assert!(quality::best(rejected_only.iter(), strength).is_none());
        // Full chroma and finest quantization provide a valid positive fixture.
        let first = jpeg::candidate(&reference, &analysis, Shape::Detail, 444, 1).unwrap();
        let before = decode_baseline(&first.data).unwrap();
        let size = first.bytes;
        let mut candidates = vec![first];
        quality::refine_jpeg_candidates(
            &reference,
            &analysis,
            &mut candidates,
            Strength::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        let strength = Strength::new(100).unwrap();
        quality::refine_jpeg_candidates(&reference, &analysis, &mut candidates, strength).unwrap();
        let winner = quality::best(candidates.iter(), strength).unwrap();
        assert!(winner.bytes <= size);
        assert_eq!(decode_baseline(&winner.data).unwrap().pixels, before.pixels);
        let once = winner.data.clone();
        let count = candidates.len();
        quality::refine_jpeg_candidates(&reference, &analysis, &mut candidates, strength).unwrap();
        assert_eq!(candidates.len(), count);
        assert_eq!(
            quality::best(candidates.iter(), strength).unwrap().data,
            once
        );
    }
}

#[test]
fn eob_run_coding_preserves_pixels_metadata_and_zero_strength() {
    for source in [plain(), metadata_source()] {
        let reference = decode_baseline(&source).unwrap();
        let analysis =
            Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
        for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
            for sampling in [444, 422, 420] {
                for scale in [1, 60, 1500] {
                    for scans in [2, 4] {
                        let old = jpeg::encode_options(
                            &reference, &analysis, shape, sampling, scale, scans, false,
                        )
                        .unwrap();
                        let new = jpeg::encode_options(
                            &reference, &analysis, shape, sampling, scale, scans, true,
                        )
                        .unwrap();
                        let a = decode_baseline(&old).unwrap();
                        let b = decode_baseline(&new).unwrap();
                        assert_eq!(a.pixels, b.pixels);
                        assert_eq!(
                            a.jpeg_metadata.attach(plain()).unwrap(),
                            b.jpeg_metadata.attach(plain()).unwrap()
                        );
                    }
                }
            }
        }
        let first = jpeg::candidate(&reference, &analysis, Shape::Detail, 444, 1).unwrap();
        let expected = decode_baseline(&first.data).unwrap().pixels;
        let size = first.bytes;
        let mut candidates = vec![first];
        quality::refine_jpeg_eob_candidates(
            &reference,
            &analysis,
            &mut candidates,
            Strength::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        let strength = Strength::new(100).unwrap();
        quality::refine_jpeg_eob_candidates(&reference, &analysis, &mut candidates, strength)
            .unwrap();
        let winner = quality::best(candidates.iter(), strength).unwrap();
        assert!(winner.bytes <= size);
        assert_eq!(decode_baseline(&winner.data).unwrap().pixels, expected);
        let once = winner.data.clone();
        let count = candidates.len();
        quality::refine_jpeg_eob_candidates(&reference, &analysis, &mut candidates, strength)
            .unwrap();
        assert_eq!(candidates.len(), count);
        assert_eq!(
            quality::best(candidates.iter(), strength).unwrap().data,
            once
        );
    }
}

#[test]
fn scan_huffman_coding_preserves_pixels_metadata_and_finishing_guards() {
    for source in [plain(), metadata_source()] {
        let reference = decode_baseline(&source).unwrap();
        let analysis =
            Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
        for shape in [Shape::Standard, Shape::Detail, Shape::Spectrum] {
            for sampling in [444, 422, 420] {
                for scale in [1, 60, 1500] {
                    for scans in [2, 4] {
                        let old = jpeg::encode_scans(
                            &reference, &analysis, shape, sampling, scale, scans,
                        )
                        .unwrap();
                        let new = jpeg::encode_entropy(
                            &reference,
                            &analysis,
                            shape,
                            sampling,
                            scale,
                            scans,
                            jpeg::Entropy::ScanHuffman,
                        )
                        .unwrap();
                        let a = decode_baseline(&old).unwrap();
                        let b = decode_baseline(&new).unwrap();
                        assert_eq!(a.pixels, b.pixels);
                        assert_eq!(a.alpha, b.alpha);
                        assert_eq!(
                            a.jpeg_metadata.attach(plain()).unwrap(),
                            b.jpeg_metadata.attach(plain()).unwrap()
                        );
                    }
                }
            }
        }
        let rejected = jpeg::candidate(&reference, &analysis, Shape::Detail, 420, 60).unwrap();
        let strength = Strength::new(100).unwrap();
        assert!(!strength.limits().accepts(&rejected.score));
        let mut rejected_only = vec![rejected];
        quality::refine_jpeg_scan_huffman_candidates(
            &reference,
            &analysis,
            &mut rejected_only,
            strength,
        )
        .unwrap();
        assert_eq!(rejected_only.len(), 1);
        assert!(quality::best(rejected_only.iter(), strength).is_none());
    }
    let reference = crate::Reference {
        pixels: image::RgbImage::from_pixel(257, 129, image::Rgb([32, 64, 128])),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    };
    let analysis = Analysis::for_reference(&reference, resources::DEFAULT_MEMORY_BYTES).unwrap();
    let first = jpeg::candidate(&reference, &analysis, Shape::Detail, 444, 1).unwrap();
    let expected = decode_baseline(&first.data).unwrap().pixels;
    let size = first.bytes;
    let mut candidates = vec![first];
    quality::refine_jpeg_scan_huffman_candidates(
        &reference,
        &analysis,
        &mut candidates,
        Strength::new(0).unwrap(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 1);
    let strength = Strength::default();
    quality::refine_jpeg_scan_huffman_candidates(&reference, &analysis, &mut candidates, strength)
        .unwrap();
    let winner = quality::best(candidates.iter(), strength).unwrap();
    assert!(winner.bytes < size);
    assert!(matches!(
        winner.parameters,
        crate::optimize::Parameters::Jpeg {
            scan_huffman: true,
            ..
        }
    ));
    assert_eq!(decode_baseline(&winner.data).unwrap().pixels, expected);
    let once = winner.data.clone();
    let count = candidates.len();
    quality::refine_jpeg_scan_huffman_candidates(&reference, &analysis, &mut candidates, strength)
        .unwrap();
    assert_eq!(candidates.len(), count);
    assert_eq!(
        quality::best(candidates.iter(), strength).unwrap().data,
        once
    );
}

#[test]
fn metadata_budget_reduces_concurrency_and_is_checked_before_admission() {
    let without = resources::Plan::new(1000, 1000, 1000, 256 * 1024 * 1024, 8).unwrap();
    let with = resources::Plan::with_metadata(1000, 1000, 1000, 1024 * 1024, 256 * 1024 * 1024, 8)
        .unwrap();
    assert!(with.workers < without.workers);
    assert!(with.estimated_shared_bytes > without.estimated_shared_bytes);
    assert!(with.estimated_worker_bytes > without.estimated_worker_bytes);
    let source = metadata_source();
    assert!(JpegMetadata::extract(&source, 16 * 1024).is_err());
    assert!(JpegMetadata::extract(&source, 128 * 1024).is_ok());
    assert!(resources::Plan::with_metadata(1, 1, 0, usize::MAX, u64::MAX, 1).is_err());
}
