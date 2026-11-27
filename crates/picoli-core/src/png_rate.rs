//! Bounded greedy filtering priced by fast level1 DEFLATE prefixes, then
//! encoded once with level9. Model prices are NOT final PNG file sizes.
//! Existing miniz_oxide codec reused; no new entropy coder or pixel quantizer.
use miniz_oxide::deflate::core::{
    CompressorOxide, TDEFLFlush, TDEFLStatus, compress, create_comp_flags_from_zip_params,
};
use std::io::Cursor;
mod finish;
mod streamed;
pub(crate) use finish::refine;
pub(crate) fn verify_current(
    reference: &crate::Reference,
    bytes: &[u8],
    budget: u64,
) -> Result<(), crate::Error> {
    streamed::verify(reference, bytes, budget)
}

pub struct Trial {
    pub data: Vec<u8>,
    pub choices: Vec<u8>,
    pub prefix_sizes: Vec<usize>,
    pub final_zlib_bytes: usize,
}
fn compressor() -> CompressorOxide {
    CompressorOxide::new(create_comp_flags_from_zip_params(9, 15, 0))
}
fn feed(state: &mut CompressorOxide, input: &[u8], finish: bool) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut consumed = 0;
    loop {
        let mut buffer = [0u8; 16 * 1024];
        let (status, used, written) = compress(
            state,
            &input[consumed..],
            &mut buffer,
            if finish {
                TDEFLFlush::Finish
            } else {
                TDEFLFlush::None
            },
        );
        consumed += used;
        output.extend_from_slice(&buffer[..written]);
        match status {
            TDEFLStatus::BadParam | TDEFLStatus::PutBufFailed => {
                return Err(format!("compressor error: {status:?}"));
            }
            TDEFLStatus::Done => {
                if !finish || consumed != input.len() {
                    return Err("premature compressor termination".into());
                }
                break;
            }
            TDEFLStatus::Okay => {
                if consumed == input.len() && !finish {
                    break;
                }
                if used == 0 && written == 0 {
                    return Err("compressor made no progress".into());
                }
            }
        }
    }
    Ok(output)
}
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (da, db, dc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    if da <= db && da <= dc {
        a
    } else if db <= dc {
        b
    } else {
        c
    }
}
fn filtered_rows(
    raw: &[u8],
    original: &[u8],
    stride: usize,
    bpp: usize,
    start: usize,
    end: usize,
    choice: u8,
) -> Vec<u8> {
    if choice == 5 {
        return original[start * (stride + 1)..end * (stride + 1)].to_vec();
    }
    let mut result = Vec::with_capacity((end - start) * (stride + 1));
    for y in start..end {
        result.push(choice);
        for x in 0..stride {
            let here = raw[y * stride + x];
            let left = if x >= bpp {
                raw[y * stride + x - bpp]
            } else {
                0
            };
            let up = if y > 0 { raw[(y - 1) * stride + x] } else { 0 };
            let upper_left = if y > 0 && x >= bpp {
                raw[(y - 1) * stride + x - bpp]
            } else {
                0
            };
            let predictor = match choice {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                4 => paeth(left, up, upper_left),
                _ => unreachable!(),
            };
            result.push(here.wrapping_sub(predictor));
        }
    }
    result
}
fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) -> Result<(), String> {
    let length = u32::try_from(data.len()).map_err(|_| "chunk too large")?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    output.extend_from_slice(&crc.finalize().to_be_bytes());
    Ok(())
}
pub fn candidate(input: &[u8], budget: usize) -> Result<Option<Trial>, String> {
    candidate_impl(input, budget, true)
}
fn candidate_impl(
    input: &[u8],
    budget: usize,
    skip_duplicates: bool,
) -> Result<Option<Trial>, String> {
    if input.len() < 33 || &input[..8] != b"\x89PNG\r\n\x1a\n" {
        return Ok(None);
    }
    if input[28] != 0 {
        return Ok(None);
    } // Adam7 requires a different row model.
    let mut ranges = Vec::new();
    let mut idat = Vec::new();
    let mut offset = 8usize;
    while offset + 12 <= input.len() {
        let length = u32::from_be_bytes(input[offset..offset + 4].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(length)
            .and_then(|x| x.checked_add(12))
            .ok_or("PNG chunk overflow")?;
        if end > input.len() {
            return Err("truncated PNG chunk".into());
        }
        let kind: [u8; 4] = input[offset + 4..offset + 8].try_into().unwrap();
        if matches!(&kind, b"acTL" | b"fcTL" | b"fdAT") {
            return Ok(None);
        }
        let known = matches!(
            &kind,
            b"IHDR"
                | b"PLTE"
                | b"tRNS"
                | b"IDAT"
                | b"IEND"
                | b"pHYs"
                | b"gAMA"
                | b"cHRM"
                | b"sRGB"
                | b"iCCP"
                | b"sBIT"
                | b"bKGD"
                | b"hIST"
                | b"tIME"
                | b"tEXt"
                | b"zTXt"
                | b"iTXt"
                | b"eXIf"
        );
        if !known && kind[3] & 32 == 0 {
            return Ok(None);
        }
        if &kind == b"IDAT" {
            idat.extend_from_slice(&input[offset + 8..end - 4]);
        }
        ranges.push((kind, offset, end));
        offset = end;
    }
    if offset != input.len() {
        return Err("trailing or truncated PNG bytes".into());
    }
    let pixels = u64::from(u32::from_be_bytes(input[16..20].try_into().unwrap()))
        * u64::from(u32::from_be_bytes(input[20..24].try_into().unwrap()));
    let auxiliary = pixels
        .checked_mul(8)
        .and_then(|n| n.checked_add(4 * 1024 * 1024))
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX)
        .min(budget / 4);
    let mut decoder = png::Decoder::new(Cursor::new(input));
    decoder.set_limits(png::Limits { bytes: auxiliary });
    let mut reader = match decoder.read_info() {
        Ok(reader) => reader,
        Err(png::DecodingError::LimitsExceeded) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let size = reader.output_buffer_size().ok_or("PNG frame overflow")?;
    if size > budget / 4 {
        return Err("research frame capture exceeds explicit memory budget".into());
    }
    let mut raw = vec![0u8; size];
    let info = match reader.next_frame(&mut raw) {
        Ok(info) => info,
        Err(png::DecodingError::LimitsExceeded) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    raw.truncate(info.buffer_size());
    drop(reader); // Release decoder row/metadata buffers before branching.
    let stride = info.line_size;
    let height = info.height as usize;
    let bpp = (info.color_type.samples() * info.bit_depth as usize).div_ceil(8);
    let expected = (stride + 1)
        .checked_mul(height)
        .ok_or("PNG filtered size overflow")?;
    let original = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, expected)
        .map_err(|e| format!("source zlib invalid: {e:?}"))?;
    if original.len() != expected || raw.len() != stride * height {
        return Err("PNG scanline size mismatch".into());
    }
    if original.chunks_exact(stride + 1).any(|row| row[0] > 4) {
        return Err("invalid original PNG row filter".into());
    }
    let group_rows = (65536 / (stride + 1)).max(1);
    let mut state = CompressorOxide::new(create_comp_flags_from_zip_params(1, 15, 0));
    let mut emitted_model_bytes = 0usize;
    let mut selected_rows = Vec::with_capacity(expected);
    let mut choices = Vec::new();
    let mut prefix_sizes = Vec::new();
    for start in (0..height).step_by(group_rows) {
        let end = (start + group_rows).min(height);
        let mut best: Option<(usize, u8, CompressorOxide, Vec<u8>)> = None;
        // Original row stream wins exact price ties. Its previous prefix may
        // differ, so retaining this branch alone does not guarantee final gain.
        for choice in [5, 0, 1, 2, 3, 4] {
            let rows = filtered_rows(&raw, &original, stride, bpp, start, end, choice);
            // Original choice5 was already priced from exactly this state and
            // wins all equal-price ties. Compare complete bytes, not just filter
            // IDs or pixel values: packed-row padding must also be identical.
            if skip_duplicates
                && choice != 5
                && rows.as_slice() == &original[start * (stride + 1)..end * (stride + 1)]
            {
                continue;
            }
            let mut branch = state.clone();
            let extra = feed(&mut branch, &rows, false)?;
            let tail = feed(&mut branch.clone(), &[], true)?;
            let price = emitted_model_bytes + extra.len() + tail.len();
            if best.as_ref().is_none_or(|x| price < x.0) {
                best = Some((price, choice, branch, extra));
            }
        }
        let (price, choice, branch, extra) = best.ok_or("no filter branch")?;
        state = branch;
        emitted_model_bytes += extra.len();
        selected_rows.extend_from_slice(&filtered_rows(
            &raw, &original, stride, bpp, start, end, choice,
        ));
        choices.push(choice);
        prefix_sizes.push(price);
    }
    if selected_rows == original {
        return Ok(None); // No representation change: keep current complete file.
    }
    drop(state);
    drop(raw);
    drop(original);
    drop(idat);
    // Final compressor stays at the original high-compression level. Never
    // report the fast model's prefix price as this stream's actual byte count.
    let mut final_state = compressor();
    let encoded = feed(&mut final_state, &selected_rows, true)?;
    drop(final_state);
    drop(selected_rows);
    let decoded = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&encoded, expected)
        .map_err(|e| format!("candidate zlib invalid: {e:?}"))?;
    if decoded.len() != expected {
        return Err("candidate filtered size mismatch".into());
    }
    drop(decoded);
    let mut output = input[..8].to_vec();
    let mut wrote = false;
    for (kind, start, end) in ranges {
        if &kind == b"IDAT" {
            if !wrote {
                for part in encoded.chunks(64 * 1024) {
                    chunk(&mut output, b"IDAT", part)?;
                }
                wrote = true;
            }
        } else {
            output.extend_from_slice(&input[start..end]);
        }
    }
    if !wrote {
        return Err("no PNG image data".into());
    }
    Ok(Some(Trial {
        data: output,
        choices,
        prefix_sizes,
        final_zlib_bytes: encoded.len(),
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_skip_matches_full_trial_oracle_including_paths_and_padding() {
        for (color, depth, width, height) in [
            (png::ColorType::Indexed, png::BitDepth::One, 17u32, 257u32),
            (png::ColorType::Rgb, png::BitDepth::Eight, 513, 129),
            (png::ColorType::Rgba, png::BitDepth::Eight, 129, 257),
        ] {
            let stride = (width as usize * color.samples() * depth as usize).div_ceil(8);
            let raw: Vec<_> = (0..stride * height as usize)
                .map(|i| ((i * 31) ^ (i / 13)) as u8)
                .collect();
            for filter in [
                png::Filter::NoFilter,
                png::Filter::Sub,
                png::Filter::Up,
                png::Filter::Avg,
                png::Filter::Paeth,
                png::Filter::Adaptive,
            ] {
                let mut input = Vec::new();
                {
                    let mut encoder = png::Encoder::new(&mut input, width, height);
                    encoder.set_color(color);
                    encoder.set_depth(depth);
                    encoder.set_filter(filter);
                    encoder.set_deflate_compression(png::DeflateCompression::Level(9));
                    if color == png::ColorType::Indexed {
                        encoder.set_palette(vec![17, 23, 71, 211, 101, 53]);
                        encoder.set_trns(vec![0, 127]);
                    }
                    encoder
                        .write_header()
                        .unwrap()
                        .write_image_data(&raw)
                        .unwrap();
                }
                let old = candidate_impl(&input, 128 * 1024 * 1024, false).unwrap();
                let new = candidate_impl(&input, 128 * 1024 * 1024, true).unwrap();
                assert_eq!(
                    old.is_some(),
                    new.is_some(),
                    "no-op choice differs from full trial oracle"
                );
                if let (Some(old), Some(new)) = (old, new) {
                    assert_eq!(old.data, new.data);
                    assert_eq!(old.choices, new.choices);
                    assert_eq!(old.prefix_sizes, new.prefix_sizes);
                    assert_eq!(old.final_zlib_bytes, new.final_zlib_bytes);
                }
            }
        }
    }
    #[test]
    fn final_stream_uses_level9_not_the_fast_pricing_stream() {
        let (width, height) = (513u32, 129u32);
        let raw: Vec<_> = (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| [x as u8, y as u8, (x + y) as u8]))
            .collect();
        let mut input = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut input, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_filter(png::Filter::NoFilter);
            encoder.set_deflate_compression(png::DeflateCompression::Level(9));
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&raw)
                .unwrap();
        }
        let trial = candidate(&input, 128 * 1024 * 1024)
            .unwrap()
            .expect("structured fixture must choose changed filter rows");
        let mut encoded = Vec::new();
        let mut offset = 8;
        while offset + 12 <= trial.data.len() {
            let length =
                u32::from_be_bytes(trial.data[offset..offset + 4].try_into().unwrap()) as usize;
            if &trial.data[offset + 4..offset + 8] == b"IDAT" {
                encoded.extend_from_slice(&trial.data[offset + 8..offset + 8 + length]);
            }
            offset += length + 12;
        }
        assert_eq!(encoded.len(), trial.final_zlib_bytes);
        let filtered = miniz_oxide::inflate::decompress_to_vec_zlib(&encoded).unwrap();
        assert_eq!(
            encoded,
            miniz_oxide::deflate::compress_to_vec_zlib(&filtered, 9)
        );
        assert_eq!(
            crate::decode_baseline(&input).unwrap().rgba().unwrap(),
            crate::decode_baseline(&trial.data).unwrap().rgba().unwrap()
        );
    }
    #[test]
    fn snapshot_branches_have_independent_dictionary_and_checksums() {
        let prefix: Vec<_> = (0..71003).map(|i| ((i * 37) ^ (i / 31)) as u8).collect();
        let suffix: Vec<_> = (0..37019).map(|i| ((i * 71) ^ (i / 7)) as u8).collect();
        let mut state = compressor();
        let before = feed(&mut state, &prefix, false).unwrap();
        let original_adler = state.adler32();
        let mut branch = state.clone();
        let mut bytes = before.clone();
        bytes.extend(feed(&mut branch, &suffix, false).unwrap());
        bytes.extend(feed(&mut branch, &[], true).unwrap());
        let mut expected = prefix.clone();
        expected.extend_from_slice(&suffix);
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec_zlib(&bytes).unwrap(),
            expected
        );
        assert_eq!(state.adler32(), original_adler);
        let mut original = before;
        original.extend(feed(&mut state, &[], true).unwrap());
        assert_eq!(
            miniz_oxide::inflate::decompress_to_vec_zlib(&original).unwrap(),
            prefix
        );
    }
    #[test]
    fn all_fixed_predictors_preserve_packed_and_multichannel_pixels() {
        for (color, depth, width, height) in [
            (png::ColorType::Indexed, png::BitDepth::One, 17u32, 19u32),
            (png::ColorType::Grayscale, png::BitDepth::Eight, 1, 257),
            (png::ColorType::Rgb, png::BitDepth::Eight, 257, 1),
            (png::ColorType::Rgba, png::BitDepth::Eight, 41, 53),
        ] {
            let bpp = (color.samples() * depth as usize).div_ceil(8);
            let stride = (width as usize * color.samples() * depth as usize).div_ceil(8);
            let raw: Vec<_> = (0..stride * height as usize)
                .map(|i| ((i * 31) ^ (i / 13)) as u8)
                .collect();
            for choice in 0..5 {
                let rows = filtered_rows(&raw, &[], stride, bpp, 0, height as usize, choice);
                let packed = miniz_oxide::deflate::compress_to_vec_zlib(&rows, 9);
                let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
                let mut header = Vec::new();
                header.extend_from_slice(&width.to_be_bytes());
                header.extend_from_slice(&height.to_be_bytes());
                header.extend_from_slice(&[depth as u8, color as u8, 0, 0, 0]);
                chunk(&mut png, b"IHDR", &header).unwrap();
                if color == png::ColorType::Indexed {
                    chunk(&mut png, b"PLTE", &[17, 23, 71, 211, 101, 53]).unwrap();
                    chunk(&mut png, b"tRNS", &[0, 127]).unwrap();
                }
                chunk(&mut png, b"IDAT", &packed).unwrap();
                chunk(&mut png, b"IEND", &[]).unwrap();
                let mut reader = png::Decoder::new(Cursor::new(&png)).read_info().unwrap();
                let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
                let info = reader.next_frame(&mut decoded).unwrap();
                decoded.truncate(info.buffer_size());
                assert_eq!(decoded, raw);
                let out = candidate(&png, 1024 * 1024 * 1024).unwrap();
                let actual = out
                    .as_ref()
                    .map_or(png.as_slice(), |out| out.data.as_slice());
                assert_eq!(
                    crate::decode_with_budget(&png, 1024 * 1024 * 1024)
                        .unwrap()
                        .rgba()
                        .unwrap(),
                    crate::decode_with_budget(actual, 1024 * 1024 * 1024)
                        .unwrap()
                        .rgba()
                        .unwrap()
                );
            }
        }
    }
}
