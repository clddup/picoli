//! Experimental standard-JPEG coefficient input. No RGB round trip or FDCT.
use super::{
    AlignedBlock, ChromaSubsamplingMethod, DefaultOperations, Encoder, EncodingError, ImageBuffer,
    JfifWrite, JpegColorType, Marker, Operations, QuantizationTable, QuantizationTableType,
    RgbImage, SamplingFactor, ZIGZAG, get_block, get_block_averaged,
};
use alloc::{boxed::Box, vec::Vec};
mod rate_model;
pub use rate_model::CoefficientRateModel;

/// Precomputed natural-order FDCT blocks for one RGB image and sampling layout.
/// Avoids repeating RGB->YCbCr, chroma averaging and FDCT across table searches.
pub struct PreparedFdct {
    pub width: u16,
    pub height: u16,
    pub sampling_factor: SamplingFactor,
    pub h_max: usize,
    pub v_max: usize,
    pub mcu_x: usize,
    pub mcu_y: usize,
    pub components: [PreparedFdctComponent; 3],
}

pub struct PreparedFdctComponent {
    pub h_sample: u8,
    pub v_sample: u8,
    pub cols: usize,
    pub rows: usize,
    pub block_width: usize,
    pub block_height: usize,
    pub blocks: Vec<AlignedBlock>,
}

/// Precomputed luma/chroma quantizer tables for repeated row-by-row quantization.
pub struct PreparedQuantizers {
    tables: [QuantizationTable; 2],
}

impl PreparedQuantizers {
    pub fn new(luma: &[u16; 64], chroma: &[u16; 64]) -> Self {
        Self {
            tables: [
                QuantizationTable::new_with_quality(
                    &QuantizationTableType::Custom(Box::new(*luma)),
                    100,
                    true,
                ),
                QuantizationTable::new_with_quality(
                    &QuantizationTableType::Custom(Box::new(*chroma)),
                    100,
                    false,
                ),
            ],
        }
    }
}

impl PreparedFdct {
    pub fn new(
        rgb: &[u8],
        width: u16,
        height: u16,
        sampling_factor: SamplingFactor,
        chroma_subsampling_method: ChromaSubsamplingMethod,
    ) -> Result<Self, EncodingError> {
        let required = usize::from(width) * usize::from(height) * 3;
        if width == 0 || height == 0 {
            return Err(EncodingError::ZeroImageDimensions { width, height });
        }
        if rgb.len() < required {
            return Err(EncodingError::BadImageData {
                length: rgb.len(),
                required,
            });
        }
        if !matches!(
            sampling_factor,
            SamplingFactor::R_4_4_4
                | SamplingFactor::R_4_2_2
                | SamplingFactor::R_4_2_0
                | SamplingFactor::F_1_1
                | SamplingFactor::F_2_1
                | SamplingFactor::F_2_2
        ) {
            return Err(EncodingError::InvalidConfiguration(
                "PreparedFdct supports 444/422/420 sampling only",
            ));
        }
        let image = RgbImage(rgb, width, height);
        let (h_u8, v_u8) = sampling_factor.get_sampling_factors();
        let (max_h, max_v) = (usize::from(h_u8), usize::from(v_u8));
        let mcu_x = usize::from(width).div_ceil(8 * max_h);
        let mcu_y = usize::from(height).div_ceil(8 * max_v);
        let padded_cols = mcu_x * max_h;
        let padded_rows = mcu_y * max_v;
        let buffer_width = padded_cols * 8;
        let buffer_size = padded_cols * padded_rows * 64;

        let mut row = [
            Vec::with_capacity(buffer_size),
            Vec::with_capacity(buffer_size),
            Vec::with_capacity(buffer_size),
            Vec::new(),
        ];
        for y in 0..padded_rows * 8 {
            let clamped_y = (y.min(usize::from(height) - 1)) as u16;
            image.fill_buffers(clamped_y, &mut row);
            for _ in usize::from(width)..buffer_width {
                for channel in &mut row[..3] {
                    channel.push(channel[channel.len() - 1]);
                }
            }
        }

        let num_cols = usize::from(width).div_ceil(8);
        let num_rows = usize::from(height).div_ceil(8);
        let specs = [(h_u8, v_u8), (1, 1), (1, 1)];
        let mut components: [Option<PreparedFdctComponent>; 3] = [None, None, None];
        for (i, &(hs, vs)) in specs.iter().enumerate() {
            let h_scale = max_h / usize::from(hs);
            let v_scale = max_v / usize::from(vs);
            let cols = num_cols.div_ceil(h_scale);
            let rows = num_rows.div_ceil(v_scale);
            let block_width = mcu_x * usize::from(hs);
            let block_height = mcu_y * usize::from(vs);
            let average = chroma_subsampling_method == ChromaSubsamplingMethod::Average
                && (h_scale > 1 || v_scale > 1);
            let mut blocks = Vec::with_capacity(cols * rows);
            for block_y in 0..rows {
                for block_x in 0..cols {
                    let bx = block_x * 8 * h_scale;
                    let by = block_y * 8 * v_scale;
                    let mut block = if average {
                        get_block_averaged(&row[i], bx, by, h_scale, v_scale, buffer_width)
                    } else {
                        get_block(&row[i], bx, by, h_scale, v_scale, buffer_width)
                    };
                    DefaultOperations::fdct(&mut block);
                    blocks.push(block);
                }
            }
            components[i] = Some(PreparedFdctComponent {
                h_sample: hs,
                v_sample: vs,
                cols,
                rows,
                block_width,
                block_height,
                blocks,
            });
        }
        let [c0, c1, c2] = components;
        Ok(Self {
            width,
            height,
            sampling_factor,
            h_max: max_h,
            v_max: max_v,
            mcu_x,
            mcu_y,
            components: [c0.unwrap(), c1.unwrap(), c2.unwrap()],
        })
    }

