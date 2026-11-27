use super::*;
use crate::optimize::Shape;
use image::{Rgb, RgbImage};
use jpeg_encoder::{ColorType, Encoder, QuantizationTableType, SamplingFactor};

fn parameters(sampling: u16, coding: Coding, lambda: u32) -> Parameters {
    Parameters::Jpeg {
        shape: Shape::Standard,
        sampling,
        quantization_scale: 8,
        scans_per_component: coding.scans,
        eob_runs: coding.eob_runs,
        scan_huffman: coding.scan_huffman,
        coefficient_rdo_lambda: lambda,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
    }
}
fn generated(reference: &Reference, sampling: u16, coding: Coding, lambda: u32) -> Vec<u8> {
    let mut data = Vec::new();
    let mut e = Encoder::new(&mut data, 100);
    e.set_sampling_factor(match sampling {
        444 => SamplingFactor::R_4_4_4,
        422 => SamplingFactor::R_4_2_2,
        420 => SamplingFactor::R_4_2_0,
        _ => unreachable!(),
    });
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([8; 64])),
        QuantizationTableType::Custom(Box::new([8; 64])),
    );
    e.set_progressive_scans(coding.scans);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(coding.eob_runs);
    e.set_progressive_scan_huffman(coding.scan_huffman);
    e.set_experimental_ac_rdo_lambda(lambda);
    e.encode(
        reference.pixels.as_raw(),
        reference.pixels.width() as u16,
        reference.pixels.height() as u16,
        ColorType::Rgb,
    )
    .unwrap();
    reference.jpeg_metadata.attach(data).unwrap()
}
fn candidate(
    reference: &Reference,
    analysis: &Analysis,
    data: Vec<u8>,
    sampling: u16,
    coding: Coding,
    lambda: u32,
) -> Candidate {
    let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes).unwrap();
    assert_eq!(decoded.pixels.dimensions(), reference.pixels.dimensions());
    Candidate {
        id: "generated-control".into(),
        parameters: parameters(sampling, coding, lambda),
        bytes: data.len(),
        data,
        score: analysis.score(&decoded.pixels).unwrap(),
    }
}
fn template(width: u32, height: u32) -> Reference {
    Reference {
        pixels: RgbImage::from_pixel(width, height, Rgb([97, 127, 167])),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    }
}
fn coding() -> Coding {
    Coding {
        scans: 4,
        eob_runs: true,
        scan_huffman: true,
    }
}

// Frozen v80 stage ordering/math/selection oracle, without its diagnostic-only
// worst-case admission gate or cloned diagnostic outputs. Uses unbounded IO and
// the frozen unbounded model entry point, independently of product admission.
fn frozen_alternatives(
    reference: &Reference,
    analysis: &Analysis,
    current: &[u8],
    coding: Coding,
) -> [(Vec<u8>, Score); 2] {
    let captured = coefficient_io::capture(current, analysis.memory_budget_bytes).unwrap();
    assert_eq!(
        coefficient_io::serialize(reference, &captured.frame, coding).unwrap(),
        current
    );
    let model = model::build(
        reference,
        analysis,
        &captured.pixels,
        &captured.frame,
        coding.scans,
    )
    .unwrap();
    [false, true].map(|paired| {
        let solution = solver::solve(&model.curves, paired);
        let mut frame = coefficient_io::capture(current, analysis.memory_budget_bytes)
            .unwrap()
            .frame;
        model::apply(&model, &solution, &mut frame).unwrap();
        let data = coefficient_io::serialize(reference, &frame, coding).unwrap();
        let decoded = crate::decode_with_budget(&data, analysis.memory_budget_bytes).unwrap();
        (data, analysis.score(&decoded.pixels).unwrap())
    })
}
fn damaged_constant(sampling: u16, coding: Coding) -> (Reference, Vec<u8>, Vec<u8>) {
    let t = template(33, 35);
    let original = generated(&t, sampling, coding, 0);
    let mut frame = coefficient_io::capture(&original, 1024 * 1024 * 1024)
        .unwrap()
        .frame;
    let source = coefficient_io::serialize(&t, &frame, coding).unwrap();
    let reference = crate::decode_baseline(&source).unwrap();
    for by in 0..35usize.div_ceil(8) {
        for bx in 0..33usize.div_ceil(8) {
            let start = (by * frame.components[0].block_width + bx) * 64;
            assert_eq!(frame.components[0].coefficients[start + 1], 0);
            frame.components[0].coefficients[start + 1] = 1;
        }
    }
    let current = coefficient_io::serialize(&reference, &frame, coding).unwrap();
    (reference, source, current)
}

