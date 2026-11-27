//! Exact, allocation-free chroma-block trials over retained raw IDCT planes.

use alloc::vec::Vec;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;

use super::renderer::{dequantize, validate_layout};
use super::{zeroed_plane, QuantizedCoefficients};
use crate::color_convert::choose_ycbcr_to_rgb_convert_func;
use crate::decoder::{ColorConvert16Ptr, IDCTPtr};
use crate::errors::DecodeErrors;
use crate::idct::choose_idct_func;

fn invalid() -> DecodeErrors {
    DecodeErrors::FormatStatic("Chroma renderer: unsupported or mismatched coefficient layout")
}

/// An owned reconstruction context for exact Cb/Cr block trials and commits.
///
/// Retains the raw padded Y, Cb and Cr IDCT planes, not upsampled chroma or
/// coefficients. Uses the default platform-selected integer IDCT and RGB kernels.
/// Chroma interpolation exactly follows the scalar upsampler, including its
/// separate vertical and horizontal rounding and padded boundary replication.
/// Available only with the default-off `experimental-coefficients` feature.
pub struct ChromaRenderer {
    width: usize,
    height: usize,
    stride: usize,
    chroma_stride: usize,
    chroma_height: usize,
    h: usize,
    v: usize,
    quantizers: [[u16; 64]; 2],
    planes: [Vec<i16>; 3],
    idct: IDCTPtr,
    rgb: ColorConvert16Ptr,
}

struct Candidate<'a> {
    component: usize,
    left: usize,
    top: usize,
    samples: &'a [i16; 64],
}

