//! Reconstruction for generated 8-bit Luma / YCbCr 444, 422 and 420 snapshots.

use alloc::vec::Vec;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;

use super::{zeroed_plane, QuantizedCoefficients, QuantizedComponent};
use crate::color_convert::choose_ycbcr_to_rgb_convert_func;
use crate::decoder::{ColorConvert16Ptr, IDCTPtr};
use crate::errors::DecodeErrors;
use crate::idct::choose_idct_func;
use crate::upsampler::{choose_horizontal_samp_function, choose_hv_samp_function};

fn invalid() -> DecodeErrors {
    DecodeErrors::FormatStatic("Luma renderer: unsupported or mismatched coefficient layout")
}

fn unsafe_coefficients() -> DecodeErrors {
    DecodeErrors::FormatStatic("Luma renderer: coefficient arithmetic exceeds safe IDCT bounds")
}

fn zeroed_rgb(len: usize) -> Result<Vec<u8>, DecodeErrors> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| DecodeErrors::FormatStatic("Luma renderer: allocation failed"))?;
    out.resize(len, 0);
    Ok(out)
}

/// An immutable, owned reconstruction context for independent luma-block trials.
///
/// Uses the same platform-selected integer IDCT, chroma upsamplers and RGB
/// conversion as a strict, full-resolution RGB decode with default CPU options.
/// No frame or current luma coefficients are retained. Chroma is reconstructed
/// once, including padded MCU edges, and never changes during block trials.
/// Available only with the default-off `experimental-coefficients` feature.
pub struct LumaRenderer {
    width: usize,
    height: usize,
    stride: usize,
    quantizers: [u16; 64],
    chroma: [Vec<i16>; 2],
    idct: IDCTPtr,
    rgb: ColorConvert16Ptr,
}

impl LumaRenderer {
    /// Construct a renderer and the entire initial tightly packed RGB image.
    ///
    /// Supports generated 8-bit Luma (1x1) and YCbCr 444/422/420 (chroma 1x1)
    /// snapshots with the exact capture block grids and nonzero 8-bit quantizers.
    /// Both sequential and progressive snapshots are accepted. Component IDs
    /// must be distinct; components are interpreted in the snapshot's SOF order.
    ///
    /// `limit_bytes` bounds peak heap payload allocated here: returned RGB,
    /// retained padded chroma fields, and one temporary chroma plane plus
    /// upsampling scratch. It excludes the caller's snapshot and bounded stack
    /// buffers/metadata. Sizes and the limit are checked before any allocation.
    /// Invalid layouts, dimensions outside 1..=65535, allocation failures, byte
    /// limit violations and coefficients outside conservative safe IDCT bounds
    /// return errors. This deliberately does not emulate malformed-data wrapping.
    pub fn new(
        frame: &QuantizedCoefficients,
        limit_bytes: usize,
    ) -> Result<(Self, Vec<u8>), DecodeErrors> {
        let (h, v) = validate_layout(frame)?;
        let count = frame.components.len();
        let y = &frame.components[0];
        let stride = y.block_width * 8;
        let padded_height = y.block_height * 8;
        let rgb_len = frame
            .width
            .checked_mul(frame.height)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(invalid)?;
        let field_len = stride.checked_mul(padded_height).ok_or_else(invalid)?;
        let chroma_len = if count == 3 { field_len } else { 0 };
        let temporary_len = if count == 3 {
            // The raw plane has the padded component dimensions, NOT visible
            // ceil(width/h), ceil(height/v). Scratch holds two chroma scanlines.
            (field_len / (h * v))
                .checked_add(stride)
                .ok_or_else(invalid)?
        } else {
            0
        };
        let peak_bytes = chroma_len
            .checked_mul(2)
            .and_then(|n| n.checked_add(temporary_len))
            .and_then(|n| n.checked_mul(core::mem::size_of::<i16>()))
            .and_then(|n| n.checked_add(rgb_len))
            .ok_or_else(invalid)?;
        if peak_bytes > limit_bytes {
            return Err(DecodeErrors::FormatStatic(
                "Luma renderer: byte limit exceeded",
            ));
        }
        // Validate every block, including padding, before invoking any codec
        // arithmetic. Trial blocks go through the identical guard below.
        for comp in &frame.components {
            for block in comp.coefficients.chunks_exact(64) {
                dequantize(
                    block.try_into().map_err(|_| invalid())?,
                    &comp.quantization_table,
                )?;
            }
        }
        let options = DecoderOptions::default();
        let mut renderer = Self {
            width: frame.width,
            height: frame.height,
            stride,
            quantizers: y.quantization_table,
            chroma: [Vec::new(), Vec::new()],
            idct: choose_idct_func(&options),
            rgb: choose_ycbcr_to_rgb_convert_func(ColorSpace::RGB, &options).ok_or_else(invalid)?,
        };
        let mut initial = zeroed_rgb(rgb_len)?;
        if count == 3 {
            // Allocate both destinations first so the documented peak is simple
            // and independent of component construction order.
            renderer.chroma = [zeroed_plane(chroma_len)?, zeroed_plane(chroma_len)?];
            for (dest, comp) in renderer.chroma.iter_mut().zip(&frame.components[1..]) {
                reconstruct_chroma(comp, h, v, renderer.idct, &options, dest)?;
            }
        }
        let mut patch = [0; 192];
        for by in 0..frame.height.div_ceil(8) {
            for bx in 0..frame.width.div_ceil(8) {
                let start = (by * y.block_width + bx) * 64;
                let coeff = y.coefficients[start..start + 64]
                    .try_into()
                    .map_err(|_| invalid())?;
                let (left, top, width, height) =
                    renderer.render_block(bx, by, coeff, &mut patch)?;
                for row in 0..height {
                    let start = ((top + row) * frame.width + left) * 3;
                    initial[start..start + width * 3]
                        .copy_from_slice(&patch[row * width * 3..(row + 1) * width * 3]);
                }
            }
        }
        Ok((renderer, initial))
    }

