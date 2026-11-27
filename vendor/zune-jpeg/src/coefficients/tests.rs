/*
 * Copyright (c) 2026.
 *
 * This software is free software;
 *
 * You can redistribute it or modify it under terms of the MIT, Apache License or Zlib license
 */

use alloc::{vec, vec::Vec};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;

use super::{CoefficientCapture, QuantizedCoefficients};
use crate::misc::UN_ZIGZAG;
use crate::JpegDecoder;

const IDS: [u8; 4] = [7, 23, 91, 109];

fn quantizer(natural: usize) -> u16 {
    1 + (natural % 13) as u16
}

fn segment(out: &mut Vec<u8>, marker: u8, data: &[u8]) {
    out.extend_from_slice(&[255, marker]);
    out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(data);
}

#[derive(Default)]
struct Entropy {
    bytes: Vec<u8>,
    byte: u8,
    bits: u8,
}

impl Entropy {
    fn put(&mut self, value: u16, bits: u8) {
        for shift in (0..bits).rev() {
            self.byte = (self.byte << 1) | ((value >> shift) as u8 & 1);
            self.bits += 1;
            if self.bits == 8 {
                self.bytes.push(self.byte);
                if self.byte == 255 {
                    self.bytes.push(0);
                }
                self.byte = 0;
                self.bits = 0;
            }
        }
    }
    fn flush(&mut self) {
        if self.bits != 0 {
            self.put(255, 8 - self.bits);
        }
    }
    fn restart(&mut self, n: usize) {
        self.flush();
        self.bytes.extend_from_slice(&[255, 0xd0 + (n % 8) as u8]);
    }
    fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.bytes
    }
    fn magnitude(&mut self, value: i16, category: u8) {
        let bits = if value < 0 {
            i32::from(value) + (1 << category) - 1
        } else {
            i32::from(value)
        };
        self.put(bits as u16, category);
    }
    fn dc(&mut self, value: i16, previous: &mut i16) {
        let difference = value - *previous;
        *previous = value;
        let category = (16 - difference.unsigned_abs().leading_zeros()) as u8;
        self.put(u16::from(category), 4);
        self.magnitude(difference, category);
    }
    fn ac_symbol(&mut self, symbol: u8) {
        let code = match symbol {
            0 => 0,
            0xf0 => 1,
            _ => 2 + (symbol >> 4) as u16 * 10 + (symbol & 15) as u16 - 1,
        };
        self.put(code, 8);
    }
    fn ac(&mut self, block: &[i16; 64]) {
        let mut run = 0;
        for &natural in &UN_ZIGZAG[1..64] {
            let value = block[natural];
            if value == 0 {
                run += 1;
                continue;
            }
            while run >= 16 {
                self.ac_symbol(0xf0);
                run -= 16;
            }
            let category = (16 - value.unsigned_abs().leading_zeros()) as u8;
            self.ac_symbol((run << 4) | category);
            self.magnitude(value, category);
            run = 0;
        }
        if run != 0 {
            self.ac_symbol(0);
        }
    }
}

fn known_block(k: usize, x: usize, y: usize, actual: (usize, usize)) -> [i16; 64] {
    let mut block = [0; 64];
    block[0] = (k * 11 + x + y * 5) as i16 - 17;
    if x < actual.0 && y < actual.1 {
        match (k + x + y) % 3 {
            0 => {}
            1 => {
                block[8] = -3;
                block[16] = 2;
            }
            _ => {
                block[1] = 3;
                block[63] = -1;
            }
        }
    }
    block
}