    pub fn allocate_coeffs(&self) -> [Vec<i16>; 3] {
        core::array::from_fn(|i| {
            let comp = &self.components[i];
            alloc::vec![0i16; comp.block_width * comp.block_height * 64]
        })
    }

    /// Quantize a single MCU row (`0..self.mcu_y`) into `out`, which must be
    /// pre-sized by `allocate_coeffs()`. Allows early-aborting quality checks
    /// without quantizing later MCU rows.
    pub fn quantize_mcu_row(
        &self,
        mcu_row: usize,
        quantizers: &PreparedQuantizers,
        out: &mut [Vec<i16>; 3],
    ) {
        for (i, comp) in self.components.iter().enumerate() {
            let table = &quantizers.tables[usize::from(i > 0)];
            let vs = usize::from(comp.v_sample);
            let by_start = mcu_row * vs;
            let by_end = (by_start + vs).min(comp.block_height);
            for by in by_start..by_end {
                let row_dst = by * comp.block_width * 64;
                if by < comp.rows {
                    for bx in 0..comp.cols {
                        let src = &comp.blocks[by * comp.cols + bx].data;
                        let dst = &mut out[i][row_dst + bx * 64..row_dst + (bx + 1) * 64];
                        for z in 0..64 {
                            dst[z] = table.quantize(src[z], z);
                        }
                    }
                    if comp.cols < comp.block_width {
                        out[i][row_dst + comp.cols * 64..row_dst + comp.block_width * 64].fill(0);
                    }
                } else {
                    out[i][row_dst..row_dst + comp.block_width * 64].fill(0);
                }
            }
        }
    }

    pub fn quantize_into(
        &self,
        luma: &[u16; 64],
        chroma: &[u16; 64],
        out: &mut [Vec<i16>; 3],
    ) {
        let quantizers = PreparedQuantizers::new(luma, chroma);
        for (i, comp) in self.components.iter().enumerate() {
            let total = comp.block_width * comp.block_height * 64;
            if out[i].len() != total {
                out[i].resize(total, 0);
            }
        }
        for mcu_row in 0..self.mcu_y {
            self.quantize_mcu_row(mcu_row, &quantizers, out);
        }
    }
}