#[test]
fn actual_finisher_matches_v80_changes_tables_keeps_dc_chroma_and_every_old_candidate() {
    let coding = coding();
    let (reference, source, current) = damaged_constant(444, coding);
    let analysis = Analysis::new(&reference.pixels).unwrap();
    let [plain, raw] = frozen_alternatives(&reference, &analysis, &current, coding);
    let before = coefficient_io::capture(&current, analysis.memory_budget_bytes).unwrap();
    let control_score = analysis.score(&before.pixels).unwrap();
    assert!(plain.0.len() < current.len());
    assert!(raw.0.len() < current.len());
    assert!(nonworse(&plain.1, &control_score, Strength::default()));
    assert!(nonworse(&raw.1, &control_score, Strength::default()));
    let expected = if raw.0.len() < plain.0.len() {
        &raw.0
    } else {
        &plain.0
    };
    let mut candidates = vec![candidate(&reference, &analysis, current, 444, coding, 0)];
    // Retain a losing candidate as well, including spare capacity.
    let mut loser = quality::original(&reference, &analysis, &source).unwrap();
    loser.data.extend_from_slice(&[0; 1024]);
    loser.bytes = loser.data.len();
    loser.data.reserve_exact(4096);
    candidates.push(loser);
    let controls = format!("{candidates:?}");
    let data_before: Vec<_> = candidates
        .iter()
        .map(|c| (c.data.clone(), c.data.capacity()))
        .collect();
    quality::refine_jpeg_table_candidates(
        &source,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 3);
    assert_eq!(format!("{:?}", &candidates[..2]), controls);
    for (candidate, (data, capacity)) in candidates.iter().zip(&data_before) {
        assert_eq!(&candidate.data, data);
        assert_eq!(candidate.data.capacity(), *capacity);
    }
    let selected = &candidates[2];
    assert_eq!(&selected.data, expected);
    assert_eq!(selected.format(), "jpeg");
    let Parameters::JpegTableAllocated {
        luma_quantization_table,
        rewritten_frequencies,
        allocation,
        ..
    } = &selected.parameters
    else {
        panic!("missing literal table provenance");
    };
    assert!(!rewritten_frequencies.is_empty());
    assert_eq!(
        allocation,
        &if raw.0.len() < plain.0.len() {
            "paired_luma_tables"
        } else {
            "single_luma_tables"
        }
    );
    // JSON tag/provenance is asserted by the CLI test using its existing serde_json dependency.
    let after = coefficient_io::capture(&selected.data, analysis.memory_budget_bytes)
        .unwrap()
        .frame;
    assert_ne!(
        before.frame.components[0].quantization_table,
        after.components[0].quantization_table
    );
    assert_eq!(
        luma_quantization_table,
        &after.components[0].quantization_table.to_vec()
    );
    assert_eq!(
        before.frame.components[0].quantization_table[0],
        after.components[0].quantization_table[0]
    );
    for (index, (a, b)) in before
        .frame
        .components
        .iter()
        .zip(&after.components)
        .enumerate()
    {
        assert_eq!(a.horizontal_sampling_factor, b.horizontal_sampling_factor);
        assert_eq!(a.vertical_sampling_factor, b.vertical_sampling_factor);
        for (x, y) in a
            .coefficients
            .chunks_exact(64)
            .zip(b.coefficients.chunks_exact(64))
        {
            assert_eq!(x[0], y[0]);
            if index > 0 {
                assert_eq!(x, y);
            }
        }
        if index > 0 {
            assert_eq!(a.quantization_table, b.quantization_table);
        }
    }
    assert_eq!(
        crate::decode_baseline(&selected.data).unwrap().pixels,
        reference.pixels
    );
    let count = candidates.len();
    quality::refine_jpeg_table_candidates(
        &source,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(
        candidates.len(),
        count,
        "already finalized winner is not rewritten"
    );
}

#[test]
fn admitted_bytes_match_frozen_v80_for_both_scans_all_sampling_and_rdo_controls() {
    for sampling in [444, 422, 420] {
        for scans in [2, 4] {
            for lambda in [0, 256] {
                let mut reference = template(19, 25);
                reference.pixels = RgbImage::from_fn(19, 25, |x, y| {
                    Rgb([(x * 7 + y * 3) as u8, (y * 9) as u8, (x * 3 + y * 5) as u8])
                });
                let coding = Coding { scans, ..coding() };
                let data = generated(&reference, sampling, coding, lambda);
                let analysis = Analysis::new(&reference.pixels).unwrap();
                let mut candidates = vec![candidate(
                    &reference,
                    &analysis,
                    data.clone(),
                    sampling,
                    coding,
                    lambda,
                )];
                let [(plain, ps), (raw, rs)] =
                    frozen_alternatives(&reference, &analysis, &data, coding);
                let control = &candidates[0].score;
                let pa = plain.len() < data.len() && nonworse(&ps, control, Strength::default());
                let ra = raw.len() < data.len() && nonworse(&rs, control, Strength::default());
                let expected = if ra && (!pa || raw.len() < plain.len()) {
                    &raw
                } else if pa {
                    &plain
                } else {
                    &data
                };
                // Assert the unchanged v81 stage itself. Conditional repair is
                // separately tested against this COMPLETE prefix and frozen v84.
                refine_original(
                    &data,
                    &reference,
                    &analysis,
                    &mut candidates,
                    Strength::default(),
                )
                .unwrap();
                assert_eq!(
                    &quality::best(candidates.iter(), Strength::default())
                        .unwrap()
                        .data,
                    expected
                );
                assert_eq!(candidates.len(), if pa || ra { 2 } else { 1 });
            }
        }
    }
}

#[test]
fn actual_quality_checks_use_all_components_and_caller_strength() {
    let control = Score {
        weighted_pixel_error: 10.0,
        weighted_edge_error: 10.0,
        worst_tile_error: 10.0,
        loss: 0.0,
    };
    assert!(nonworse(&control, &control, Strength::default()));
    for component in 0..3 {
        let mut bad = control.clone();
        match component {
            0 => bad.weighted_pixel_error += 0.001,
            1 => bad.weighted_edge_error += 0.001,
            _ => bad.worst_tile_error += 0.001,
        }
        assert!(!nonworse(&bad, &control, Strength::new(100).unwrap()));
    }
    assert!(!nonworse(&control, &control, Strength::new(1).unwrap()));
    let (reference, source, current) = damaged_constant(420, coding());
    let analysis = Analysis::new(&reference.pixels).unwrap();
    let mut candidates = vec![candidate(&reference, &analysis, current, 420, coding(), 0)];
    candidates[0].score = analysis.score(&reference.pixels).unwrap(); // Force eligibility; actual capture must govern acceptance.
    quality::refine_jpeg_table_candidates(
        &source,
        &reference,
        &analysis,
        &mut candidates,
        Strength::new(1).unwrap(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(
        Strength::new(1)
            .unwrap()
            .limits()
            .accepts(&candidates[1].score)
    );
}

#[test]
fn bounded_serializer_is_exact_and_only_expected_size_exhaustion_is_optional() {
    let r = template(19, 25);
    let current = generated(&r, 420, coding(), 0);
    let frame = coefficient_io::capture(&current, 1024 * 1024 * 1024)
        .unwrap()
        .frame;
    let exact = coefficient_io::serialize(&r, &frame, coding()).unwrap();
    assert_eq!(
        coefficient_io::serialize_bounded(&r, &frame, coding(), exact.len())
            .unwrap()
            .unwrap(),
        exact
    );
    assert!(
        coefficient_io::serialize_bounded(&r, &frame, coding(), exact.len() - 1)
            .unwrap()
            .is_none()
    );
    assert!(
        coefficient_io::serialize_bounded(
            &r,
            &frame,
            Coding {
                scans: 3,
                ..coding()
            },
            exact.len()
        )
        .is_err()
    );
    let mut malformed = frame;
    malformed.components[0].coefficients[1] = 1024;
    assert!(
        coefficient_io::serialize_bounded(&r, &malformed, coding(), 1).is_err(),
        "logic errors before output are not size rejection"
    );
}

#[test]
fn v71_padding_and_width_dependent_decoder_scratch_are_preserved() {
    use resources::{decoder_working, payload};
    assert_eq!(decoder_working(65535, 444), Some(3_276_800));
    assert_eq!(decoder_working(65535, 422), Some(6_291_456));
    assert_eq!(decoder_working(65535, 420), Some(10_092_544));
    assert_eq!(decoder_working(1, 444), Some(400));
    assert_eq!(decoder_working(1, 422), Some(1536));
    assert_eq!(decoder_working(1, 420), Some(2464));
    assert!(decoder_working(1, 411).is_none());
    assert_eq!(payload(65535, 1, 420), Some(3 * 1024 * 1024));
    for (width, height) in [(1, 19), (19, 1), (19, 25), (33, 35)] {
        let r = template(width, height);
        for sampling in [444, 422, 420] {
            let data = generated(&r, sampling, coding(), 0);
            let captured = coefficient_io::capture_generated(
                &data,
                &r,
                sampling,
                payload(width, height, sampling).unwrap(),
            )
            .unwrap();
            let actual: usize = captured
                .frame
                .components
                .iter()
                .map(|c| c.coefficients.capacity() * 2)
                .sum();
            assert_eq!(actual as u64, payload(width, height, sampling).unwrap());
        }
    }
}

fn dummy(reference: &Reference, analysis: &Analysis, sampling: u16) -> Candidate {
    Candidate {
        id: "intentionally-undecodable".into(),
        parameters: parameters(sampling, coding(), 0),
        data: vec![0],
        bytes: 1,
        score: analysis.score(&reference.pixels).unwrap(),
    }
}
#[test]
fn preflight_rejects_wide_thin_before_capture_and_counts_all_retained_capacity() {
    let r = template(65535, 1);
    let a = Analysis::for_reference(&r, 16 * 1024 * 1024).unwrap();
    let mut candidates = vec![dummy(&r, &a, 420)];
    assert!(resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 420).is_none());
    quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].data, [0]);
    let r = template(33, 35);
    let mut a = Analysis::new(&r.pixels).unwrap();
    let mut candidates = vec![dummy(&r, &a, 444), dummy(&r, &a, 444)];
    let before = resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 444).unwrap();
    candidates[1].data.reserve_exact(2 * 1024 * 1024);
    let after = resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 444).unwrap();
    assert_eq!(
        after.fixed_bytes - before.fixed_bytes,
        (candidates[1].data.capacity() - 1) as u64
    );
    a.memory_budget_bytes = after.fixed_bytes + 63 * resources::HISTOGRAM_ENTRY_BYTES;
    assert_eq!(
        resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 444)
            .unwrap()
            .histogram_entries,
        63
    );
    a.memory_budget_bytes -= 1;
    assert!(resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 444).is_none());
    quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
        .unwrap();
    assert_eq!(candidates.len(), 2);
}

