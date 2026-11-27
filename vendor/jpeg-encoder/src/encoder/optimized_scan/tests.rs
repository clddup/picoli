use super::*;
use crate::{Encoder, JpegColorType, QuantizedComponent, QuantizedImage};
use alloc::{vec, vec::Vec};
#[test]
fn exact_dp_matches_all_1024_partitions_and_fewer_scan_ties() {
    let mut seed = 17u64;
    for trial in 0..32 {
        let mut costs = [[0u64; 12]; 12];
        for (i, row) in costs.iter_mut().enumerate() {
            for value in row.iter_mut().skip(i + 1) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                *value = if trial == 0 { 0 } else { (seed >> 32) % 1000 };
            }
        }
        let (path, cost) = shortest(&costs).unwrap();
        let mut minimum = (u64::MAX, usize::MAX);
        for mask in 0..1024 {
            let mut previous = 0;
            let mut total = 0;
            let mut scans = 0;
            for (end, _) in CUTS.iter().enumerate().skip(1) {
                if end == 11 || mask & (1 << (end - 1)) != 0 {
                    total += costs[previous][end];
                    scans += 1;
                    previous = end;
                }
            }
            minimum = minimum.min((total, scans));
        }
        assert_eq!((cost, path.len()), minimum);
        let mut end = 1;
        for (a, b) in path {
            assert_eq!(usize::from(a), end);
            assert!(a < b);
            end = usize::from(b);
        }
        assert_eq!(end, 64);
    }
    let mut overflow = [[u64::MAX; 12]; 12];
    overflow[0][1] = 1;
    assert!(shortest(&overflow).is_err());
}
fn ac_bytes(bytes: &[u8]) -> u64 {
    assert_eq!(&bytes[..2], &[255, 216]);
    let mut p = 2;
    let mut dht = None;
    let mut sum = 0u64;
    while p < bytes.len() {
        assert_eq!(bytes[p], 255);
        let marker = bytes[p + 1];
        if marker == 217 {
            break;
        }
        let len = usize::from(u16::from_be_bytes([bytes[p + 2], bytes[p + 3]]));
        let end = p + 2 + len;
        if marker == 196 {
            dht = Some((p, end));
        }
        if marker != 218 {
            p = end;
            continue;
        }
        let n = usize::from(bytes[p + 4]);
        let spectral = bytes[p + 5 + 2 * n];
        let mut q = end;
        while q + 1 < bytes.len() {
            if bytes[q] != 255 {
                q += 1;
                continue;
            }
            let tag = bytes[q + 1];
            if tag == 0 || (208..=215).contains(&tag) {
                q += 2;
            } else {
                break;
            }
        }
        if spectral > 0 {
            let (a, b) = dht.take().unwrap();
            assert_eq!(b, p);
            sum += (q - a) as u64;
        } else {
            dht = None;
        }
        p = q;
    }
    sum
}
fn output(
    image: QuantizedImage<'_>,
    scans: u8,
    restart: u16,
    optimize: bool,
) -> (Vec<u8>, Option<ScanOptimizationReport>) {
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes, 100);
    e.set_progressive_scans(scans);
    e.set_optimized_huffman_tables(true);
    e.set_progressive_eob_runs(true);
    e.set_progressive_scan_huffman(true);
    if restart > 0 {
        e.set_restart_interval(restart);
    }
    let report = if optimize {
        Some(e.encode_quantized_optimized_scans(image).unwrap())
    } else {
        e.encode_quantized(image).unwrap();
        None
    };
    (bytes, report)
}
#[test]
fn predicted_real_ac_bytes_and_independent_pixels_match_on_sampling_restarts_odd_thin_gray() {
    let q = [9; 64];
    for (width, height) in [(17u16, 19u16), (1, 65), (65, 1)] {
        for (h, v, count) in [(1u8, 1u8, 1usize), (1, 1, 3), (2, 1, 3), (2, 2, 3)] {
            let mut planes = Vec::new();
            let mut geometry = Vec::new();
            for i in 0..count {
                let (ch, cv) = if i == 0 { (h, v) } else { (1, 1) };
                let cols = (usize::from(width) * usize::from(ch)).div_ceil(8 * usize::from(h));
                let rows = (usize::from(height) * usize::from(cv)).div_ceil(8 * usize::from(v));
                let stride = cols + 1;
                let mut data = vec![i16::MAX; stride * (rows + 1) * 64];
                for y in 0..rows {
                    for x in 0..cols {
                        let off = (y * stride + x) * 64;
                        for n in 0..64 {
                            data[off + n] = if n == 0 {
                                ((y * cols + x) % 11) as i16 * 5 - 25
                            } else if (n + x + y + i) % 7 == 0 {
                                (n % 5) as i16 - 2
                            } else {
                                0
                            };
                        }
                    }
                }
                geometry.push((ch, cv, stride, rows + 1));
                planes.push(data);
            }
            let cs: Vec<_> = planes
                .iter()
                .zip(&geometry)
                .map(|(data, &(ch, cv, cols, rows))| QuantizedComponent {
                    horizontal_sampling_factor: ch,
                    vertical_sampling_factor: cv,
                    quantization_table: &q,
                    block_width: cols,
                    block_height: rows,
                    coefficients: data,
                })
                .collect();
            let make = || QuantizedImage {
                width,
                height,
                color_type: if count == 1 {
                    JpegColorType::Luma
                } else {
                    JpegColorType::Ycbcr
                },
                components: &cs,
            };
            for restart in [0, 1, 3] {
                let (candidate, report) = output(make(), 2, restart, true);
                let report = report.unwrap();
                assert_eq!(report.interval_evaluations, 66 * count);
                assert_eq!(ac_bytes(&candidate), report.predicted_ac_bytes);
                let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&candidate));
                let pixels = decoder.decode().unwrap();
                assert_eq!(
                    (
                        decoder.info().unwrap().width,
                        decoder.info().unwrap().height
                    ),
                    (width, height)
                );
                for scans in [2, 4] {
                    let (baseline, _) = output(make(), scans, restart, false);
                    assert!(
                        candidate.len() <= baseline.len(),
                        "{width}x{height},sampling{h}x{v},scans{scans},restart{restart}"
                    );
                    assert_eq!(
                        candidate.len() as u64 - report.predicted_ac_bytes,
                        baseline.len() as u64 - ac_bytes(&baseline)
                    );
                    let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(baseline));
                    assert_eq!(d.decode().unwrap(), pixels);
                }
            }
        }
    }
}
#[test]
fn invalid_optimization_configuration_returns_before_any_header() {
    let q = [1; 64];
    let data = [0i16; 64];
    let cs = [QuantizedComponent {
        horizontal_sampling_factor: 1,
        vertical_sampling_factor: 1,
        quantization_table: &q,
        block_width: 1,
        block_height: 1,
        coefficients: &data,
    }];
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 100);
    encoder.set_progressive_scans(2);
    assert!(
        encoder
            .encode_quantized_optimized_scans(QuantizedImage {
                width: 8,
                height: 8,
                color_type: JpegColorType::Luma,
                components: &cs
            })
            .is_err()
    );
    assert!(bytes.is_empty());
}
