//! Exact candidate RGBA verification without allocating a second full RGB frame.
//! Current reference is still strictly decoded by the ordinary core boundary.
use crate::{Error, Reference};
use std::io::Cursor;
fn same_row(row: &[u8], color: png::ColorType, rgb: &[u8], alpha: Option<&[u8]>) -> bool {
    let width = rgb.len() / 3;
    if row.len() != width * color.samples() {
        return false;
    }
    if color == png::ColorType::Rgb {
        return row == rgb && alpha.is_none_or(|a| a.iter().all(|&a| a == 255));
    }
    match color {
        png::ColorType::Rgba => row
            .chunks_exact(4)
            .zip(rgb.chunks_exact(3))
            .enumerate()
            .all(|(i, (p, r))| p[..3] == *r && p[3] == alpha.map_or(255, |a| a[i])),
        png::ColorType::Grayscale => row
            .iter()
            .zip(rgb.chunks_exact(3))
            .enumerate()
            .all(|(i, (&p, r))| r == [p, p, p] && alpha.map_or(255, |a| a[i]) == 255),
        png::ColorType::GrayscaleAlpha => row
            .chunks_exact(2)
            .zip(rgb.chunks_exact(3))
            .enumerate()
            .all(|(i, (p, r))| r == [p[0], p[0], p[0]] && p[1] == alpha.map_or(255, |a| a[i])),
        _ => false,
    }
}
pub(super) fn verify(reference: &Reference, bytes: &[u8], budget: u64) -> Result<(), Error> {
    let (width, height) = reference.pixels.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    let auxiliary = pixels
        .checked_mul(8)
        .and_then(|n| n.checked_add(4 * 1024 * 1024))
        .unwrap_or(u64::MAX)
        .min(budget / 4);
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(auxiliary).unwrap_or(usize::MAX),
    });
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Encoding(e.to_string()))?;
    if (reader.info().width, reader.info().height) != (width, height) || reader.info().interlaced {
        return Err(Error::Encoding(
            "PNG streamed validation changed dimensions or interlace".into(),
        ));
    }
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight || color == png::ColorType::Indexed {
        return Err(Error::Encoding(
            "PNG streamed validation did not produce expanded8bit samples".into(),
        ));
    }
    let stride = width as usize * 3;
    let alpha = reference.alpha.as_ref().map(|a| a.as_raw());
    for y in 0..height as usize {
        let row = reader
            .next_row()
            .map_err(|e| Error::Encoding(e.to_string()))?
            .ok_or_else(|| Error::Encoding("PNG candidate ended before final row".into()))?;
        let expected = &reference.pixels.as_raw()[y * stride..(y + 1) * stride];
        let a = alpha.map(|a| &a[y * width as usize..(y + 1) * width as usize]);
        if !same_row(row.data(), color, expected, a) {
            return Err(Error::Encoding(
                "PNG streamed validation changed current RGBA or hidden RGB".into(),
            ));
        }
    }
    if reader
        .next_row()
        .map_err(|e| Error::Encoding(e.to_string()))?
        .is_some()
    {
        return Err(Error::Encoding(
            "PNG candidate has extra decoded rows".into(),
        ));
    }
    reader
        .finish()
        .map_err(|e| Error::Encoding(e.to_string()))?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn encode(
        color: png::ColorType,
        depth: png::BitDepth,
        width: u32,
        height: u32,
        raw: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            encoder.set_filter(png::Filter::Paeth);
            if color == png::ColorType::Indexed {
                let k = 1usize << (depth as usize);
                encoder.set_palette(
                    (0..k)
                        .flat_map(|i| [(i * 37) as u8, (i * 71) as u8, (i * 13) as u8])
                        .collect::<Vec<_>>(),
                );
                encoder.set_trns((0..k).map(|i| (i * 13) as u8).collect::<Vec<_>>());
            }
            encoder
                .write_header()
                .unwrap()
                .write_image_data(raw)
                .unwrap();
        }
        bytes
    }
    fn agrees(before: &Reference, candidate: &[u8]) {
        let old = crate::decode_with_budget(candidate, 128 * 1024 * 1024)
            .is_ok_and(|after| after.pixels == before.pixels && after.alpha == before.alpha);
        let new = verify(before, candidate, 128 * 1024 * 1024).is_ok();
        assert_eq!(old, new);
    }
    #[test]
    fn streaming_matches_full_rgb_oracle_across_depths_colors_and_geometry() {
        for (color, depth) in [
            (png::ColorType::Grayscale, png::BitDepth::One),
            (png::ColorType::Grayscale, png::BitDepth::Two),
            (png::ColorType::Grayscale, png::BitDepth::Four),
            (png::ColorType::Grayscale, png::BitDepth::Eight),
            (png::ColorType::Indexed, png::BitDepth::One),
            (png::ColorType::Indexed, png::BitDepth::Two),
            (png::ColorType::Indexed, png::BitDepth::Four),
            (png::ColorType::Indexed, png::BitDepth::Eight),
            (png::ColorType::Rgb, png::BitDepth::Eight),
            (png::ColorType::Rgba, png::BitDepth::Eight),
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight),
        ] {
            for (width, height) in [(17u32, 19u32), (1, 257), (257, 1)] {
                let stride = (width as usize * color.samples() * depth as usize).div_ceil(8);
                let raw: Vec<_> = (0..stride * height as usize)
                    .map(|i| ((i * 31) ^ (i / 13)) as u8)
                    .collect();
                let bytes = encode(color, depth, width, height, &raw);
                let before = crate::decode_with_budget(&bytes, 128 * 1024 * 1024).unwrap();
                verify(&before, &bytes, 128 * 1024 * 1024).unwrap();
                agrees(&before, &bytes);
                let mut changed = raw.clone();
                changed[0] ^= 0x80;
                let changed = encode(color, depth, width, height, &changed);
                agrees(&before, &changed);
            }
        }
    }
    #[test]
    fn alpha_hidden_rgb_and_truncated_tail_are_not_ignored() {
        let raw = [
            17u8, 23, 71, 0, 9, 10, 11, 127, 211, 101, 53, 255, 31, 32, 33, 1,
        ];
        let bytes = encode(png::ColorType::Rgba, png::BitDepth::Eight, 2, 2, &raw);
        let before = crate::decode_baseline(&bytes).unwrap();
        for index in [0, 1, 2, 3, 4, 7, 11, 15] {
            let mut changed = raw;
            changed[index] ^= 1;
            let changed = encode(png::ColorType::Rgba, png::BitDepth::Eight, 2, 2, &changed);
            assert!(verify(&before, &changed, 128 * 1024 * 1024).is_err());
            agrees(&before, &changed);
        }
        let truncated = &bytes[..bytes.len() - 5];
        assert!(verify(&before, truncated, 128 * 1024 * 1024).is_err());
        agrees(&before, truncated);
    }
}
