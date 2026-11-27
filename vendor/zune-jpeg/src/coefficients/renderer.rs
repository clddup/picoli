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

/// Reusable MCU-row-streaming progressive YCbCr->RGB reconstructor.
/// Reuses decoder scratch buffers across quantization-table searches and
/// supports on-demand MCU-row quantization so early rejections never quantize
/// or IDCT later rows.
pub struct StreamReconstructor {
    dec: crate::decoder::JpegDecoder<zune_core::bytestream::ZCursor<&'static [u8]>>,
    upsampler_scratch_space: Vec<i16>,
    row_buf: Vec<u8>,
    mcu_height: usize,
    w: usize,
    h: usize,
    padded_width: usize,
    max_rows_per_mcu: usize,
}

impl StreamReconstructor {
    pub fn new(width: u16, height: u16, h_max: u8, v_max: u8) -> Result<Self, DecodeErrors> {
        use crate::components::{Components, SampleRatios};
        use crate::decoder::JpegDecoder;
        use crate::misc::{calculate_padded_width, setup_component_params};
        use zune_core::bytestream::ZCursor;

        if width == 0 || height == 0 || !matches!((h_max, v_max), (1 | 2, 1) | (2, 2)) {
            return Err(invalid());
        }
        let options = DecoderOptions::default()
            .set_strict_mode(true)
            .jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut dec = JpegDecoder::new_with_options(ZCursor::new(&[][..]), options);
        dec.info.width = width;
        dec.info.height = height;
        dec.info.components = 3;
        dec.info.sample_ratio = match (h_max, v_max) {
            (1, 1) => SampleRatios::None,
            (2, 1) => SampleRatios::H,
            (2, 2) => SampleRatios::HV,
            _ => return Err(invalid()),
        };
        dec.is_progressive = true;
        dec.input_colorspace = ColorSpace::YCbCr;
        dec.qt_tables[0] = Some([1; 64]);
        dec.qt_tables[1] = Some([1; 64]);
        let hv = (h_max << 4) | v_max;
        dec.components = alloc::vec![
            Components::from([1, hv, 0], 0)?,
            Components::from([2, 0x11, 1], 1)?,
            Components::from([3, 0x11, 1], 2)?,
        ];
        setup_component_params(&mut dec)?;
        if dec.is_interleaved {
            dec.set_upsampling()?;
        }

        let mcu_height = if dec.is_interleaved {
            dec.mcu_y
        } else {
            usize::from(height).div_ceil(8)
        };
        let is_hv = usize::from(dec.is_interleaved);
        let upsampler_scratch_size = is_hv * dec.components[0].width_stride;
        let w = usize::from(width);
        let h = usize::from(height);
        let padded_width = calculate_padded_width(w, dec.info.sample_ratio);
        let upsampler_scratch_space = alloc::vec![0i16; upsampler_scratch_size];

        for comp in dec.components.iter_mut() {
            comp.needed = true;
            comp.raw_coeff = alloc::vec![0i16; comp.width_stride * comp.vertical_sample * 8];
        }

        let max_rows_per_mcu = usize::from(v_max) * 8 + usize::from(v_max);
        let row_buf = alloc::vec![0u8; max_rows_per_mcu * w * 3];

        Ok(Self {
            dec,
            upsampler_scratch_space,
            row_buf,
            mcu_height,
            w,
            h,
            padded_width,
            max_rows_per_mcu,
        })
    }

