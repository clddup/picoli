/*
 * Copyright (c) 2026.
 *
 * This software is free software;
 *
 * You can redistribute it or modify it under terms of the MIT, Apache License or Zlib license
 */

//! Experimental coefficient capture and independent exact RGB reconstruction.

mod chroma_renderer;
pub use chroma_renderer::ChromaRenderer;
mod renderer;
pub use renderer::{LumaRenderer, stream_reconstruct_progressive};

use alloc::vec::Vec;
use zune_core::bytestream::ZByteReaderTrait;
use zune_core::colorspace::ColorSpace;

use crate::decoder::MAX_COMPONENTS;
use crate::errors::DecodeErrors;
use crate::JpegDecoder;

/// One original JPEG component and its owned quantized DCT plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizedComponent {
    /// Component identifier from SOF, not a color-channel index.
    pub id: u8,
    pub horizontal_sampling_factor: usize,
    pub vertical_sampling_factor: usize,
    /// Quantizers in natural (not zigzag) order.
    pub quantization_table: [u16; 64],
    /// Padded block-grid dimensions. Single-component scans use an 8-pixel grid.
    pub block_width: usize,
    pub block_height: usize,
    /// Row-major blocks, each containing 64 natural-order quantized coefficients.
    /// Padding blocks absent from non-interleaved scans remain zero.
    pub coefficients: Vec<i16>,
}

/// Original frame dimensions, input colorspace and components in SOF order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizedCoefficients {
    pub width: usize,
    pub height: usize,
    pub input_colorspace: ColorSpace,
    /// Original frame coding process: progressive rather than sequential DCT.
    pub progressive: bool,
    pub components: Vec<QuantizedComponent>,
}

#[derive(Default)]
pub(crate) struct CoefficientCapture {
    pub(crate) limit: usize,
    pub(crate) pending: Option<QuantizedCoefficients>,
    saved: Option<QuantizedCoefficients>,
    baseline_seen: [usize; MAX_COMPONENTS],
    baseline_interleaved: [Option<bool>; MAX_COMPONENTS],
    dc_seen: [bool; MAX_COMPONENTS],
    complete: bool,
}

fn capture_error() -> DecodeErrors {
    DecodeErrors::FormatStatic("Coefficient capture: incomplete or unsupported coefficient plane")
}

pub(crate) fn zeroed_plane(len: usize) -> Result<Vec<i16>, DecodeErrors> {
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(len)
        .map_err(|_| DecodeErrors::FormatStatic("Coefficient capture: allocation failed"))?;
    plane.resize(len, 0);
    Ok(plane)
}

impl<T: ZByteReaderTrait> JpegDecoder<T> {
    /// Enable experimental capture for the next ordinary decode; zero disables it.
    ///
    /// Requires strict decoder options. The limit bounds the sum of coefficient
    /// payload bytes (two bytes per coefficient, including padded blocks), not
    /// pixel output, existing decoder scratch space or bounded component metadata.
    /// Counts and the total are checked before allocating any capture planes.
    /// Unsupported output modes that discard input components return an error.
    /// Calling this method clears any previously captured frame.
    pub fn set_save_coefficients(&mut self, limit_bytes: usize) {
        self.coefficient_capture.clear();
        self.coefficient_capture.limit = limit_bytes;
    }

    /// Take the owned coefficients after a successful complete strict decode.
    /// Returns `None` before success, after failure, when disabled, or after taking.
    pub fn take_coefficients(&mut self) -> Option<QuantizedCoefficients> {
        self.coefficient_capture.saved.take()
    }

    /// Called after setup_component_params, before grayscale normalization.
    pub(crate) fn begin_coefficient_capture(&mut self) -> Result<(), DecodeErrors> {
        if self.coefficient_capture.limit == 0 {
            return Ok(());
        }
        if !self.options.strict_mode() {
            return Err(DecodeErrors::FormatStatic(
                "Coefficient capture requires strict decoding",
            ));
        }
        let count = self.components.len();
        if count == 0 || count > MAX_COMPONENTS || count != self.input_colorspace.num_components() {
            return Err(capture_error());
        }
        if !self.is_progressive
            && count != 4
            && self.options.jpeg_get_out_colorspace().num_components() < count
        {
            return Err(DecodeErrors::FormatStatic(
                "Coefficient capture: output mode discards input components",
            ));
        }
        let mut components = Vec::new();
        components
            .try_reserve_exact(count)
            .map_err(|_| capture_error())?;
        let mut total_bytes = 0_usize;
        for comp in &self.components {
            let (block_width, block_height) = if count == 1 {
                // A single component is entropy coded non-interleaved, even
                // when SOF carries non-unit sampling factors.
                (
                    (self.info.width as usize).div_ceil(8),
                    (self.info.height as usize).div_ceil(8),
                )
            } else {
                let height = self
                    .mcu_y
                    .checked_mul(comp.vertical_sample)
                    .ok_or_else(capture_error)?;
                let width = self
                    .mcu_x
                    .checked_mul(comp.horizontal_sample)
                    .ok_or_else(capture_error)?;
                if comp.width_stride % 8 != 0 || comp.width_stride / 8 != width {
                    return Err(capture_error());
                }
                (comp.width_stride / 8, height)
            };
            let bytes = block_width
                .checked_mul(block_height)
                .and_then(|n| n.checked_mul(64))
                .and_then(|n| n.checked_mul(core::mem::size_of::<i16>()))
                .ok_or_else(capture_error)?;
            total_bytes = total_bytes.checked_add(bytes).ok_or_else(capture_error)?;
            if total_bytes > self.coefficient_capture.limit {
                return Err(DecodeErrors::FormatStatic(
                    "Coefficient capture: byte limit exceeded",
                ));
            }
            let mut table = [0_u16; 64];
            for (out, &q) in table.iter_mut().zip(&comp.quantization_table) {
                *out = u16::try_from(q).ok().filter(|q| *q != 0).ok_or_else(|| {
                    DecodeErrors::FormatStatic("Coefficient capture: invalid quantization table")
                })?;
            }
            components.push(QuantizedComponent {
                id: comp.id,
                horizontal_sampling_factor: comp.horizontal_sample,
                vertical_sampling_factor: comp.vertical_sample,
                quantization_table: table,
                block_width,
                block_height,
                coefficients: Vec::new(),
            });
        }
        // Progressive decoding already owns quantized planes; move them after
        // pixel processing instead of allocating a second copy here.
        if !self.is_progressive {
            for comp in &mut components {
                comp.coefficients = zeroed_plane(comp.block_width * comp.block_height * 64)?;
            }
        }
        self.coefficient_capture.pending = Some(QuantizedCoefficients {
            width: self.info.width as usize,
            height: self.info.height as usize,
            input_colorspace: self.input_colorspace,
            progressive: self.is_progressive,
            components,
        });
        Ok(())
    }
}

