use super::*;
use image::{Rgb, RgbImage};
use jpeg_encoder::{ColorType, Encoder, QuantizationTableType, SamplingFactor};
const B: u64 = 1024 * 1024 * 1024;
fn safe(a: &Score, b: &Score) -> bool {
    nonworse(a, b, Strength::default())
}
#[test]
fn explicit_nondefault_strength_uses_requested_limits_without_changing_default() {
    let mut r = reference(16, 16);
    r.pixels = RgbImage::from_pixel(16, 16, Rgb([128; 3]));
    let mut frame = coefficient_io::capture(&encoded(&r, SamplingFactor::R_4_4_4, 4), B)
        .unwrap()
        .frame;
    for component in &mut frame.components {
        component.quantization_table = [64; 64];
    }
    for block in frame.components[1].coefficients.chunks_exact_mut(64) {
        block[1] = 1;
    }
    let current = coefficient_io::serialize(&r, &frame, coding(4)).unwrap();
    let a = Analysis::for_reference(&r, B).unwrap();
    let score = a
        .score(&crate::decode_with_budget(&current, B).unwrap().pixels)
        .unwrap();
    let strength = Strength::new(80).unwrap();
    assert!(!Strength::default().limits().accepts(&score));
    assert!(strength.limits().accepts(&score), "{score:?}");
    let cs = vec![Candidate {
        id: "strength80-control".into(),
        parameters: Parameters::Jpeg {
            shape: crate::optimize::Shape::Standard,
            sampling: 444,
            quantization_scale: 100,
            scans_per_component: 4,
            eob_runs: true,
            scan_huffman: true,
            coefficient_rdo_lambda: 0,
            quantization_margin_percent: 100,
            coefficient_rdo_sampling_weights: false,
        },
        bytes: current.len(),
        score: score.clone(),
        data: current.clone(),
    }];
    let ad = admission(&current, &r, &a, &cs, &cs[0], 444).unwrap();
    let alt = pass(&r, &a, &cs[0], 444, coding(4), &ad, strength, false)
        .unwrap()
        .unwrap();
    assert!(alt.stats.committed > 0);
    assert!(nonworse(&alt.score, &score, strength));
    assert_eq!(Strength::default().percent(), 50);
    assert_eq!(Strength::default().limits().pixel, 64.0);
}
fn coding(scans: u8) -> Coding {
    Coding {
        scans,
        eob_runs: true,
        scan_huffman: true,
    }
}
fn reference(w: u32, h: u32) -> Reference {
    Reference {
        pixels: RgbImage::from_fn(w, h, |x, y| {
            Rgb([
                (x * 37 + y * 13) as u8,
                (x * 17 + y * 53) as u8,
                (x * y * 7 + 19) as u8,
            ])
        }),
        alpha: None,
        source_format: "jpeg",
        jpeg_metadata: Default::default(),
    }
}
fn encoded(r: &Reference, s: SamplingFactor, scans: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out, 100);
    e.set_sampling_factor(s);
    e.set_quantization_tables(
        QuantizationTableType::Custom(Box::new([16; 64])),
        QuantizationTableType::Custom(Box::new([16; 64])),
    );
    e.set_progressive_scans(scans);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    e.encode(
        r.pixels.as_raw(),
        r.pixels.width() as u16,
        r.pixels.height() as u16,
        ColorType::Rgb,
    )
    .unwrap();
    out
}
fn apply(im: &mut RgbImage, rect: (usize, usize, usize, usize), rgb: &[u8]) {
    let (x, y, w, h) = rect;
    let stride = im.width() as usize;
    for row in 0..h {
        let start = ((y + row) * stride + x) * 3;
        im.as_mut()[start..start + w * 3].copy_from_slice(&rgb[row * w * 3..(row + 1) * w * 3]);
    }
}
#[test]
fn chroma_initial_trial_halo_and_sequential_commits_equal_full_strict_decoder() {
    for sampling in [
        SamplingFactor::R_4_4_4,
        SamplingFactor::R_4_2_2,
        SamplingFactor::R_4_2_0,
    ] {
        for scans in [2, 4] {
            for (w, h) in [(1, 1), (1, 19), (23, 1), (17, 19), (33, 35)] {
                let r = reference(w, h);
                let current = encoded(&r, sampling, scans);
                let Captured { mut frame, pixels } = coefficient_io::capture(&current, B).unwrap();
                let (mut renderer, initial) =
                    ChromaRenderer::new(&frame, (B / 4) as usize).unwrap();
                assert_eq!(initial, pixels.as_raw().as_slice());
                let mut reconstructed = pixels;
                for component in 1..=2 {
                    for by in 0..frame.components[component].block_height {
                        for bx in 0..frame.components[component].block_width {
                            let start = (by * frame.components[component].block_width + bx) * 64;
                            let mut block: [i16; 64] = frame.components[component].coefficients
                                [start..start + 64]
                                .try_into()
                                .unwrap();
                            block[1] = block[1].saturating_sub(1);
                            block[8] = block[8].saturating_add(1);
                            let mut patch = [0x5au8; 972];
                            let rect = renderer
                                .render_block(component, bx, by, &block, &mut patch)
                                .unwrap();
                            let n = rect.2 * rect.3 * 3;
                            assert!(patch[n..].iter().all(|&b| b == 0x5a));
                            apply(&mut reconstructed, rect, &patch[..n]);
                            renderer.commit_block(component, bx, by, &block).unwrap();
                            frame.components[component].coefficients[start..start + 64]
                                .copy_from_slice(&block);
                            let full = crate::decode_with_budget(
                                &coefficient_io::serialize(&r, &frame, coding(scans)).unwrap(),
                                B,
                            )
                            .unwrap();
                            assert_eq!(
                                reconstructed, full.pixels,
                                "{sampling:?}/{scans}/{w}x{h}/C{component}/{bx},{by}"
                            );
                        }
                    }
                }
            }
        }
    }
}
#[test]
fn chroma_renderer_rejects_layout_budget_coords_and_unsafe_trials_atomically() {
    let r = reference(17, 19);
    let current = encoded(&r, SamplingFactor::R_4_2_0, 4);
    let frame = coefficient_io::capture(&current, B).unwrap().frame;
    assert!(ChromaRenderer::new(&frame, 1).is_err());
    let (mut renderer, _) = ChromaRenderer::new(&frame, (B / 4) as usize).unwrap();
    let block: [i16; 64] = frame.components[1].coefficients[..64].try_into().unwrap();
    let mut good = [0u8; 972];
    let rect = renderer.render_block(1, 0, 0, &block, &mut good).unwrap();
    for (component, x, y) in [(0, 0, 0), (3, 0, 0), (1, usize::MAX, 0), (2, 0, usize::MAX)] {
        let mut out = [0x5au8; 972];
        assert!(
            renderer
                .render_block(component, x, y, &block, &mut out)
                .is_err()
        );
        assert!(out.iter().all(|&b| b == 0x5a));
        assert!(renderer.commit_block(component, x, y, &block).is_err());
    }
    let mut bad = frame.clone();
    bad.components[1].quantization_table = [255; 64];
    for block in bad.components[1].coefficients.chunks_exact_mut(64) {
        block[1..].fill(0);
    }
    let (mut dangerous, _) = ChromaRenderer::new(&bad, (B / 4) as usize).unwrap();
    let safe_block: [i16; 64] = bad.components[1].coefficients[..64].try_into().unwrap();
    let mut safe_before = [0u8; 972];
    let safe_rect = dangerous
        .render_block(1, 0, 0, &safe_block, &mut safe_before)
        .unwrap();
    let mut out = [0x5au8; 972];
    assert!(
        dangerous
            .render_block(1, 0, 0, &[1023; 64], &mut out)
            .is_err()
    );
    assert!(out.iter().all(|&b| b == 0x5a));
    assert!(dangerous.commit_block(1, 0, 0, &[1023; 64]).is_err());
    let mut safe_after = [0u8; 972];
    assert_eq!(
        safe_rect,
        dangerous
            .render_block(1, 0, 0, &safe_block, &mut safe_after)
            .unwrap()
    );
    assert_eq!(safe_before, safe_after);
    let mut after = [0u8; 972];
    assert_eq!(
        rect,
        renderer.render_block(1, 0, 0, &block, &mut after).unwrap()
    );
    assert_eq!(good, after);
    let mut luma = frame.clone();
    luma.input_colorspace = zune_core::colorspace::ColorSpace::Luma;
    luma.components.truncate(1);
    assert!(ChromaRenderer::new(&luma, (B / 4) as usize).is_err());
}
#[test]
fn nearest_cheaper_category_is_strictly_lower_for_all_legal_nonzero_values() {
    for v in -1023i16..=1023 {
        if v == 0 {
            assert_eq!(descend(v), None);
            continue;
        }
        let n = descend(v).unwrap();
        assert!(n == 0 || n.signum() == v.signum());
        assert!(n.unsigned_abs().leading_zeros() > v.unsigned_abs().leading_zeros());
    }
    assert_eq!(descend(1024), None);
    assert_eq!(descend(i16::MIN), None);
}
#[test]
fn bounded_product_cross_chroma_branches_and_prefix_are_exact() {
    let mut r = reference(32, 32);
    r.pixels = RgbImage::from_pixel(32, 32, Rgb([128; 3]));
    let mut frame = coefficient_io::capture(&encoded(&r, SamplingFactor::R_4_4_4, 4), B)
        .unwrap()
        .frame;
    for c in &mut frame.components {
        c.quantization_table = [64; 64];
    }
    for c in &mut frame.components[1..] {
        for b in c.coefficients.chunks_exact_mut(64) {
            b[1..].fill(0);
        }
    }
    let clean = coefficient_io::serialize(&r, &frame, coding(4)).unwrap();
    r = crate::decode_with_budget(&clean, B).unwrap();
    frame.components[1].coefficients[1] = 1;
    frame.components[2].coefficients[1] = 1;
    // A separated second impulse guarantees an independent single-move success.
    frame.components[1].coefficients[15 * 64 + 1] = 1;
    let current = coefficient_io::serialize(&r, &frame, coding(4)).unwrap();
    let current_pixels = crate::decode_with_budget(&current, B).unwrap().pixels;
    // Put the source strictly inside the joint segment, where the antagonistic
    // Cb/Cr cross-term makes either first single move worse. Keep exact source Y
    // flat so integer RGB->Y rounding cannot spuriously create an edge barrier.
    for y in 0..8 {
        for x in 0..8 {
            let p = current_pixels.get_pixel(x, y);
            let mut q = [0u8; 3];
            for c in 0..3 {
                q[c] = ((7 * u16::from(p[c]) + 9 * 128 + 8) / 16) as u8;
            }
            let yy = (77 * i32::from(q[0]) + 150 * i32::from(q[1]) + 29 * i32::from(q[2])) >> 8;
            for c in &mut q {
                *c = (i32::from(*c) + 128 - yy) as u8;
            }
            r.pixels.put_pixel(x, y, Rgb(q));
        }
    }
    let original_score = Analysis::for_reference(&r, B)
        .unwrap()
        .score(&crate::decode_with_budget(&current, B).unwrap().pixels)
        .unwrap();
    assert!(
        Strength::default().limits().accepts(&original_score),
        "{original_score:?}"
    );
    let a = Analysis::for_reference(&r, B).unwrap();
    for component in [1, 2] {
        let mut one = frame.clone();
        one.components[component].coefficients[1] = 0;
        let decoded =
            crate::decode_with_budget(&coefficient_io::serialize(&r, &one, coding(4)).unwrap(), B)
                .unwrap();
        let one_score = a.score(&decoded.pixels).unwrap();
        assert!(
            !safe(&one_score, &original_score),
            "single C{component} unexpectedly admissible: {one_score:?} vs {original_score:?}"
        );
    }
    let mut both = frame.clone();
    both.components[1].coefficients[1] = 0;
    both.components[2].coefficients[1] = 0;
    let pair_score = a
        .score(
            &crate::decode_with_budget(
                &coefficient_io::serialize(&r, &both, coding(4)).unwrap(),
                B,
            )
            .unwrap()
            .pixels,
        )
        .unwrap();
    assert!(
        safe(&pair_score, &original_score),
        "joint {pair_score:?} vs {original_score:?}"
    );
    let parameters = Parameters::Jpeg {
        shape: crate::optimize::Shape::Standard,
        sampling: 444,
        quantization_scale: 100,
        scans_per_component: 4,
        eob_runs: true,
        scan_huffman: true,
        coefficient_rdo_lambda: 0,
        quantization_margin_percent: 100,
        coefficient_rdo_sampling_weights: false,
    };
    let mut cs = vec![Candidate {
        id: "unit-control".into(),
        parameters,
        bytes: current.len(),
        score: original_score.clone(),
        data: current.clone(),
    }];
    let ad = admission(&current, &r, &a, &cs, &cs[0], 444).unwrap();
    let single = pass(
        &r,
        &a,
        &cs[0],
        444,
        coding(4),
        &ad,
        Strength::default(),
        false,
    )
    .unwrap()
    .unwrap();
    let raw = pass(
        &r,
        &a,
        &cs[0],
        444,
        coding(4),
        &ad,
        Strength::default(),
        true,
    )
    .unwrap()
    .unwrap();
    assert!(single.stats.committed > 0);
    assert!(raw.stats.compensated_commits > 0);
    for alt in [&single, &raw] {
        assert!(nonworse(&alt.score, &original_score, Strength::default()));
        let new = coefficient_io::capture(&alt.data, B).unwrap().frame;
        assert_eq!(frame.components[0], new.components[0]);
        for (x, y) in frame.components.iter().zip(&new.components) {
            assert_eq!(x.quantization_table, y.quantization_table);
            for (a, b) in x
                .coefficients
                .chunks_exact(64)
                .zip(y.coefficients.chunks_exact(64))
            {
                assert_eq!(a[0], b[0]);
            }
        }
    }
    refine(&current, &r, &a, &mut cs, Strength::default()).unwrap();
    assert_eq!(cs[0].data, current);
    assert_eq!(cs[0].id, "unit-control");
    assert_eq!(cs.len(), 2);
    assert_eq!(cs[1].format(), "jpeg");
    assert!(cs[1].bytes < cs[0].bytes);
    assert!(nonworse(&cs[1].score, &original_score, Strength::default()));
    let count = cs.len();
    refine(&current, &r, &a, &mut cs, Strength::default()).unwrap();
    assert_eq!(cs.len(), count);
    let mut tiny = Analysis::for_reference(&r, B).unwrap();
    tiny.memory_budget_bytes = 1;
    assert!(admission(&current, &r, &tiny, &cs, &cs[0], 444).is_none());
    let bad = cs[0].bytes;
    cs[0].bytes += 1;
    let one = vec![cs.remove(0)];
    let mut one = one;
    assert!(refine(&current, &r, &a, &mut one, Strength::default()).is_err());
    assert_eq!(one[0].bytes, bad + 1);
}