/// A small deterministic JPEG writer, not another DCT implementation. The
/// entropy-coded values are known independently of either decoder capture path.
fn fixture(
    width: usize,
    height: usize,
    sampling: &[(usize, usize)],
    progressive: bool,
    restart: usize,
) -> Vec<u8> {
    let count = sampling.len();
    let (max_h, max_v) = if count == 1 {
        (1, 1)
    } else {
        (
            sampling.iter().map(|s| s.0).max().unwrap(),
            sampling.iter().map(|s| s.1).max().unwrap(),
        )
    };
    let mx = width.div_ceil(max_h * 8);
    let my = height.div_ceil(max_v * 8);
    let actual: Vec<_> = sampling
        .iter()
        .map(|&(h, v)| {
            if count == 1 {
                (mx, my)
            } else {
                (
                    (width * h).div_ceil(max_h * 8),
                    (height * v).div_ceil(max_v * 8),
                )
            }
        })
        .collect();
    let mut out = vec![255, 0xd8];
    let mut qt = vec![0];
    qt.extend(UN_ZIGZAG[..64].iter().map(|&i| quantizer(i) as u8));
    segment(&mut out, 0xdb, &qt);
    let mut sof = vec![8];
    sof.extend_from_slice(&(height as u16).to_be_bytes());
    sof.extend_from_slice(&(width as u16).to_be_bytes());
    sof.push(count as u8);
    for (k, &(h, v)) in sampling.iter().enumerate() {
        sof.extend_from_slice(&[IDS[k], ((h << 4) | v) as u8, 0]);
    }
    segment(&mut out, if progressive { 0xc2 } else { 0xc0 }, &sof);
    let mut dc = vec![0; 17];
    dc[4] = 12; // twelve four-bit codes, symbols 0..11
    dc.extend(0..12);
    segment(&mut out, 0xc4, &dc);
    let mut ac = vec![0; 17];
    ac[0] = 0x10;
    ac[8] = 162; // EOB, ZRL, then all sixteen runs and ten magnitudes
    ac.extend_from_slice(&[0, 0xf0]);
    for run in 0..16 {
        for size in 1..=10 {
            ac.push((run << 4) | size);
        }
    }
    segment(&mut out, 0xc4, &ac);
    if restart != 0 {
        segment(&mut out, 0xdd, &(restart as u16).to_be_bytes());
    }
    let mut sos = vec![count as u8];
    for &id in &IDS[..count] {
        sos.extend_from_slice(&[id, 0]);
    }
    sos.extend_from_slice(&[0, if progressive { 0 } else { 63 }, 0]);
    segment(&mut out, 0xda, &sos);
    let mut entropy = Entropy::default();
    let mut previous = [0; 4];
    let mut unit = 0;
    for y in 0..my {
        for x in 0..mx {
            for (k, &(h, v)) in sampling.iter().enumerate() {
                let (h, v) = if count == 1 { (1, 1) } else { (h, v) };
                for by in 0..v {
                    for bx in 0..h {
                        let block = known_block(k, x * h + bx, y * v + by, actual[k]);
                        entropy.dc(block[0], &mut previous[k]);
                        if !progressive {
                            entropy.ac(&block);
                        }
                    }
                }
            }
            unit += 1;
            if restart != 0 && unit % restart == 0 && unit < mx * my {
                entropy.restart(unit / restart - 1);
                previous.fill(0);
            }
        }
    }
    out.extend(entropy.finish());
    if progressive {
        for (k, &(w, h)) in actual.iter().enumerate() {
            segment(&mut out, 0xda, &[1, IDS[k], 0, 1, 63, 0]);
            let mut entropy = Entropy::default();
            let mut unit = 0;
            for y in 0..h {
                for x in 0..w {
                    entropy.ac(&known_block(k, x, y, actual[k]));
                    unit += 1;
                    if restart != 0 && unit % restart == 0 && unit < w * h {
                        entropy.restart(unit / restart - 1);
                    }
                }
            }
            out.extend(entropy.finish());
        }
    }
    out.extend_from_slice(&[255, 0xd9]);
    out
}

fn strict_options() -> DecoderOptions {
    DecoderOptions::default()
        .set_strict_mode(true)
        .jpeg_set_out_colorspace(ColorSpace::RGB)
}

fn decode_capture(jpeg: &[u8], limit: usize) -> (Vec<u8>, QuantizedCoefficients) {
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(jpeg), strict_options());
    decoder.set_save_coefficients(limit);
    assert!(decoder.take_coefficients().is_none());
    let pixels = decoder.decode().unwrap();
    let frame = decoder.take_coefficients().unwrap();
    assert!(decoder.take_coefficients().is_none());
    (pixels, frame)
}