#[test]
fn histogram_exhaustion_is_safe_noop_exact_entry_boundary_reproduces_model() {
    let mut r = template(19, 25);
    r.pixels = RgbImage::from_fn(19, 25, |x, y| {
        Rgb([
            (x * 37 + y * 13) as u8,
            (x * 17 + y * 53) as u8,
            (x * y * 7 + 19) as u8,
        ])
    });
    let mut a = Analysis::new(&r.pixels).unwrap();
    let data = generated(&r, 420, coding(), 0);
    let captured = coefficient_io::capture(&data, a.memory_budget_bytes).unwrap();
    let full = model::build(&r, &a, &captured.pixels, &captured.frame, 4).unwrap();
    assert!(full.histogram_entries > 63);
    assert!(
        model::build_bounded(
            &r,
            &a,
            &captured.pixels,
            &captured.frame,
            4,
            full.histogram_entries - 1
        )
        .unwrap()
        .is_none()
    );
    let exact = model::build_bounded(
        &r,
        &a,
        &captured.pixels,
        &captured.frame,
        4,
        full.histogram_entries,
    )
    .unwrap()
    .unwrap();
    assert_eq!(full.raw, exact.raw);
    assert_eq!(full.curves, exact.curves);
    let mut candidates = vec![candidate(&r, &a, data.clone(), 420, coding(), 0)];
    candidates[0].score = a.score(&r.pixels).unwrap();
    let admission = resources::preflight(&data, &r, &a, &candidates, &candidates[0], 420).unwrap();
    a.memory_budget_bytes = admission.fixed_bytes
        + (full.histogram_entries as u64 - 1) * resources::HISTOGRAM_ENTRY_BYTES;
    quality::refine_jpeg_table_candidates(&data, &r, &a, &mut candidates, Strength::default())
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].data, data);
}

