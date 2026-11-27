//! Project-local spectral-selection AC event coding with JPEG EOBRUNs.
//! This implements standard JPEG run syntax, not a new compression invention.
//! One event walker drives both Huffman statistics and serialization.
use crate::encoder::AlignedBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcEvent {
    Symbol { symbol: u8, value: u16, bits: u8 },
    Restart(u8),
}

fn flush<E>(run: &mut u16, emit: &mut impl FnMut(AcEvent) -> Result<(), E>) -> Result<(), E> {
    if *run != 0 {
        let bits = run.ilog2() as u8;
        emit(AcEvent::Symbol {
            symbol: bits << 4,
            value: *run - (1u16 << bits),
            bits,
        })?;
        *run = 0;
    }
    Ok(())
}

/// Nonempty AC bands cover coefficients 1..64 exactly once. Preserve the
/// upstream boundaries for 2..33 total scans, including product layouts 2/4.
/// With more scans the upstream 64/n formula produced an empty first band.
pub(crate) fn ac_ranges(scans: u8) -> impl Iterator<Item = (usize, usize)> {
    debug_assert!((2..=64).contains(&scans));
    let count = usize::from(scans) - 1;
    let step = 64 / count;
    let offset = usize::from(step == 1);
    (0..count).map(move |scan| {
        let start = (offset + scan * step).max(1);
        let end = if scan + 1 == count {
            64
        } else {
            offset + (scan + 1) * step
        };
        (start, end)
    })
}

