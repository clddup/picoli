//! Shared exact optimized AC-band writer. Optional scan-partition DP changes
//! no coefficient or quantizer; its cost includes real DHT/SOS, padding/stuffing.
use super::{AlignedBlock, Component, EncodingError, JfifWrite, JfifWriter};
use crate::{
    huffman::{CodingClass, HuffmanTable},
    marker::Marker,
    progressive::{self, AcEvent},
};
#[cfg(feature = "experimental-coefficients")]
use alloc::vec::Vec;
pub(super) fn write_band<W: JfifWrite>(
    writer: &mut JfifWriter<W>,
    component: &Component,
    blocks: &[AlignedBlock],
    start: usize,
    end: usize,
    restart: u16,
) -> Result<(), EncodingError> {
    let mut frequencies = [0u32; 257];
    frequencies[256] = 1;
    progressive::ac_events(blocks, start, end, restart, |event| {
        if let AcEvent::Symbol { symbol, .. } = event {
            frequencies[usize::from(symbol)] += 1;
        }
        Ok::<(), core::convert::Infallible>(())
    })
    .unwrap_or_else(|never| match never {});
    let table = HuffmanTable::new_optimized(frequencies);
    writer.write_huffman_segment(CodingClass::Ac, component.ac_huffman_table, &table)?;
    writer.write_scan_header(&[component], Some((start as u8, end as u8 - 1)))?;
    progressive::ac_events(blocks, start, end, restart, |event| match event {
        AcEvent::Symbol {
            symbol,
            value,
            bits,
        } => writer.huffman_encode_value(bits, symbol, value, &table),
        AcEvent::Restart(number) => {
            writer.finalize_bit_buffer()?;
            writer.write_marker(Marker::RST(number))
        }
    })?;
    writer.finalize_bit_buffer()
}
/// Exact-byte DP result on analytical DCT frequency-shell endpoints plus the
/// existing legacy split42. Not a new JPEG syntax or perceptual quality metric.
#[cfg(feature = "experimental-coefficients")]
#[derive(Debug)]
pub struct ScanOptimizationReport {
    /// Half-open zigzag AC bands for each component; DC remains separate.
    pub component_bands: Vec<Vec<(u8, u8)>>,
    /// Includes DHT/SOS, entropy, byte padding/stuffing and restart markers.
    pub predicted_ac_bytes: u64,
    pub interval_evaluations: usize,
}
#[cfg(feature = "experimental-coefficients")]
const CUTS: [usize; 12] = [1, 3, 6, 10, 15, 21, 28, 36, 42, 45, 55, 64];
#[cfg(feature = "experimental-coefficients")]
struct Counter<'a>(&'a mut u64);
#[cfg(feature = "experimental-coefficients")]
impl JfifWrite for Counter<'_> {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), EncodingError> {
        *self.0 =
            self.0
                .checked_add(buf.len() as u64)
                .ok_or(EncodingError::InvalidConfiguration(
                    "AC byte counter overflow",
                ))?;
        Ok(())
    }
}
#[cfg(feature = "experimental-coefficients")]
fn measured(
    component: &Component,
    blocks: &[AlignedBlock],
    a: usize,
    b: usize,
    restart: u16,
) -> Result<u64, EncodingError> {
    let mut bytes = 0;
    {
        let mut writer = JfifWriter::new(Counter(&mut bytes));
        write_band(&mut writer, component, blocks, a, b, restart)?;
    }
    Ok(bytes)
}
#[cfg(feature = "experimental-coefficients")]
fn shortest(cost: &[[u64; 12]; 12]) -> Result<(Vec<(u8, u8)>, u64), EncodingError> {
    let bad = EncodingError::InvalidConfiguration;
    let mut best = [u64::MAX; 12];
    let mut count = [usize::MAX; 12];
    let mut previous = [0; 12];
    best[0] = 0;
    count[0] = 0;
    for end in 1..12 {
        for start in 0..end {
            let candidate = best[start]
                .checked_add(cost[start][end])
                .ok_or_else(|| bad("AC partition cost overflow"))?;
            let scans = count[start] + 1;
            // Equal byte cost prefers fewer scans, then the longer final band. Starts
            // are visited ascending, so a complete tie retains its earlier predecessor.
            if candidate < best[end] || (candidate == best[end] && scans < count[end]) {
                best[end] = candidate;
                count[end] = scans;
                previous[end] = start;
            }
        }
    }
    let mut bands = Vec::new();
    bands
        .try_reserve_exact(count[11])
        .map_err(|_| bad("AC partition allocation failed"))?;
    let mut end = 11;
    while end != 0 {
        let start = previous[end];
        bands.push((CUTS[start] as u8, CUTS[end] as u8));
        end = start;
    }
    bands.reverse();
    Ok((bands, best[11]))
}
#[cfg(feature = "experimental-coefficients")]
pub(super) fn plan(
    components: &[Component],
    blocks: &[alloc::vec::Vec<AlignedBlock>; 4],
    restart: u16,
) -> Result<ScanOptimizationReport, EncodingError> {
    let mut report = ScanOptimizationReport {
        component_bands: Vec::new(),
        predicted_ac_bytes: 0,
        interval_evaluations: 0,
    };
    for (i, component) in components.iter().enumerate() {
        let mut costs = [[0u64; 12]; 12];
        for a in 0..11 {
            for b in a + 1..12 {
                costs[a][b] = measured(component, &blocks[i], CUTS[a], CUTS[b], restart)?;
                report.interval_evaluations += 1;
            }
        }
        let (bands, bytes) = shortest(&costs)?;
        report.predicted_ac_bytes = report.predicted_ac_bytes.checked_add(bytes).ok_or(
            EncodingError::InvalidConfiguration("AC total byte cost overflow"),
        )?;
        report.component_bands.push(bands);
    }
    Ok(report)
}
#[cfg(all(test, feature = "experimental-coefficients"))]
mod tests;