#[test]
fn skipped_types_zero_strength_and_malformed_admitted_control_are_distinct() {
    let mut r = template(19, 25);
    let a = Analysis::new(&r.pixels).unwrap();
    let mut candidates = vec![dummy(&r, &a, 444)];
    quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::new(0).unwrap())
        .unwrap();
    assert!(
        quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
            .is_err()
    );
    for parameters in [
        Parameters::Original {
            source_format: "jpeg",
        },
        Parameters::PngLossless,
        Parameters::JpegTableAllocated {
            control: Box::new(parameters(444, coding(), 0)),
            luma_quantization_table: vec![8; 64],
            rewritten_frequencies: vec![1],
            scans_per_component: 4,
            eob_runs: true,
            scan_huffman: true,
            model: "test",
            allocation: "test",
            exchanges: 1,
        },
    ] {
        candidates[0].parameters = parameters;
        quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
            .unwrap();
    }
    candidates[0].parameters = parameters(444, coding(), 0);
    r.source_format = "png";
    quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
        .unwrap();
    r.source_format = "jpeg";
    r.alpha = Some(image::GrayImage::new(19, 25));
    quality::refine_jpeg_table_candidates(&[0], &r, &a, &mut candidates, Strength::default())
        .unwrap();
    assert_eq!(candidates.len(), 1);
}