impl CoefficientCapture {
    pub(crate) fn clear(&mut self) {
        self.pending = None;
        self.saved = None;
        self.baseline_seen = [0; MAX_COMPONENTS];
        self.baseline_interleaved = [None; MAX_COMPONENTS];
        self.dc_seen = [false; MAX_COMPONENTS];
        self.complete = false;
    }

    /// Verify visitation order as well as count without a per-block bitmap.
    pub(crate) fn baseline_block(
        &mut self,
        k: usize,
        x: usize,
        y: usize,
        interleaved: bool,
        dequantized: &[i32; 64],
    ) -> Result<(), DecodeErrors> {
        let Some(frame) = &mut self.pending else {
            return Ok(());
        };
        let single_component = frame.components.len() == 1;
        let max_h = frame
            .components
            .iter()
            .map(|c| c.horizontal_sampling_factor)
            .max()
            .unwrap();
        let comp = frame.components.get_mut(k).ok_or_else(capture_error)?;
        let seen = self.baseline_seen[k];
        let h = if interleaved {
            comp.horizontal_sampling_factor
        } else {
            1
        };
        let v = if interleaved {
            comp.vertical_sampling_factor
        } else {
            1
        };
        let row_width = if interleaved || single_component {
            comp.block_width
        } else {
            (frame.width * comp.horizontal_sampling_factor).div_ceil(max_h * 8)
        };
        let mcu = seen / (h * v);
        let expected_x = (mcu % (row_width / h)) * h + seen % h;
        let expected_y = (mcu / (row_width / h)) * v + (seen / h) % v;
        if x != expected_x || y != expected_y || x >= comp.block_width || y >= comp.block_height {
            return Err(capture_error());
        }
        if self.baseline_interleaved[k].is_some_and(|mode| mode != interleaved) {
            return Err(capture_error());
        }
        self.baseline_interleaved[k] = Some(interleaved);
        let offset = (y * comp.block_width + x) * 64;
        let block = &mut comp.coefficients[offset..offset + 64];
        for ((out, &value), &q) in block
            .iter_mut()
            .zip(dequantized)
            .zip(&comp.quantization_table)
        {
            let q = i32::from(q);
            if value % q != 0 {
                return Err(capture_error());
            }
            *out = i16::try_from(value / q).map_err(|_| capture_error())?;
        }
        self.baseline_seen[k] += 1;
        Ok(())
    }

    pub(crate) fn complete_baseline(&mut self, seen_eoi: bool) -> Result<(), DecodeErrors> {
        let Some(frame) = &self.pending else {
            return Ok(());
        };
        if !seen_eoi {
            return Err(capture_error());
        }
        let max_h = frame
            .components
            .iter()
            .map(|c| c.horizontal_sampling_factor)
            .max()
            .unwrap();
        let max_v = frame
            .components
            .iter()
            .map(|c| c.vertical_sampling_factor)
            .max()
            .unwrap();
        for (k, comp) in frame.components.iter().enumerate() {
            let expected =
                if self.baseline_interleaved[k] == Some(true) || frame.components.len() == 1 {
                    comp.block_width * comp.block_height
                } else {
                    (frame.width * comp.horizontal_sampling_factor).div_ceil(max_h * 8)
                        * (frame.height * comp.vertical_sampling_factor).div_ceil(max_v * 8)
                };
            if self.baseline_seen[k] != expected {
                return Err(capture_error());
            }
        }
        self.complete = true;
        Ok(())
    }

    pub(crate) fn progressive_scan(&mut self, components: &[usize], dc_first: bool) {
        if self.pending.is_some() && dc_first {
            for &k in components {
                self.dc_seen[k] = true;
            }
        }
    }

    pub(crate) fn complete_progressive(
        &mut self,
        block: &mut [Vec<i16>; MAX_COMPONENTS],
    ) -> Result<(), DecodeErrors> {
        let Some(frame) = &mut self.pending else {
            return Ok(());
        };
        for (k, comp) in frame.components.iter_mut().enumerate() {
            if !self.dc_seen[k] || block[k].len() != comp.block_width * comp.block_height * 64 {
                return Err(capture_error());
            }
            comp.coefficients = core::mem::take(&mut block[k]);
        }
        self.complete = true;
        Ok(())
    }

    pub(crate) fn finish(&mut self, result: Result<(), DecodeErrors>) -> Result<(), DecodeErrors> {
        match result {
            Ok(()) if self.limit == 0 => Ok(()),
            Ok(()) if self.complete => {
                self.saved = self.pending.take();
                Ok(())
            }
            other => {
                self.clear();
                other.and(Err(capture_error()))
            }
        }
    }
}

#[cfg(test)]
mod tests;