    pub fn reconstruct_rows(
        &mut self,
        luma_q: &[u16; 64],
        chroma_q: &[u16; 64],
        coeffs: &mut [Vec<i16>; 3],
        mut on_mcu_row: impl FnMut(usize, &mut [Vec<i16>; 3]),
        mut on_rows: impl FnMut(&[u8]) -> bool,
    ) -> Result<bool, DecodeErrors> {
        use crate::mcu::DCT_BLOCK;

        let luma_i32: [i32; 64] = core::array::from_fn(|i| i32::from(luma_q[i]));
        let chroma_i32: [i32; 64] = core::array::from_fn(|i| i32::from(chroma_q[i]));
        self.dec.components[0].quantization_table = luma_i32;
        self.dec.components[1].quantization_table = chroma_i32;
        self.dec.components[2].quantization_table = chroma_i32;

        for (pos, comp) in self.dec.components.iter_mut().enumerate() {
            let step = (comp.width_stride / 8) * comp.vertical_sample * 64;
            let expected = step.checked_mul(self.mcu_height).ok_or_else(invalid)?;
            if coeffs[pos].len() != expected {
                return Err(invalid());
            }
            comp.idct_pos = 0;
        }

        let mut tmp = [0i32; DCT_BLOCK];
        let mut y_written = 0usize;

        for i in 0..self.mcu_height {
            on_mcu_row(i, coeffs);
            for (pos, component) in self.dec.components.iter_mut().enumerate() {
                let qt_table = &component.quantization_table;
                let mcu_x = component.width_stride / 8;
                let step = mcu_x * component.vertical_sample * 64;
                let slice = &coeffs[pos][i * step..(i + 1) * step];
                let temp_channel = &mut component.raw_coeff;
                for k in 0..component.vertical_sample {
                    for j in 0..mcu_x {
                        let start = j * 64 + k * mcu_x * 64;
                        let qt_slice = &slice[start..start + 64];
                        for ((x, out), qt_val) in
                            qt_slice.iter().zip(tmp.iter_mut()).zip(qt_table.iter())
                        {
                            *out = i32::from(*x) * qt_val;
                        }
                        let sl = &mut temp_channel[component.idct_pos..];
                        component.idct_pos += 8;
                        (self.dec.idct_func)(&mut tmp, sl, component.width_stride);
                    }
                    component.idct_pos += 7 * component.width_stride;
                }
                component.idct_pos = 0;
            }
            let remaining_bytes = (self.h - y_written).min(self.max_rows_per_mcu) * self.w * 3;
            let mut local_px = 0usize;
            self.dec.post_process(
                &mut self.row_buf[..remaining_bytes],
                i,
                self.mcu_height,
                self.w,
                self.padded_width,
                &mut local_px,
                &mut self.upsampler_scratch_space,
            )?;
            y_written += local_px / (self.w * 3);
            if local_px > 0 && !on_rows(&self.row_buf[..local_px]) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Reconstruct tightly packed RGB MCU-row-by-MCU-row from natural-order quantized
/// YCbCr coefficients, matching `JpegDecoder::decode` with strict/default options.
/// Stops immediately if `on_rows` returns `false`.
pub fn stream_reconstruct_progressive(
    width: u16,
    height: u16,
    h_max: u8,
    v_max: u8,
    luma_q: &[u16; 64],
    chroma_q: &[u16; 64],
    coeffs: [&[i16]; 3],
    mut on_rows: impl FnMut(&[u8]) -> bool,
) -> Result<bool, DecodeErrors> {
    use crate::mcu::DCT_BLOCK;

    let mut rec = StreamReconstructor::new(width, height, h_max, v_max)?;
    let luma_i32: [i32; 64] = core::array::from_fn(|i| i32::from(luma_q[i]));
    let chroma_i32: [i32; 64] = core::array::from_fn(|i| i32::from(chroma_q[i]));
    rec.dec.components[0].quantization_table = luma_i32;
    rec.dec.components[1].quantization_table = chroma_i32;
    rec.dec.components[2].quantization_table = chroma_i32;
    for (pos, comp) in rec.dec.components.iter_mut().enumerate() {
        let step = (comp.width_stride / 8) * comp.vertical_sample * 64;
        let expected = step.checked_mul(rec.mcu_height).ok_or_else(invalid)?;
        if coeffs[pos].len() != expected {
            return Err(invalid());
        }
    }
    let mut tmp = [0i32; DCT_BLOCK];
    let mut y_written = 0usize;
    for i in 0..rec.mcu_height {
        for (pos, component) in rec.dec.components.iter_mut().enumerate() {
            let qt_table = &component.quantization_table;
            let mcu_x = component.width_stride / 8;
            let step = mcu_x * component.vertical_sample * 64;
            let slice = &coeffs[pos][i * step..(i + 1) * step];
            let temp_channel = &mut component.raw_coeff;
            for k in 0..component.vertical_sample {
                for j in 0..mcu_x {
                    let start = j * 64 + k * mcu_x * 64;
                    let qt_slice = &slice[start..start + 64];
                    for ((x, out), qt_val) in
                        qt_slice.iter().zip(tmp.iter_mut()).zip(qt_table.iter())
                    {
                        *out = i32::from(*x) * qt_val;
                    }
                    let sl = &mut temp_channel[component.idct_pos..];
                    component.idct_pos += 8;
                    (rec.dec.idct_func)(&mut tmp, sl, component.width_stride);
                }
                component.idct_pos += 7 * component.width_stride;
            }
            component.idct_pos = 0;
        }
        let remaining_bytes = (rec.h - y_written).min(rec.max_rows_per_mcu) * rec.w * 3;
        let mut local_px = 0usize;
        rec.dec.post_process(
            &mut rec.row_buf[..remaining_bytes],
            i,
            rec.mcu_height,
            rec.w,
            rec.padded_width,
            &mut local_px,
            &mut rec.upsampler_scratch_space,
        )?;
        y_written += local_px / (rec.w * 3);
        if local_px > 0 && !on_rows(&rec.row_buf[..local_px]) {
            return Ok(false);
        }
    }
    Ok(true)
}