#[test]
fn ordinary_dimensions_can_be_admitted_at_unchanged_128mib_without_pixel_cutoffs() {
    let r = template(1000, 1000);
    let a = Analysis::new(&r.pixels).unwrap();
    let candidates = vec![dummy(&r, &a, 420)];
    let admission = resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 420).unwrap();
    assert!(admission.histogram_entries >= 63);
    assert_eq!(crate::resources::DEFAULT_MEMORY_BYTES, 128 * 1024 * 1024);
}

#[test]
fn repair_opportunity_is_strictly_below_post_pass_winner_not_old_control() {
    let before = 100;
    let after = 80;
    assert!(90 < before);
    assert!(!repair_opportunity(Some(90), after));
    assert!(repair_opportunity(Some(70), after));
    assert!(!repair_opportunity(Some(80), after));
    assert!(!repair_opportunity(None, after));
}

#[test]
fn conditional_trigger_uses_actual_rejections_below_post_v81_best_and_preserves_prefix() {
    let mut triggered = 0;
    let mut repaired = 0;
    let mut allocated_controls = 0;
    let mut rejected_but_no_opportunity = 0;
    for scene in 0..2 {
        for sampling in [444, 422, 420] {
            for scans in [2, 4] {
                for lambda in [0, 256] {
                    let (width, height) = if scene == 0 { (19, 25) } else { (37, 29) };
                    let mut r = template(width, height);
                    r.pixels = RgbImage::from_fn(width, height, |x, y| {
                        Rgb(if scene == 0 {
                            [(x * 7 + y * 3) as u8, (y * 9) as u8, (x * 3 + y * 5) as u8]
                        } else {
                            [
                                (x * 9 + y * 3) as u8,
                                (y * 9 + x * 2) as u8,
                                (x * 3 + y * 9) as u8,
                            ]
                        })
                    });
                    let coding = Coding { scans, ..coding() };
                    let data = generated(&r, sampling, coding, lambda);
                    let a = Analysis::new(&r.pixels).unwrap();
                    let old_raw = frozen_alternatives(&r, &a, &data, coding);
                    for percent in [0, 25, 50, 75] {
                        let strength = Strength::new(percent).unwrap();
                        let fresh =
                            || vec![candidate(&r, &a, data.clone(), sampling, coding, lambda)];
                        let mut prefix = fresh();
                        let control = prefix[0].score.clone();
                        let eligible = percent != 0 && strength.limits().accepts(&control);
                        let trigger =
                            refine_original(&data, &r, &a, &mut prefix, strength).unwrap();
                        let old_best = quality::best(prefix.iter(), strength);
                        let rejected: Vec<_> = old_raw
                            .iter()
                            .filter(|(_, score)| !nonworse(score, &control, strength))
                            .collect();
                        let expected_trigger = eligible
                            && old_best.is_some_and(|winner| {
                                rejected.iter().any(|(bytes, _)| bytes.len() < winner.bytes)
                            });
                        assert_eq!(
                            trigger, expected_trigger,
                            "sampling{sampling} scans{scans} lambda{lambda} strength{percent}"
                        );
                        if eligible && !rejected.is_empty() && !trigger {
                            rejected_but_no_opportunity += 1;
                        }
                        let prefix_snapshot = format!("{prefix:?}");
                        let prefix_caps: Vec<_> =
                            prefix.iter().map(|c| c.data.capacity()).collect();
                        let mut all = fresh();
                        quality::refine_jpeg_table_candidates(&data, &r, &a, &mut all, strength)
                            .unwrap();
                        assert_eq!(format!("{:?}", &all[..prefix.len()]), prefix_snapshot);
                        assert_eq!(
                            all[..prefix.len()]
                                .iter()
                                .map(|c| c.data.capacity())
                                .collect::<Vec<_>>(),
                            prefix_caps
                        );
                        assert!(all.len() <= prefix.len() + 1);
                        if !trigger {
                            assert_eq!(all.len(), prefix.len());
                            continue;
                        }
                        triggered += 1;
                        let current = old_best.unwrap();
                        if matches!(current.parameters, Parameters::JpegTableAllocated { .. }) {
                            allocated_controls += 1;
                        }
                        let frozen = test_oracle::run_coupled(
                            &r,
                            &current.data,
                            coding,
                            a.memory_budget_bytes,
                        )
                        .unwrap();
                        assert!(frozen.identity_file_exact);
                        let pa = frozen.plain.len() < current.bytes
                            && nonworse(&frozen.plain_score, &frozen.control_score, strength);
                        let ra = frozen.raw.len() < current.bytes
                            && nonworse(&frozen.raw_score, &frozen.control_score, strength);
                        let expected = if ra && (!pa || frozen.raw.len() < frozen.plain.len()) {
                            &frozen.raw
                        } else if pa {
                            &frozen.plain
                        } else {
                            &current.data
                        };
                        assert_eq!(&quality::best(all.iter(), strength).unwrap().data, expected);
                        assert_eq!(all.len(), prefix.len() + usize::from(pa || ra));
                        if pa || ra {
                            repaired += 1;
                            let last = all.last().unwrap();
                            let Parameters::JpegTableAllocated {
                                control: provenance,
                                model,
                                ..
                            } = &last.parameters
                            else {
                                panic!("repair provenance");
                            };
                            assert_eq!(
                                format!("{provenance:?}"),
                                format!("{:?}", current.parameters)
                            );
                            assert_eq!(
                                *model,
                                "v84_current_rgb_jacobian_q14_joint_field_top8_original_dct_edge_full_legal_q"
                            );
                            assert!(nonworse(&last.score, &current.score, strength));
                        }
                    }
                }
            }
        }
    }
    assert!(triggered > 0, "actual old-pass quality failures exercised");
    assert!(repaired > 0, "actual conditional repair exercised");
    assert!(
        allocated_controls > 0,
        "post-v81 allocated winner exercised"
    );
    assert!(
        rejected_but_no_opportunity > 0,
        "rejected but already beaten proposals do not trigger"
    );
}