fn verify_frame(
    frame: &QuantizedCoefficients,
    width: usize,
    height: usize,
    sampling: &[(usize, usize)],
    progressive: bool,
) {
    assert_eq!((frame.width, frame.height), (width, height));
    assert_eq!(frame.progressive, progressive);
    assert_eq!(
        frame.input_colorspace,
        match sampling.len() {
            1 => ColorSpace::Luma,
            4 => ColorSpace::CMYK,
            _ => ColorSpace::YCbCr,
        }
    );
    assert_eq!(frame.components.len(), sampling.len());
    let max_h = sampling.iter().map(|s| s.0).max().unwrap();
    let max_v = sampling.iter().map(|s| s.1).max().unwrap();
    for (k, (comp, &(h, v))) in frame.components.iter().zip(sampling).enumerate() {
        assert_eq!(comp.id, IDS[k]);
        assert_eq!(
            (
                comp.horizontal_sampling_factor,
                comp.vertical_sampling_factor
            ),
            (h, v)
        );
        let (w, rows, actual) = if sampling.len() == 1 {
            (
                width.div_ceil(8),
                height.div_ceil(8),
                (width.div_ceil(8), height.div_ceil(8)),
            )
        } else {
            (
                width.div_ceil(max_h * 8) * h,
                height.div_ceil(max_v * 8) * v,
                (
                    (width * h).div_ceil(max_h * 8),
                    (height * v).div_ceil(max_v * 8),
                ),
            )
        };
        assert_eq!((comp.block_width, comp.block_height), (w, rows));
        assert_eq!(comp.coefficients.len(), w * rows * 64);
        for (i, &q) in comp.quantization_table.iter().enumerate() {
            assert_eq!(q, quantizer(i));
        }
        for y in 0..rows {
            for x in 0..w {
                let offset = (y * w + x) * 64;
                assert_eq!(
                    &comp.coefficients[offset..offset + 64],
                    &known_block(k, x, y, actual),
                    "component {k}, block ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn baseline_progressive_natural_planes_and_identical_pixels() {
    for &(width, height) in &[(1, 1), (1, 33), (41, 1), (17, 19), (32, 32)] {
        for sampling in [
            &[(1, 1)][..],
            &[(1, 1), (1, 1), (1, 1)],
            &[(2, 1), (1, 1), (1, 1)],
            &[(1, 2), (1, 1), (1, 1)],
            &[(2, 2), (1, 1), (1, 1)],
            &[(1, 1), (1, 1), (1, 1), (1, 1)],
        ] {
            let mut baseline = None;
            for progressive in [false, true] {
                let jpeg = fixture(width, height, sampling, progressive, 0);
                let mut ordinary =
                    JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
                let expected_pixels = ordinary.decode().unwrap();
                assert!(ordinary.take_coefficients().is_none());
                let (pixels, frame) = decode_capture(&jpeg, usize::MAX);
                assert_eq!(pixels, expected_pixels, "capture changed pixels: {width}x{height}, {sampling:?}, progressive={progressive}");
                verify_frame(&frame, width, height, sampling, progressive);
                if progressive {
                    // Compare every other field: only the original coding
                    // process intentionally differs between these fixtures.
                    let mut sequential_metadata = frame;
                    sequential_metadata.progressive = false;
                    assert_eq!(Some(sequential_metadata), baseline);
                } else {
                    baseline = Some(frame);
                }
            }
        }
    }
}

#[test]
fn baseline_separate_component_scans_use_global_block_rows() {
    let sampling = [(1, 1), (1, 1), (1, 1)];
    let mut jpeg = fixture(17, 19, &sampling, false, 0);
    let sos = jpeg.windows(2).position(|v| v == [255, 0xda]).unwrap();
    jpeg.truncate(sos);
    let (w, h) = (17_usize.div_ceil(8), 19_usize.div_ceil(8));
    for (k, &id) in IDS[..3].iter().enumerate() {
        segment(&mut jpeg, 0xda, &[1, id, 0, 0, 63, 0]);
        let mut entropy = Entropy::default();
        let mut previous = 0;
        for y in 0..h {
            for x in 0..w {
                let block = known_block(k, x, y, (w, h));
                entropy.dc(block[0], &mut previous);
                entropy.ac(&block);
            }
        }
        jpeg.extend(entropy.finish());
    }
    jpeg.extend_from_slice(&[255, 0xd9]);
    let mut ordinary = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
    let (pixels, frame) = decode_capture(&jpeg, usize::MAX);
    assert_eq!(pixels, ordinary.decode().unwrap());
    verify_frame(&frame, 17, 19, &sampling, false);
}

#[test]
fn sampled_grayscale_preserves_original_sof_metadata() {
    for progressive in [false, true] {
        let jpeg = fixture(17, 25, &[(2, 2)], progressive, 0);
        let mut ordinary = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        let (pixels, frame) = decode_capture(&jpeg, usize::MAX);
        assert_eq!(pixels, ordinary.decode().unwrap());
        verify_frame(&frame, 17, 25, &[(2, 2)], progressive);
    }
}

#[test]
fn restart_boundaries_preserve_coefficients_and_pixels() {
    for progressive in [false, true] {
        let sampling = [(2, 2), (1, 1), (1, 1)];
        let jpeg = fixture(35, 37, &sampling, progressive, 2);
        let mut ordinary = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        let (pixels, frame) = decode_capture(&jpeg, usize::MAX);
        assert_eq!(pixels, ordinary.decode().unwrap());
        verify_frame(&frame, 35, 37, &sampling, progressive);
    }
}

#[test]
fn exact_limit_and_disabled_capture() {
    for progressive in [false, true] {
        let jpeg = fixture(17, 19, &[(2, 2), (1, 1), (1, 1)], progressive, 0);
        let (_, frame) = decode_capture(&jpeg, usize::MAX);
        let bytes = frame
            .components
            .iter()
            .map(|c| c.coefficients.len() * 2)
            .sum::<usize>();
        assert_eq!(decode_capture(&jpeg, bytes).1, frame);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        decoder.set_save_coefficients(bytes - 1);
        assert!(decoder.decode().is_err());
        assert!(decoder.take_coefficients().is_none());
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        decoder.set_save_coefficients(0);
        decoder.decode().unwrap();
        assert!(decoder.take_coefficients().is_none());
    }
}

#[test]
fn failed_decode_never_exposes_partial_or_stale_frame() {
    for progressive in [false, true] {
        let jpeg = fixture(17, 19, &[(2, 2), (1, 1), (1, 1)], progressive, 0);
        for cut in [jpeg.len() / 2, jpeg.len() - 3, jpeg.len() - 2] {
            let mut decoder =
                JpegDecoder::new_with_options(ZCursor::new(&jpeg[..cut]), strict_options());
            decoder.set_save_coefficients(usize::MAX);
            assert!(decoder.decode().is_err());
            assert!(decoder.take_coefficients().is_none());
        }
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        decoder.set_save_coefficients(usize::MAX);
        decoder.decode().unwrap();
        // Failure before entering the MCU loop must also clear the saved frame.
        assert!(decoder.decode_into(&mut []).is_err());
        assert!(decoder.take_coefficients().is_none());
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        decoder.set_save_coefficients(usize::MAX);
        decoder.decode().unwrap();
        decoder.set_save_coefficients(0);
        assert!(decoder.take_coefficients().is_none());
    }
}

#[test]
fn strict_mode_and_complete_input_components_are_required() {
    let jpeg = fixture(17, 19, &[(2, 2), (1, 1), (1, 1)], false, 0);
    let mut decoder =
        JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options().set_strict_mode(false));
    decoder.set_save_coefficients(usize::MAX);
    assert!(decoder.decode().is_err());
    assert!(decoder.take_coefficients().is_none());
    let mut decoder = JpegDecoder::new_with_options(
        ZCursor::new(&jpeg),
        strict_options().jpeg_set_out_colorspace(ColorSpace::Luma),
    );
    decoder.set_save_coefficients(usize::MAX);
    assert!(decoder.decode().is_err());
    assert!(decoder.take_coefficients().is_none());
}

#[test]
fn decode_into_publishes_only_after_success_and_preserves_output_tail() {
    for progressive in [false, true] {
        let jpeg = fixture(17, 19, &[(2, 2), (1, 1), (1, 1)], progressive, 0);
        let mut ordinary = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        let pixels = ordinary.decode().unwrap();
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg), strict_options());
        decoder.set_save_coefficients(usize::MAX);
        decoder.decode_headers().unwrap();
        assert!(decoder.take_coefficients().is_none());
        let mut output = vec![205; pixels.len() + 13];
        decoder.decode_into(&mut output).unwrap();
        assert_eq!(&output[..pixels.len()], pixels);
        assert_eq!(&output[pixels.len()..], &[205; 13]);
        verify_frame(
            &decoder.take_coefficients().unwrap(),
            17,
            19,
            &[(2, 2), (1, 1), (1, 1)],
            progressive,
        );
    }
}

#[test]
fn baseline_inverse_quantization_checks_remainder_and_range() {
    let jpeg = fixture(8, 8, &[(1, 1)], false, 0);
    let (_, frame) = decode_capture(&jpeg, usize::MAX);
    for value in [1, i32::from(i16::MAX) * 2 + 2] {
        let mut capture = CoefficientCapture::default();
        capture.pending = Some(frame.clone());
        let mut dequantized = [0; 64];
        dequantized[1] = value; // Q[1]=2: non-integral or outside i16
        assert!(capture
            .baseline_block(0, 0, 0, false, &dequantized)
            .is_err());
    }
}