    /// Render one natural-order quantized luma 8x8 block without heap allocation.
    ///
    /// Coordinates index the visible luma block grid, not MCUs or pixels.
    /// Returns `(left, top, width, height)` in pixels. Only `width*height*3`
    /// bytes of `out` are written, in tightly packed RGB row order, cropped at
    /// true image edges. The remaining bytes are untouched. The renderer does
    /// not remember this edit. Invalid coordinates or unsafe coefficients return
    /// an error before writing `out`; no caller-supplied coefficient is clamped.
    pub fn render_block(
        &self,
        block_x: usize,
        block_y: usize,
        coeff: &[i16; 64],
        out: &mut [u8; 192],
    ) -> Result<(usize, usize, usize, usize), DecodeErrors> {
        if block_x >= self.width.div_ceil(8) || block_y >= self.height.div_ceil(8) {
            return Err(DecodeErrors::FormatStatic(
                "Luma renderer: block outside image",
            ));
        }
        let mut block = dequantize(coeff, &self.quantizers)?;
        let mut luma = [0; 64];
        (self.idct)(&mut block, &mut luma, 8);
        let left = block_x * 8;
        let top = block_y * 8;
        let width = (self.width - left).min(8);
        let height = (self.height - top).min(8);
        if self.chroma[0].is_empty() {
            crate::worker::color_convert(
                &[&luma, &[], &[], &[]],
                self.rgb,
                ColorSpace::Luma,
                ColorSpace::RGB,
                &mut out[..width * height * 3],
                width,
                8,
            )?;
        } else {
            // Kernel conversion is lane-independent. Use its ordinary 16-lane
            // entry point with eight valid lanes and stack-only RGB staging.
            let mut y = [0; 16];
            let mut cb = [0; 16];
            let mut cr = [0; 16];
            let mut rgb = [0; 48];
            for row in 0..height {
                y[..8].copy_from_slice(&luma[row * 8..row * 8 + 8]);
                let start = (top + row) * self.stride + left;
                cb[..8].copy_from_slice(&self.chroma[0][start..start + 8]);
                cr[..8].copy_from_slice(&self.chroma[1][start..start + 8]);
                (self.rgb)(&y, &cb, &cr, &mut rgb, &mut 0);
                out[row * width * 3..(row + 1) * width * 3].copy_from_slice(&rgb[..width * 3]);
            }
        }
        Ok((left, top, width, height))
    }
}