#[test]
fn repair_can_recover_sampling_through_nested_controls_but_uses_outer_coding() {
    let (r, source, current) = damaged_constant(420, coding());
    let a = Analysis::new(&r.pixels).unwrap();
    let mut c = candidate(&r, &a, current, 420, coding(), 0);
    // Inner scan2 is deliberately not the outer/file scan4 configuration.
    let inner = Parameters::JpegTableAllocated {
        control: Box::new(parameters(
            420,
            Coding {
                scans: 2,
                ..coding()
            },
            0,
        )),
        luma_quantization_table: vec![8; 64],
        rewritten_frequencies: vec![1],
        scans_per_component: 2,
        eob_runs: true,
        scan_huffman: true,
        model: "test-inner",
        allocation: "test-inner",
        exchanges: 1,
    };
    c.parameters = Parameters::JpegTableAllocated {
        control: Box::new(inner),
        luma_quantization_table: vec![8; 64],
        rewritten_frequencies: vec![1],
        scans_per_component: 4,
        eob_runs: true,
        scan_huffman: true,
        model: "test-outer",
        allocation: "test-outer",
        exchanges: 1,
    };
    assert_eq!(generated_sampling(&c.parameters).unwrap(), 420);
    let frozen = test_oracle::run_coupled(&r, &c.data, coding(), a.memory_budget_bytes).unwrap();
    let mut candidates = vec![c];
    repair::refine(&source, &r, &a, &mut candidates, Strength::default()).unwrap();
    assert_eq!(candidates.len(), 2);
    let expected = if frozen.raw.len() < frozen.plain.len() {
        frozen.raw
    } else {
        frozen.plain
    };
    assert_eq!(candidates[1].data, expected);
    assert_eq!(candidates[1].format(), "jpeg");
}