/// Borrowed quantized coefficients in NATURAL order, 64 values per block.
/// Padding blocks may be present; only the actual image data-unit grid is coded.
pub struct QuantizedComponent<'a> {
    pub horizontal_sampling_factor: u8,
    pub vertical_sampling_factor: u8,
    pub quantization_table: &'a [u16; 64],
    pub block_width: usize,
    pub block_height: usize,
    pub coefficients: &'a [i16],
}
/// JPEG Luma or standard YCbCr4:4:4/4:2:2/4:2:0 coefficient planes.
/// RGB-coded JPEG/CMYK/differentCbCr tables/16-bitDQT are not silently converted.
pub struct QuantizedImage<'a> {
    pub width: u16,
    pub height: u16,
    pub color_type: JpegColorType,
    pub components: &'a [QuantizedComponent<'a>],
}
impl<W: JfifWrite> Encoder<W> {
    /// Serialize supplied coefficients without dequantization/requantization,
    /// color conversion, resampling or FDCT. Sampling/tables come from the
    /// coefficient frame, not the pixel-input quality settings. Progressive2/4
    /// is required; configured pixel AC RDO is rejected rather than ignored.
    pub fn encode_quantized(self, image: QuantizedImage<'_>) -> Result<(), EncodingError> {
        self.encode_quantized_impl(image, false).map(|_| ())
    }
    /// Select component-specific AC spectral partitions using exact emitted-byte
    /// costs on fixed DCT frequency-shell endpoints (plus legacy split42).
    /// Every coefficient, DQT and sampling factor remains unchanged. Requires
    /// optimized Huffman tables, EOBRUN and per-scan Huffman configuration.
    pub fn encode_quantized_optimized_scans(
        self,
        image: QuantizedImage<'_>,
    ) -> Result<super::ScanOptimizationReport, EncodingError> {
        self.encode_quantized_impl(image, true)?
            .ok_or(EncodingError::InvalidConfiguration(
                "Missing optimized scan report",
            ))
    }
    fn encode_quantized_impl(
        mut self,
        image: QuantizedImage<'_>,
        optimized: bool,
    ) -> Result<Option<super::ScanOptimizationReport>, EncodingError> {
        let bad = EncodingError::InvalidConfiguration;
        if optimized
            && !(self.optimize_huffman_table
                && self.progressive_eob_runs
                && self.progressive_scan_huffman)
        {
            return Err(bad(
                "Scan optimization requires optimized Huffman/EOBRUN/per-scan tables",
            ));
        }
        if image.width == 0 || image.height == 0 {
            return Err(EncodingError::ZeroImageDimensions {
                width: image.width,
                height: image.height,
            });
        }
        let scans = self
            .progressive_scans
            .ok_or_else(|| bad("Coefficient input requires progressive coding"))?;
        if !matches!(scans, 2 | 4) || self.ac_rdo_lambda != 0 {
            return Err(bad(
                "Coefficient input requires scans2/4 and no pixel-input AC RDO",
            ));
        }
        if self.progressive_eob_runs && !self.optimize_huffman_table {
            return Err(bad("EOB runs require optimized Huffman tables"));
        }
        if self.progressive_scan_huffman && !self.progressive_eob_runs {
            return Err(bad("Per-scan Huffman tables require EOB runs"));
        }
        let n = match image.color_type {
            JpegColorType::Luma => 1,
            JpegColorType::Ycbcr => 3,
            _ => return Err(bad("Coefficient input supports Luma or YCbCr only")),
        };
        if image.components.len() != n {
            return Err(bad("Coefficient component count mismatch"));
        }
        let y = &image.components[0];
        let (h, v) = (y.horizontal_sampling_factor, y.vertical_sampling_factor);
        self.sampling_factor = match (image.color_type, h, v) {
            (JpegColorType::Luma, 1, 1) | (JpegColorType::Ycbcr, 1, 1) => SamplingFactor::R_4_4_4,
            (JpegColorType::Ycbcr, 2, 1) => SamplingFactor::R_4_2_2,
            (JpegColorType::Ycbcr, 2, 2) => SamplingFactor::R_4_2_0,
            _ => return Err(bad("Unsupported coefficient sampling layout")),
        };
        if n == 3 {
            if image.components[1..]
                .iter()
                .any(|c| c.horizontal_sampling_factor != 1 || c.vertical_sampling_factor != 1)
            {
                return Err(bad("Chroma coefficient footprints must be1x1"));
            }
            if image.components[1].quantization_table != image.components[2].quantization_table {
                return Err(bad(
                    "Distinct Cb/Cr quantizers not supported by this coefficient writer",
                ));
            }
        }
        if image
            .components
            .iter()
            .any(|c| c.quantization_table.iter().any(|q| !(1..=255).contains(q)))
        {
            return Err(bad("Coefficient input requires8-bit nonzero DQT"));
        }
        let chroma = if n == 3 {
            image.components[1].quantization_table
        } else {
            y.quantization_table
        };
        let q_tables = [
            QuantizationTable::new_with_quality(
                &QuantizationTableType::Custom(Box::new(*y.quantization_table)),
                100,
                true,
            ),
            QuantizationTable::new_with_quality(
                &QuantizationTableType::Custom(Box::new(*chroma)),
                100,
                false,
            ),
        ];
        let mut blocks: [Vec<AlignedBlock>; 4] = core::array::from_fn(|_| Vec::new());
        for (i, c) in image.components.iter().enumerate() {
            let expected = c
                .block_width
                .checked_mul(c.block_height)
                .and_then(|n| n.checked_mul(64))
                .ok_or_else(|| bad("Coefficient input length overflow"))?;
            if c.coefficients.len() != expected {
                return Err(bad("Coefficient plane length mismatch"));
            }
            let cols = (usize::from(image.width) * usize::from(c.horizontal_sampling_factor))
                .div_ceil(8 * usize::from(h));
            let rows = (usize::from(image.height) * usize::from(c.vertical_sampling_factor))
                .div_ceil(8 * usize::from(v));
            if c.block_width < cols || c.block_height < rows {
                return Err(bad("Coefficient plane does not cover image"));
            }
            blocks[i] = Vec::with_capacity(cols * rows);
            let mut previous_dc = 0i16;
            let restart = usize::from(self.restart_interval.unwrap_or(0));
            for by in 0..rows {
                for bx in 0..cols {
                    let offset = (by * c.block_width + bx) * 64;
                    let q = AlignedBlock::new(core::array::from_fn(|z| {
                        c.coefficients[offset + (ZIGZAG[z] as usize & 0x3f)]
                    }));
                    let index = blocks[i].len();
                    if restart > 0 && index.is_multiple_of(restart) {
                        previous_dc = 0;
                    }
                    let delta = q.data[0]
                        .checked_sub(previous_dc)
                        .ok_or_else(|| bad("Coefficient DC difference overflow"))?;
                    if delta.unsigned_abs() > 2047
                        || q.data[1..].iter().any(|x| x.unsigned_abs() > 1023)
                    {
                        return Err(bad(
                            "Coefficient magnitude exceeds8-bit JPEG coding categories",
                        ));
                    }
                    previous_dc = q.data[0];
                    blocks[i].push(q);
                }
            }
        }
        // All layouts, lengths and coding categories checked before any header.
        self.init_components(image.color_type);
        let report = if optimized {
            Some(super::optimized_scan::plan(
                &self.components,
                &blocks,
                self.restart_interval.unwrap_or(0),
            )?)
        } else {
            None
        };
        self.writer.write_marker(Marker::SOI)?;
        self.writer.write_header(&self.density)?;
        for (nr, data) in &self.app_segments {
            self.writer.write_segment(Marker::APP(*nr), data)?;
        }
        if self.optimize_huffman_table {
            self.optimize_huffman_table(&blocks);
        }
        self.write_frame_header_values(image.width, image.height, n, &q_tables)?;
        if let Some(report) = &report {
            self.write_progressive_dc_coefficients(&blocks)?;
            for (i, bands) in report.component_bands.iter().enumerate() {
                for &(start, end) in bands {
                    super::optimized_scan::write_band(
                        &mut self.writer,
                        &self.components[i],
                        &blocks[i],
                        usize::from(start),
                        usize::from(end),
                        self.restart_interval.unwrap_or(0),
                    )?;
                }
            }
        } else {
            self.write_progressive_coefficients(scans, &blocks)?;
        }
        self.writer.write_marker(Marker::EOI)?;
        Ok(report)
    }
}
#[cfg(test)]
mod tests;