impl ChromaRenderer {
    /// Reconstruct all raw padded planes once and return the initial packed RGB.
    ///
    /// Accepts the same generated 8-bit YCbCr 444/422/420 layouts and conservative
    /// coefficient bounds as `LumaRenderer`. Luma and other colorspaces return
    /// an error. The snapshot is borrowed only during construction and unchanged.
    ///
    /// `limit_bytes` bounds the sum of all three owned raw plane payloads (i16)
    /// and the returned RGB payload. There are no temporary heap planes. The
    /// caller's snapshot, bounded stack scratch, metadata and allocator overhead
    /// are excluded. All sizes and the total limit are checked before allocating.
    pub fn new(
        frame: &QuantizedCoefficients,
        limit_bytes: usize,
    ) -> Result<(Self, Vec<u8>), DecodeErrors> {
        let (h, v) = validate_layout(frame)?;
        if frame.input_colorspace != ColorSpace::YCbCr {
            return Err(invalid());
        }
        let y = &frame.components[0];
        let cb = &frame.components[1];
        let stride = y.block_width.checked_mul(8).ok_or_else(invalid)?;
        let padded_height = y.block_height.checked_mul(8).ok_or_else(invalid)?;
        let chroma_stride = cb.block_width.checked_mul(8).ok_or_else(invalid)?;
        let chroma_height = cb.block_height.checked_mul(8).ok_or_else(invalid)?;
        let y_len = stride.checked_mul(padded_height).ok_or_else(invalid)?;
        let chroma_len = chroma_stride
            .checked_mul(chroma_height)
            .ok_or_else(invalid)?;
        let rgb_len = frame
            .width
            .checked_mul(frame.height)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(invalid)?;
        let payload_bytes = chroma_len
            .checked_mul(2)
            .and_then(|n| n.checked_add(y_len))
            .and_then(|n| n.checked_mul(core::mem::size_of::<i16>()))
            .and_then(|n| n.checked_add(rgb_len))
            .ok_or_else(invalid)?;
        if payload_bytes > limit_bytes {
            return Err(DecodeErrors::FormatStatic(
                "Chroma renderer: byte limit exceeded",
            ));
        }
        // Validate every block, including padding, before allocation or IDCT.
        // Never clamp coefficients or emulate malformed-data arithmetic wrapping.
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
            chroma_stride,
            chroma_height,
            h,
            v,
            quantizers: [
                cb.quantization_table,
                frame.components[2].quantization_table,
            ],
            planes: [
                zeroed_plane(y_len)?,
                zeroed_plane(chroma_len)?,
                zeroed_plane(chroma_len)?,
            ],
            idct: choose_idct_func(&options),
            rgb: choose_ycbcr_to_rgb_convert_func(ColorSpace::RGB, &options).ok_or_else(invalid)?,
        };
        let mut initial = Vec::new();
        initial
            .try_reserve_exact(rgb_len)
            .map_err(|_| DecodeErrors::FormatStatic("Chroma renderer: allocation failed"))?;
        initial.resize(rgb_len, 0);
        for (component, comp) in frame.components.iter().enumerate() {
            let plane_stride = comp.block_width * 8;
            for by in 0..comp.block_height {
                for bx in 0..comp.block_width {
                    let start = (by * comp.block_width + bx) * 64;
                    let coeff = comp.coefficients[start..start + 64]
                        .try_into()
                        .map_err(|_| invalid())?;
                    let mut block = dequantize(coeff, &comp.quantization_table)?;
                    (renderer.idct)(
                        &mut block,
                        &mut renderer.planes[component][by * 8 * plane_stride + bx * 8..],
                        plane_stride,
                    );
                }
            }
        }
        renderer.render_rectangle(0, 0, frame.width, frame.height, None, &mut initial);
        Ok((renderer, initial))
    }

    /// Render a candidate natural-order chroma block without changing this state.
    ///
    /// `component` is the SOF-order channel index: 1 for Cb, 2 for Cr, not its
    /// component ID. Coordinates index that component's padded 8x8 block grid.
    /// Returns `(left, top, width, height)` in visible image pixels. The raw
    /// block bounds are scaled by the sampling factors and expanded by one pixel
    /// on each side only in dimensions scaled by two, then cropped to the image.
    /// At most 18x18 pixels are affected. Only the first `width*height*3` bytes of
    /// `out` are written in packed RGB row order; its remainder is untouched.
    ///
    /// All other raw samples come from the retained global padded planes, so
    /// trials include neighboring blocks and previous commits exactly. Invalid
    /// channels, coordinates or unsafe coefficients fail before modifying `out`.
    pub fn render_block(
        &self,
        component: usize,
        block_x: usize,
        block_y: usize,
        coeff: &[i16; 64],
        out: &mut [u8; 972],
    ) -> Result<(usize, usize, usize, usize), DecodeErrors> {
        let samples = self.prepare_block(component, block_x, block_y, coeff)?;
        let candidate = Candidate {
            component,
            left: block_x * 8,
            top: block_y * 8,
            samples: &samples,
        };
        let halo_x = usize::from(self.h == 2);
        let halo_y = usize::from(self.v == 2);
        let left = (candidate.left * self.h).saturating_sub(halo_x);
        let top = (candidate.top * self.v).saturating_sub(halo_y);
        let right = ((candidate.left + 8) * self.h + halo_x).min(self.width);
        let bottom = ((candidate.top + 8) * self.v + halo_y).min(self.height);
        let width = right - left;
        let height = bottom - top;
        self.render_rectangle(
            left,
            top,
            width,
            height,
            Some(&candidate),
            &mut out[..width * height * 3],
        );
        Ok((left, top, width, height))
    }

    /// Replace exactly one retained raw Cb/Cr block with its candidate IDCT.
    ///
    /// Channel indices and coordinates have the same meaning as `render_block`.
    /// No other plane or block changes. Validation and IDCT finish on stack
    /// before any retained sample changes, so errors leave the renderer intact.
    /// The returned initial RGB is not retained or updated; callers apply a
    /// successful trial patch to their own RGB image when accepting a commit.
    pub fn commit_block(
        &mut self,
        component: usize,
        block_x: usize,
        block_y: usize,
        coeff: &[i16; 64],
    ) -> Result<(), DecodeErrors> {
        let samples = self.prepare_block(component, block_x, block_y, coeff)?;
        for row in 0..8 {
            let start = (block_y * 8 + row) * self.chroma_stride + block_x * 8;
            self.planes[component][start..start + 8]
                .copy_from_slice(&samples[row * 8..row * 8 + 8]);
        }
        Ok(())
    }

    /// Replace one retained raw Cb/Cr block with its candidate IDCT and save
    /// the previous 8x8 samples into `saved` so a rejected paired trial can
    /// restore them without re-running IDCT.
    pub fn swap_block(
        &mut self,
        component: usize,
        block_x: usize,
        block_y: usize,
        coeff: &[i16; 64],
        saved: &mut [i16; 64],
    ) -> Result<(), DecodeErrors> {
        let samples = self.prepare_block(component, block_x, block_y, coeff)?;
        for row in 0..8 {
            let start = (block_y * 8 + row) * self.chroma_stride + block_x * 8;
            let dst = &mut self.planes[component][start..start + 8];
            saved[row * 8..row * 8 + 8].copy_from_slice(dst);
            dst.copy_from_slice(&samples[row * 8..row * 8 + 8]);
        }
        Ok(())
    }

    /// Restore an 8x8 raw Cb/Cr block previously saved by `swap_block`.
    pub fn restore_block(
        &mut self,
        component: usize,
        block_x: usize,
        block_y: usize,
        saved: &[i16; 64],
    ) {
        for row in 0..8 {
            let start = (block_y * 8 + row) * self.chroma_stride + block_x * 8;
            self.planes[component][start..start + 8]
                .copy_from_slice(&saved[row * 8..row * 8 + 8]);
        }
    }

    fn prepare_block(
        &self,
        component: usize,
        block_x: usize,
        block_y: usize,
        coeff: &[i16; 64],
    ) -> Result<[i16; 64], DecodeErrors> {
        if !(1..=2).contains(&component)
            || block_x >= self.chroma_stride / 8
            || block_y >= self.chroma_height / 8
        {
            return Err(DecodeErrors::FormatStatic(
                "Chroma renderer: invalid component or block outside image",
            ));
        }
        let mut block = dequantize(coeff, &self.quantizers[component - 1])?;
        let mut samples = [0; 64];
        (self.idct)(&mut block, &mut samples, 8);
        Ok(samples)
    }

    fn raw_sample(
        &self,
        component: usize,
        x: usize,
        y: usize,
        candidate: Option<&Candidate<'_>>,
    ) -> i16 {
        if let Some(c) = candidate {
            if component == c.component
                && (c.left..c.left + 8).contains(&x)
                && (c.top..c.top + 8).contains(&y)
            {
                return c.samples[(y - c.top) * 8 + x - c.left];
            }
        }
        self.planes[component][y * self.chroma_stride + x]
    }

    fn vertical_sample(
        &self,
        component: usize,
        x: usize,
        y: usize,
        candidate: Option<&Candidate<'_>>,
    ) -> i16 {
        let raw_y = y / self.v;
        let a = self.raw_sample(component, x, raw_y, candidate);
        if self.v == 1 {
            return a;
        }
        let neighbor = if y % 2 == 0 {
            raw_y.saturating_sub(1)
        } else {
            (raw_y + 1).min(self.chroma_height - 1)
        };
        let b = self.raw_sample(component, x, neighbor, candidate);
        (3 * a + b + 2) >> 2
    }

    fn chroma_sample(
        &self,
        component: usize,
        x: usize,
        y: usize,
        candidate: Option<&Candidate<'_>>,
    ) -> i16 {
        let raw_x = x / self.h;
        let a = self.vertical_sample(component, raw_x, y, candidate);
        if self.h == 1 {
            return a;
        }
        let neighbor = if x % 2 == 0 {
            raw_x.saturating_sub(1)
        } else {
            (raw_x + 1).min(self.chroma_stride - 1)
        };
        // For 420, round the vertical samples FIRST, then horizontally blend.
        // Combining the two passes into a /16 expression is not equivalent.
        // IDCT samples are in 0..=255, so both i16 weighted sums are safe.
        let b = self.vertical_sample(component, neighbor, y, candidate);
        (3 * a + b + 2) >> 2
    }

    fn render_rectangle(
        &self,
        left: usize,
        top: usize,
        width: usize,
        height: usize,
        candidate: Option<&Candidate<'_>>,
        out: &mut [u8],
    ) {
        let mut y = [0; 16];
        let mut cb = [0; 16];
        let mut cr = [0; 16];
        let mut rgb = [0; 48];
        if self.h == 1 && self.v == 1 {
            if let Some(c) = candidate {
                if left == c.left && top == c.top && width <= 8 && height <= 8 {
                    let y_plane = &self.planes[0];
                    let cb_plane = &self.planes[1];
                    let cr_plane = &self.planes[2];
                    for row in 0..height {
                        let py = top + row;
                        let y_off = py * self.stride + left;
                        let c_off = py * self.chroma_stride + left;
                        let s_off = row * 8;
                        y[..width].copy_from_slice(&y_plane[y_off..y_off + width]);
                        if c.component == 1 {
                            cb[..width].copy_from_slice(&c.samples[s_off..s_off + width]);
                            cr[..width].copy_from_slice(&cr_plane[c_off..c_off + width]);
                        } else {
                            cb[..width].copy_from_slice(&cb_plane[c_off..c_off + width]);
                            cr[..width].copy_from_slice(&c.samples[s_off..s_off + width]);
                        }
                        (self.rgb)(&y, &cb, &cr, &mut rgb, &mut 0);
                        let start = row * width * 3;
                        out[start..start + width * 3].copy_from_slice(&rgb[..width * 3]);
                    }
                    return;
                }
            }
            let y_plane = &self.planes[0];
            let cb_plane = &self.planes[1];
            let cr_plane = &self.planes[2];
            for row in 0..height {
                let py = top + row;
                let y_row = py * self.stride + left;
                let c_row = py * self.chroma_stride + left;
                for col in (0..width).step_by(16) {
                    let lanes = (width - col).min(16);
                    y[..lanes].copy_from_slice(&y_plane[y_row + col..y_row + col + lanes]);
                    cb[..lanes].copy_from_slice(&cb_plane[c_row + col..c_row + col + lanes]);
                    cr[..lanes].copy_from_slice(&cr_plane[c_row + col..c_row + col + lanes]);
                    (self.rgb)(&y, &cb, &cr, &mut rgb, &mut 0);
                    let start = (row * width + col) * 3;
                    out[start..start + lanes * 3].copy_from_slice(&rgb[..lanes * 3]);
                }
            }
            return;
        }
        let cand_cb = candidate.filter(|c| c.component == 1);
        let cand_cr = candidate.filter(|c| c.component == 2);
        for row in 0..height {
            let py = top + row;
            let y_row = py * self.stride + left;
            for col in (0..width).step_by(16) {
                let lanes = (width - col).min(16);
                y[..lanes].copy_from_slice(&self.planes[0][y_row + col..y_row + col + lanes]);
                for lane in 0..lanes {
                    let px = left + col + lane;
                    cb[lane] = self.chroma_sample(1, px, py, cand_cb);
                    cr[lane] = self.chroma_sample(2, px, py, cand_cr);
                }
                // Conversion is lane-independent; unused lanes stay on stack.
                (self.rgb)(&y, &cb, &cr, &mut rgb, &mut 0);
                let start = (row * width + col) * 3;
                out[start..start + lanes * 3].copy_from_slice(&rgb[..lanes * 3]);
            }
        }
    }
}