#[test]
fn coupled_admission_accounts_fields_cache_larger_roots_and_all_retained_capacities() {
    let r = template(33, 35);
    let mut a = Analysis::new(&r.pixels).unwrap();
    let mut candidates = vec![dummy(&r, &a, 420), dummy(&r, &a, 420)];
    let old = resources::preflight(&[0], &r, &a, &candidates, &candidates[0], 420).unwrap();
    let new = resources::preflight_coupled(&[0], &r, &a, &candidates, &candidates[0], 420).unwrap();
    assert_eq!(
        new.fixed_bytes - old.fixed_bytes,
        33 * 35 * 16 + 5 * 5 * 16 + 64 * 1024
    );
    let old_fixed = new.fixed_bytes;
    candidates[1].data.reserve_exact(2 * 1024 * 1024);
    let new = resources::preflight_coupled(&[0], &r, &a, &candidates, &candidates[0], 420).unwrap();
    assert_eq!(
        new.fixed_bytes - old_fixed,
        (candidates[1].data.capacity() - 1) as u64
    );
    a.memory_budget_bytes = new.fixed_bytes + 63 * resources::PROJECTED_ENTRY_BYTES;
    assert_eq!(
        resources::preflight_coupled(&[0], &r, &a, &candidates, &candidates[0], 420)
            .unwrap()
            .histogram_entries,
        63
    );
    a.memory_budget_bytes -= 1;
    assert!(resources::preflight_coupled(&[0], &r, &a, &candidates, &candidates[0], 420).is_none());
    repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).unwrap();
    assert_eq!(
        candidates.len(),
        2,
        "budget rejection occurs before malformed bytes are captured"
    );
    let r = template(65535, 1);
    let a = Analysis::for_reference(&r, 16 * 1024 * 1024).unwrap();
    let mut candidates = vec![dummy(&r, &a, 420)];
    assert!(resources::preflight_coupled(&[0], &r, &a, &candidates, &candidates[0], 420).is_none());
    repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).unwrap();
    assert_eq!(candidates.len(), 1);
}