pub(super) fn validate_layout(frame: &QuantizedCoefficients) -> Result<(usize, usize), DecodeErrors> {
    if !(1..=65535).contains(&frame.width) || !(1..=65535).contains(&frame.height) {
        return Err(invalid());
    }
    let count = match frame.input_colorspace {
        ColorSpace::Luma => 1,
        ColorSpace::YCbCr => 3,
        _ => return Err(invalid()),
    };
    if frame.components.len() != count {
        return Err(invalid());
    }
    let y = &frame.components[0];
    let (h, v) = (y.horizontal_sampling_factor, y.vertical_sampling_factor);
    if !matches!((h, v), (1 | 2, 1) | (2, 2)) || (count == 1 && (h, v) != (1, 1)) {
        return Err(invalid());
    }
    let mcu_cols = frame.width.div_ceil(h * 8);
    let mcu_rows = frame.height.div_ceil(v * 8);
    for (k, comp) in frame.components.iter().enumerate() {
        let (ch, cv) = if k == 0 { (h, v) } else { (1, 1) };
        let cols = mcu_cols * ch;
        let rows = mcu_rows * cv;
        let len = cols
            .checked_mul(rows)
            .and_then(|n| n.checked_mul(64))
            .ok_or_else(invalid)?;
        if (
            comp.horizontal_sampling_factor,
            comp.vertical_sampling_factor,
        ) != (ch, cv)
            || (comp.block_width, comp.block_height) != (cols, rows)
            || comp.coefficients.len() != len
            || comp
                .quantization_table
                .iter()
                .any(|q| !(1..=255).contains(q))
            || frame.components[..k].iter().any(|c| c.id == comp.id)
        {
            return Err(invalid());
        }
    }
    Ok((h, v))
}

fn reconstruct_chroma(
    comp: &QuantizedComponent,
    h: usize,
    v: usize,
    idct: IDCTPtr,
    options: &DecoderOptions,
    dest: &mut [i16],
) -> Result<(), DecodeErrors> {
    let stride = comp.block_width * 8;
    let rows = comp.block_height * 8;
    let mut raw = zeroed_plane(stride * rows)?;
    for by in 0..comp.block_height {
        for bx in 0..comp.block_width {
            let start = (by * comp.block_width + bx) * 64;
            let coeff = comp.coefficients[start..start + 64]
                .try_into()
                .map_err(|_| invalid())?;
            let mut block = dequantize(coeff, &comp.quantization_table)?;
            idct(&mut block, &mut raw[by * 8 * stride + bx * 8..], stride);
        }
    }
    if h == 1 {
        dest.copy_from_slice(&raw);
    } else {
        let upsample = if v == 2 {
            choose_hv_samp_function(options)
        } else {
            choose_horizontal_samp_function(options)
        };
        let mut scratch = zeroed_plane(stride * h)?;
        for row in 0..rows {
            let current = &raw[row * stride..(row + 1) * stride];
            let up = row.saturating_sub(1);
            let down = (row + 1).min(rows - 1);
            let start = row * v * stride * h;
            upsample(
                current,
                &raw[up * stride..(up + 1) * stride],
                &raw[down * stride..(down + 1) * stride],
                &mut scratch,
                &mut dest[start..start + stride * h * v],
            );
        }
    }
    Ok(())
}

// Absolute intermediate bound for the existing scalar/AVX2/NEON IDCT pass.
// Expand each add/multiply using the triangle inequality (WITHOUT cancelling
// terms): the largest coefficient weights for indices 0..7 are below. Their
// weighted sum plus the pass bias dominates every node, including the <<12.
fn pass_bound(b: &[i64; 8], bias: i64) -> i64 {
    4096 * (b[0] + b[4])
        + 16247 * b[1]
        + 5352 * b[2]
        + 35933 * b[3]
        + 25320 * b[5]
        + 9784 * b[6]
        + 17758 * b[7]
        + bias
}

pub(super) fn dequantize(coeff: &[i16; 64], q: &[u16; 64]) -> Result<[i32; 64], DecodeErrors> {
    let block = core::array::from_fn(|i| i32::from(coeff[i]) * i32::from(q[i]));
    let mut horizontal = [0; 8];
    for col in 0..8 {
        let b: [i64; 8] = core::array::from_fn(|row| i64::from(block[row * 8 + col]).abs());
        if pass_bound(&b, 512) > i64::from(i32::MAX) {
            return Err(unsafe_coefficients());
        }
        // After cancellation the actual 1-D output weights are at most 4096
        // for DC/index4, 5352 for indices2/6 and 5683 for odd indices (see the
        // codec pass). Bound the first-pass RESULT tightly, not its temporary
        // nodes. Ceil accounts for the arithmetic right shift of negatives.
        horizontal[col] = (4096 * (b[0] + b[4])
            + 5352 * (b[2] + b[6])
            + 5683 * (b[1] + b[3] + b[5] + b[7])
            + 512
            + 1023)
            / 1024;
    }
    if pass_bound(&horizontal, 512 + 65536 + (128 << 17)) > i64::from(i32::MAX) {
        return Err(unsafe_coefficients());
    }
    Ok(block)
}