pub(crate) fn ac_events<E>(
    blocks: &[AlignedBlock],
    start: usize,
    end: usize,
    restart_interval: u16,
    mut emit: impl FnMut(AcEvent) -> Result<(), E>,
) -> Result<(), E> {
    let mut eob_run = 0u16;
    for (block_index, block) in blocks.iter().enumerate() {
        if restart_interval != 0
            && block_index > 0
            && block_index % usize::from(restart_interval) == 0
        {
            flush(&mut eob_run, &mut emit)?;
            let restart = ((block_index / usize::from(restart_interval) - 1) % 8) as u8;
            emit(AcEvent::Restart(restart))?;
        }
        if let Some(last) = block.data[start..end].iter().rposition(|&v| v != 0) {
            // EOBRUN covers only trailing zeros and subsequent all-zero blocks.
            // A new nonzero coefficient must first consume the pending run.
            flush(&mut eob_run, &mut emit)?;
            let mut zeros = 0u8;
            for &value in &block.data[start..=start + last] {
                if value == 0 {
                    zeros += 1;
                    continue;
                }
                while zeros >= 16 {
                    emit(AcEvent::Symbol {
                        symbol: 0xf0,
                        value: 0,
                        bits: 0,
                    })?;
                    zeros -= 16;
                }
                let bits = (16 - value.unsigned_abs().leading_zeros()) as u8;
                let magnitude = if value < 0 {
                    i32::from(value) - 1
                } else {
                    i32::from(value)
                };
                let encoded = (magnitude as u32 & ((1u32 << bits) - 1)) as u16;
                emit(AcEvent::Symbol {
                    symbol: (zeros << 4) | bits,
                    value: encoded,
                    bits,
                })?;
                zeros = 0;
            }
            if start + last + 1 < end {
                eob_run += 1;
            }
        } else {
            eob_run += 1;
        }
        // r=15 is reserved for ZRL, so an EOBRUN cannot exceed 2^15-1.
        if eob_run == 32767 {
            flush(&mut eob_run, &mut emit)?;
        }
    }
    flush(&mut eob_run, &mut emit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use core::convert::Infallible;

    fn collect(blocks: &[AlignedBlock], start: usize, end: usize, restart: u16) -> Vec<AcEvent> {
        let mut events = Vec::new();
        ac_events(blocks, start, end, restart, |event| {
            events.push(event);
            Ok::<(), Infallible>(())
        })
        .unwrap_or_else(|never| match never {});
        events
    }
    #[test]
    fn encoded_pixels_match_without_runs_for_sampling_restarts_and_odd_dimensions() {
        use crate::{ColorType, Encoder, SamplingFactor};
        use jpeg_decoder::Decoder;
        for pattern in 0..3 {
            let (width, height) = (37u16, 29u16);
            let mut rgb = Vec::new();
            for y in 0..height {
                for x in 0..width {
                    rgb.extend_from_slice(&match pattern {
                        0 => [128, 128, 128],
                        1 => [(x * 5) as u8, (y * 7) as u8, ((x + y) * 3) as u8],
                        _ => [
                            ((x * 71 + y * 29) % 256) as u8,
                            ((x * 13 + y * 97) % 256) as u8,
                            ((x * 43 + y * 17) % 256) as u8,
                        ],
                    });
                }
            }
            for sampling in [
                SamplingFactor::R_4_4_4,
                SamplingFactor::R_4_2_2,
                SamplingFactor::R_4_2_0,
            ] {
                for scans in [2, 4] {
                    for restart in [0, 1, 4] {
                        let encode = |runs| {
                            let mut out = Vec::new();
                            let mut encoder = Encoder::new(&mut out, 93);
                            encoder.set_sampling_factor(sampling);
                            encoder.set_progressive_scans(scans);
                            encoder.set_optimized_huffman_tables(true);
                            encoder.set_restart_interval(restart);
                            encoder.set_progressive_eob_runs(runs);
                            encoder.encode(&rgb, width, height, ColorType::Rgb).unwrap();
                            out
                        };
                        let old = encode(false);
                        let new = encode(true);
                        let mut before = Decoder::new(old.as_slice());
                        let before_pixels = before.decode().unwrap();
                        let mut after = Decoder::new(new.as_slice());
                        let after_pixels = after.decode().unwrap();
                        assert_eq!(before.info(), after.info());
                        assert_eq!(before_pixels, after_pixels);
                    }
                }
            }
        }
    }
    #[test]
    fn scan_ranges_are_complete_nonempty_and_preserve_legacy_valid_layouts() {
        for scans in 2..=64 {
            let ranges: Vec<_> = ac_ranges(scans).collect();
            assert_eq!(ranges.len(), usize::from(scans) - 1);
            let mut next = 1;
            for &(start, end) in &ranges {
                assert_eq!(start, next);
                assert!(start < end && end <= 64);
                next = end;
            }
            assert_eq!(next, 64);
            if scans <= 33 {
                let count = usize::from(scans) - 1;
                let step = 64 / count;
                let legacy: Vec<_> = (0..count)
                    .map(|i| {
                        (
                            (i * step).max(1),
                            if i + 1 == count { 64 } else { (i + 1) * step },
                        )
                    })
                    .collect();
                assert_eq!(ranges, legacy);
            }
        }
    }

    #[test]
    fn every_accepted_scan_count_decodes_to_the_same_pixels() {
        use crate::{ColorType, Encoder, SamplingFactor};
        use jpeg_decoder::Decoder;
        let (width, height) = (37u16, 29u16);
        let rgb: Vec<_> = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    [
                        ((x * 71 + y * 29) % 256) as u8,
                        ((x * 13 + y * 97) % 256) as u8,
                        ((x * 43 + y * 17) % 256) as u8,
                    ]
                })
            })
            .collect();
        for sampling in [
            SamplingFactor::R_4_4_4,
            SamplingFactor::R_4_2_2,
            SamplingFactor::R_4_2_0,
        ] {
            let mut expected = None;
            for scans in 2..=64 {
                for runs in [false, true] {
                    let mut out = Vec::new();
                    let mut encoder = Encoder::new(&mut out, 93);
                    encoder.set_sampling_factor(sampling);
                    encoder.set_progressive_scans(scans);
                    encoder.set_optimized_huffman_tables(true);
                    encoder.set_progressive_eob_runs(runs);
                    encoder.encode(&rgb, width, height, ColorType::Rgb).unwrap();
                    let pixels = Decoder::new(out.as_slice())
                        .decode()
                        .unwrap_or_else(|error| {
                            panic!("sampling={sampling:?}, scans={scans}, runs={runs}: {error}")
                        });
                    if let Some(reference) = &expected {
                        assert_eq!(
                            &pixels, reference,
                            "sampling={sampling:?}, scans={scans}, runs={runs}"
                        );
                    } else {
                        expected = Some(pixels);
                    }
                }
            }
        }
    }

    #[test]
    fn incompatible_eob_options_are_errors_not_silent_fallbacks() {
        use crate::{ColorType, Encoder, EncodingError};
        for (progressive, optimized) in [(false, false), (true, false), (false, true)] {
            let mut out = Vec::new();
            let mut encoder = Encoder::new(&mut out, 90);
            encoder.set_progressive(progressive);
            encoder.set_optimized_huffman_tables(optimized);
            encoder.set_progressive_eob_runs(true);
            assert!(matches!(
                encoder.encode(&[128; 3], 1, 1, ColorType::Rgb),
                Err(EncodingError::InvalidConfiguration(_))
            ));
        }
    }
    #[test]
    fn eob_runs_cover_every_zero_block_and_split_at_the_spec_limit() {
        for count in [1, 2, 3, 7, 8, 32766, 32767, 32768, 65534] {
            let blocks: Vec<_> = (0..count).map(|_| AlignedBlock::default()).collect();
            let events = collect(&blocks, 1, 64, 0);
            let mut covered = 0usize;
            for event in events {
                let AcEvent::Symbol {
                    symbol,
                    value,
                    bits,
                } = event
                else {
                    panic!("unexpected restart")
                };
                assert_eq!(symbol, bits << 4);
                assert!(bits <= 14);
                covered += (1usize << bits) + usize::from(value);
            }
            assert_eq!(covered, count);
        }
    }
    #[test]
    fn pending_runs_flush_before_nonzero_coefficients_and_zrl_is_preserved() {
        let mut blocks: Vec<_> = (0..3).map(|_| AlignedBlock::default()).collect();
        blocks[0].data[1] = 1;
        blocks[2].data[63] = -1;
        assert_eq!(
            collect(&blocks, 1, 64, 0),
            vec![
                AcEvent::Symbol {
                    symbol: 1,
                    value: 1,
                    bits: 1
                },
                AcEvent::Symbol {
                    symbol: 0x10,
                    value: 0,
                    bits: 1
                },
                AcEvent::Symbol {
                    symbol: 0xf0,
                    value: 0,
                    bits: 0
                },
                AcEvent::Symbol {
                    symbol: 0xf0,
                    value: 0,
                    bits: 0
                },
                AcEvent::Symbol {
                    symbol: 0xf0,
                    value: 0,
                    bits: 0
                },
                AcEvent::Symbol {
                    symbol: 0xe1,
                    value: 0,
                    bits: 1
                },
            ]
        );
    }
    #[test]
    fn runs_never_cross_restart_or_spectral_boundaries() {
        let mut blocks: Vec<_> = (0..3).map(|_| AlignedBlock::default()).collect();
        // Outside the current band; it must not stop an EOB run in this scan.
        blocks[1].data[63] = 9;
        assert_eq!(
            collect(&blocks, 1, 21, 1),
            vec![
                AcEvent::Symbol {
                    symbol: 0,
                    value: 0,
                    bits: 0
                },
                AcEvent::Restart(0),
                AcEvent::Symbol {
                    symbol: 0,
                    value: 0,
                    bits: 0
                },
                AcEvent::Restart(1),
                AcEvent::Symbol {
                    symbol: 0,
                    value: 0,
                    bits: 0
                },
            ]
        );
        assert_eq!(
            collect(&blocks, 1, 21, 0),
            vec![AcEvent::Symbol {
                symbol: 0x10,
                value: 1,
                bits: 1
            }]
        );
    }
}