#[test]
fn projected_histogram_boundary_is_exact_and_exhaustion_is_optional_not_a_partial_model() {
    let mut r = template(19, 25);
    r.pixels = RgbImage::from_fn(19, 25, |x, y| {
        Rgb([
            (x * 37 + y * 13) as u8,
            (x * 17 + y * 53) as u8,
            (x * y * 7 + 19) as u8,
        ])
    });
    let mut a = Analysis::new(&r.pixels).unwrap();
    let data = generated(&r, 420, coding(), 0);
    let captured = coefficient_io::capture(&data, a.memory_budget_bytes).unwrap();
    let full = projection::build(&r, &a, &captured.pixels, &captured.frame, 4, usize::MAX).unwrap();
    assert!(full.histogram_entries > 63);
    assert!(
        projection::build_bounded(
            &r,
            &a,
            &captured.pixels,
            &captured.frame,
            4,
            full.histogram_entries - 1
        )
        .unwrap()
        .is_none()
    );
    let exact = projection::build_bounded(
        &r,
        &a,
        &captured.pixels,
        &captured.frame,
        4,
        full.histogram_entries,
    )
    .unwrap()
    .unwrap();
    assert_eq!(exact.curves, full.curves);
    assert_eq!(exact.raw, full.raw);
    let mut candidates = vec![candidate(&r, &a, data.clone(), 420, coding(), 0)];
    candidates[0].score = a.score(&r.pixels).unwrap();
    let admission =
        resources::preflight_coupled(&data, &r, &a, &candidates, &candidates[0], 420).unwrap();
    a.memory_budget_bytes = admission.fixed_bytes
        + (full.histogram_entries as u64 - 1) * resources::PROJECTED_ENTRY_BYTES;
    let snapshot = format!("{candidates:?}");
    repair::refine(&data, &r, &a, &mut candidates, Strength::default()).unwrap();
    assert_eq!(format!("{candidates:?}"), snapshot);
}

#[test]
fn coupled_repair_bypass_and_capture_errors_remain_distinct() {
    let mut r = template(19, 25);
    let a = Analysis::new(&r.pixels).unwrap();
    let mut candidates = vec![dummy(&r, &a, 444)];
    repair::refine(&[0], &r, &a, &mut candidates, Strength::new(0).unwrap()).unwrap();
    assert!(repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).is_err());
    assert!(!refine_original(&[0], &r, &a, &mut candidates, Strength::new(0).unwrap()).unwrap());
    for parameters in [
        Parameters::Original {
            source_format: "jpeg",
        },
        Parameters::PngLossless,
    ] {
        candidates[0].parameters = parameters;
        repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).unwrap();
        assert!(!refine_original(&[0], &r, &a, &mut candidates, Strength::default()).unwrap());
    }
    candidates[0].parameters = parameters(444, coding(), 0);
    r.source_format = "png";
    repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).unwrap();
    r.source_format = "jpeg";
    r.alpha = Some(image::GrayImage::new(19, 25));
    repair::refine(&[0], &r, &a, &mut candidates, Strength::default()).unwrap();
    assert_eq!(candidates.len(), 1);
}
