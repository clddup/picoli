//! Experimental standard-JPEG coefficient input. No RGB round trip or FDCT.
use super::{
    AlignedBlock, Encoder, EncodingError, JfifWrite, JpegColorType, Marker, QuantizationTable,
    QuantizationTableType, SamplingFactor, ZIGZAG,
};
use alloc::{boxed::Box, vec::Vec};
mod rate_model;
pub use rate_model::CoefficientRateModel;

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
