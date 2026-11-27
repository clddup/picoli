use super::*;
use crate::{
    Reference,
    jpeg_table::coefficient_io::{self, Coding},
};
use image::Rgb;
use jpeg_encoder::{ColorType, Encoder, QuantizationTableType};
#[test]
fn rejecting_the_best_move_tries_the_next_ranked_choice_without_mutating_choices() {
    let keep = Point::keep(8);
    let a = Point {
        quantizer: 9,
        retain: false,
        pixel: -1,
        edge: -1,
        tile: -1,
        rate: -10,
    };
    let b = Point {
        quantizer: 10,
        retain: false,
        pixel: -2,
        edge: -2,
        tile: -2,
        rate: -9,
    };
    let curves = vec![vec![keep], vec![keep, a, b]];
    let mut calls = Vec::new();
    let (solution, info) = search(&curves, false, |choices, change| {
        assert_eq!(choices, &[0, 0]);
        calls.push(change.first.1);
        Ok(change.first.1 != 1)
    })
    .unwrap();
    assert_eq!(calls, vec![1, 2]);
    assert_eq!(solution.choices, vec![0, 2]);
    assert_eq!((info.trials, info.rejected, info.accepted), (2, 1, 1));
    assert_eq!(solution.total.rate, -9);
}
#[test]
fn an_always_accepting_guard_reproduces_the_original_solver_including_pair_barriers() {
    let keep = Point::keep(8);
    let credit = Point {
        quantizer: 7,
        retain: false,
        pixel: -10,
        edge: -20,
        tile: -5,
        rate: 3,
    };
    let saving = Point {
        quantizer: 10,
        retain: false,
        pixel: 8,
        edge: 19,
        tile: 4,
        rate: -9,
    };
    let curves = vec![vec![keep], vec![keep, credit], vec![keep, saving]];
    for paired in [false, true] {
        let expected = super::super::solver::solve(&curves, paired);
        let (actual, info) = search(&curves, paired, |_, _| Ok(true)).unwrap();
        assert_eq!(actual.choices, expected.choices);
        assert_eq!(actual.total, expected.total);
        assert_eq!(actual.exchanges, expected.exchanges);
        assert_eq!(info.rejected, 0);
    }
}
fn encoded(r: &Reference) -> Vec<u8> {
    let mut data = Vec::new();
    let mut e = Encoder::new(&mut data, 100);
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([7; 64])),
        QuantizationTableType::Custom(Box::new([7; 64])),
    );
    e.set_progressive_scans(4);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    e.set_experimental_ac_rdo_lambda(256);
    e.encode(
        r.pixels.as_raw(),
        r.pixels.width() as u16,
        r.pixels.height() as u16,
        ColorType::Rgb,
    )
    .unwrap();
    data
}
fn brute(state: &State<'_>, choices: &[usize]) -> (Vec<i64>, i128, i128) {
    let mut field = vec![0i64; state.width * state.height];
    for (index, source) in state.model.raw.iter().enumerate() {
        let (bx, by) = (index % state.cols, index / state.cols);
        let first = (by * state.frame.components[0].block_width + bx) * 64;
        for (natural, &choice) in choices.iter().enumerate().skip(1) {
            let p = state.model.curves[natural][choice];
            if p.retain {
                continue;
            }
            let new = i64::from(p.quantizer)
                * i64::from(model::quantize(source[natural], p.quantizer).unwrap());
            let old = i64::from(state.frame.components[0].quantization_table[natural])
                * i64::from(state.frame.components[0].coefficients[first + natural]);
            for (t, &basis) in BASIS[natural].iter().enumerate() {
                let (x, y) = (bx * 8 + t % 8, by * 8 + t / 8);
                if x < state.width && y < state.height {
                    field[y * state.width + x] += (new - old) * i64::from(basis);
                }
            }
        }
    }
    let (mut pixel, mut tile) = (0i128, 0i128);
    for (i, &d) in field.iter().enumerate() {
        let d = i128::from(d);
        let [a, b] = state.factors[i];
        let value = i128::from(a) * d * d - 2 * Q * i128::from(b) * d;
        pixel += value;
        if state.model.critical_tile == (i % state.width / 16, i / state.width / 16) {
            tile += value;
        }
    }
    (field, pixel, tile)
}
#[test]
fn incremental_joint_preview_and_commit_equal_full_frequency_field_with_odd_edges() {
    let r = Reference {
        pixels: RgbImage::from_fn(19, 17, |x, y| {
            Rgb([
                (x * 37 + y * 13) as u8,
                (x * 17 + y * 53) as u8,
                (x * y * 7 + 19) as u8,
            ])
        }),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    };
    let budget = 1024 * 1024 * 1024;
    let analysis = Analysis::for_reference(&r, budget).unwrap();
    let captured = coefficient_io::capture(&encoded(&r), budget).unwrap();
    let model = projection::build(
        &r,
        &analysis,
        &captured.pixels,
        &captured.frame,
        4,
        usize::MAX,
    )
    .unwrap();
    let mut state = State::new(&model, &captured.frame, &analysis, &captured.pixels).unwrap();
    let mut choices = vec![0; 64];
    let active: Vec<_> = model
        .curves
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, p)| p.len() > 1)
        .take(2)
        .map(|(f, _)| f)
        .collect();
    assert_eq!(active.len(), 2);
    for change in [
        Change {
            first: (active[0], 1),
            second: None,
        },
        Change {
            first: (active[0], 1),
            second: Some((active[1], 1)),
        },
    ] {
        let before = state.field.clone();
        let mut next = choices.clone();
        next[change.first.0] = change.first.1;
        if let Some((g, b)) = change.second {
            next[g] = b;
        }
        let (field, pixel, tile) = brute(&state, &next);
        assert_eq!(
            state.preview(&choices, change).unwrap(),
            (pixel - state.pixel, tile - state.tile)
        );
        let accepted = state.try_move(&choices, change).unwrap();
        assert_eq!(accepted, pixel <= 0 && tile <= 0);
        if accepted {
            assert_eq!(state.field, field);
            assert_eq!((state.pixel, state.tile), (pixel, tile));
            choices = next;
        } else {
            assert_eq!(state.field, before);
        }
    }
}
#[test]
fn coupled_real_codec_path_removes_error_and_preserves_dc_chroma_and_full_rgb() {
    let template = Reference {
        pixels: RgbImage::from_pixel(33, 35, Rgb([97, 127, 167])),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    };
    let budget = 1024 * 1024 * 1024;
    let coding = Coding {
        scans: 4,
        eob_runs: true,
        scan_huffman: true,
    };
    let mut frame = coefficient_io::capture(&encoded(&template), budget)
        .unwrap()
        .frame;
    let canonical = coefficient_io::serialize(&template, &frame, coding).unwrap();
    let reference = crate::decode_with_budget(&canonical, budget).unwrap();
    for by in 0..35usize.div_ceil(8) {
        for bx in 0..33usize.div_ceil(8) {
            let first = (by * frame.components[0].block_width + bx) * 64;
            assert_eq!(frame.components[0].coefficients[first + 1], 0);
            frame.components[0].coefficients[first + 1] = 1;
        }
    }
    let current = coefficient_io::serialize(&reference, &frame, coding).unwrap();
    let result =
        super::super::test_oracle::run_coupled(&reference, &current, coding, budget).unwrap();
    assert!(result.identity_file_exact);
    assert_ne!(result.selected_variant, "current");
    assert!(result.raw.len() < current.len());
    assert!(super::super::nonworse(
        &result.raw_score,
        &result.control_score,
        crate::quality::Strength::default()
    ));
    assert!(result.stats.paired.joint_guard.as_ref().unwrap().accepted > 0);
    let after = coefficient_io::capture(&result.raw, budget).unwrap().frame;
    assert_eq!(
        frame.components[0].quantization_table[0],
        after.components[0].quantization_table[0]
    );
    for (index, (a, b)) in frame.components.iter().zip(&after.components).enumerate() {
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
        crate::decode_with_budget(&result.raw, budget)
            .unwrap()
            .pixels,
        reference.pixels
    );
}
