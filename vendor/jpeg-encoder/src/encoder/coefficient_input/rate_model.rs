//! Reuse the existing independent-block standard-Huffman AC prior. This is
//! only a proposal screen, not progressive EOBRUN/final optimized-JPEG rate.
use super::ZIGZAG;
use crate::huffman::HuffmanTable;
mod context;
/// Allocation-free per-block rate queries after constructing the two priors.
/// Does not include DC, DHT/header overhead, bit padding, byte stuffing, scan-wide
/// optimized histograms or cross-block progressive EOBRUN savings.
pub struct CoefficientRateModel {
    codes: [[u8; 256]; 2],
    scans: u8,
}
impl CoefficientRateModel {
    /// Map the fixed JPEG zigzag position to its natural block index.
    pub fn natural_index(position: usize) -> Option<usize> {
        ZIGZAG.get(position).map(|v| usize::from(v & 0x3f))
    }

    /// Only the supported progressive2/4 spectral layouts are meaningful.
    pub fn new(scans: u8) -> Option<Self> {
        if !matches!(scans, 2 | 4) {
            return None;
        }
        let tables = [
            HuffmanTable::default_luma_ac(),
            HuffmanTable::default_chroma_ac(),
        ];
        let codes = core::array::from_fn(|i| {
            core::array::from_fn(|symbol| {
                if symbol == 0 || symbol == 0xf0 || (1..=10).contains(&(symbol & 15)) {
                    tables[i].get_for_value(symbol as u8).0
                } else {
                    0
                }
            })
        });
        Some(Self { codes, scans })
    }
    /// Natural-order quantized AC coefficients, using the same prior and rate
    /// calculation as the existing coefficient trellis, without changing it.
    pub fn bits(&self, natural: &[i16; 64], chroma: bool) -> Option<u32> {
        let zigzag = core::array::from_fn(|z| natural[ZIGZAG[z] as usize & 0x3f]);
        crate::progressive::ac_ranges(self.scans).try_fold(0u32, |sum, (start, end)| {
            sum.checked_add(crate::coefficient_rdo::rate(
                &zigzag,
                start,
                end,
                &self.codes[usize::from(chroma)],
            )?)
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prior_preserves_existing_rate_and_dc_is_not_modeled() {
        assert!(CoefficientRateModel::new(3).is_none());
        for scans in [2, 4] {
            for chroma in [false, true] {
                let model = CoefficientRateModel::new(scans).unwrap();
                let zero = [0; 64];
                let mut dc = zero;
                dc[0] = i16::MAX;
                assert_eq!(model.bits(&zero, chroma), model.bits(&dc, chroma));
                let mut signal = zero;
                signal[1] = 1;
                assert!(model.bits(&signal, chroma).unwrap() > model.bits(&zero, chroma).unwrap());
                signal[1] = 1024;
                assert!(model.bits(&signal, chroma).is_none());
                for k in 1..64 {
                    let mut natural = [0; 64];
                    natural[k] = -3;
                    let zz = core::array::from_fn(|z| natural[ZIGZAG[z] as usize & 0x3f]);
                    let prior = if chroma {
                        HuffmanTable::default_chroma_ac()
                    } else {
                        HuffmanTable::default_luma_ac()
                    };
                    let codes = core::array::from_fn(|s| {
                        if s == 0 || s == 0xf0 || (1..=10).contains(&(s & 15)) {
                            prior.get_for_value(s as u8).0
                        } else {
                            0
                        }
                    });
                    let expected: u32 = crate::progressive::ac_ranges(scans)
                        .map(|(a, b)| crate::coefficient_rdo::rate(&zz, a, b, &codes).unwrap())
                        .sum();
                    assert_eq!(model.bits(&natural, chroma), Some(expected));
                }
            }
        }
    }
}
