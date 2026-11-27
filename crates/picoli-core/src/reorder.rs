//! Lossless palette-index ordering, evaluated only after image quantization.
//! A permutation changes PNG filter residuals, never decoded RGB or alpha.
use crate::{Error, palette};
use std::io::Cursor;

struct Indexed {
    indices: Vec<u8>,
    colors: Vec<[u8; 3]>,
    alpha: Vec<u8>,
    width: u32,
    height: u32,
}

fn decode(bytes: &[u8]) -> Result<Option<Indexed>, Error> {
    if bytes.len() < 26 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || bytes[25] != 3 {
        return Ok(None);
    }
    let mut reader = png::Decoder::new(Cursor::new(bytes))
        .read_info()
        .map_err(|e| Error::Encoding(e.to_string()))?;
    let colors: Vec<_> = reader
        .info()
        .palette
        .as_ref()
        .ok_or_else(|| Error::Encoding("indexed output missing palette".into()))?
        .chunks_exact(3)
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    let mut alpha = reader
        .info()
        .trns
        .as_ref()
        .map_or_else(Vec::new, |a| a.to_vec());
    alpha.resize(colors.len(), 255);
    let length = reader
        .output_buffer_size()
        .ok_or_else(|| Error::Encoding("indexed output exceeds addressable memory".into()))?;
    let mut raw = vec![0; length];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|e| Error::Encoding(e.to_string()))?;
    if info.color_type != png::ColorType::Indexed {
        return Err(Error::Encoding(
            "unexpected transformed palette output".into(),
        ));
    }
    let bits = match info.bit_depth {
        png::BitDepth::One => 1usize,
        png::BitDepth::Two => 2,
        png::BitDepth::Four => 4,
        png::BitDepth::Eight => 8,
        _ => return Err(Error::Encoding("invalid indexed bit depth".into())),
    };
    let width = info.width as usize;
    let stride = (width * bits).div_ceil(8);
    let mut indices = Vec::with_capacity(width * info.height as usize);
    for y in 0..info.height as usize {
        for x in 0..width {
            let index = (raw[y * stride + x * bits / 8] >> (8 - bits - x * bits % 8))
                & ((1u16 << bits) - 1) as u8;
            if usize::from(index) >= colors.len() {
                return Err(Error::Encoding(
                    "palette output has an invalid index".into(),
                ));
            }
            indices.push(index);
        }
    }
    Ok(Some(Indexed {
        indices,
        colors,
        alpha,
        width: info.width,
        height: info.height,
    }))
}

fn encode_order(image: &Indexed, order: &[usize]) -> Result<(Vec<u8>, &'static str), Error> {
    let mut inverse = vec![0u8; image.colors.len()];
    let mut colors = Vec::with_capacity(order.len());
    let mut alpha = Vec::with_capacity(order.len());
    for (new, &old) in order.iter().enumerate() {
        inverse[old] = new as u8;
        colors.push(image.colors[old]);
        alpha.push(image.alpha[old]);
    }
    while alpha.last() == Some(&255) {
        alpha.pop();
    }
    let indices: Vec<_> = image
        .indices
        .iter()
        .map(|&i| inverse[usize::from(i)])
        .collect();
    palette::encode_indexed(&indices, &colors, &alpha, image.width, image.height)
}

pub(crate) struct Reordered {
    pub data: Vec<u8>,
    pub filter: &'static str,
    pub order: &'static str,
}

pub(crate) fn candidate(bytes: &[u8]) -> Result<Option<Reordered>, Error> {
    let Some(image) = decode(bytes)? else {
        return Ok(None);
    };
    let mut luma: Vec<_> = (0..image.colors.len()).collect();
    luma.sort_by_key(|&i| {
        let [r, g, b] = image.colors[i].map(u32::from);
        (
            77 * r + 150 * g + 29 * b,
            image.colors[i],
            image.alpha[i],
            i,
        )
    });
    if luma.iter().enumerate().all(|(new, &old)| new == old) {
        return Ok(None);
    }
    let (data, filter) = encode_order(&image, &luma)?;
    #[cfg(debug_assertions)]
    {
        let decoded = decode(&data)?.ok_or_else(|| {
            Error::Encoding("reordered output lost indexed representation".into())
        })?;
        if (decoded.width, decoded.height) != (image.width, image.height)
            || image
                .indices
                .iter()
                .zip(&decoded.indices)
                .any(|(&before, &after)| {
                    image.colors[usize::from(before)] != decoded.colors[usize::from(after)]
                        || image.alpha[usize::from(before)] != decoded.alpha[usize::from(after)]
                })
        {
            return Err(Error::Encoding(
                "palette permutation changed decoded pixels".into(),
            ));
        }
    }
    Ok(Some(Reordered {
        data,
        filter,
        order: "luminance",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_pass_preserves_density_pixels_and_is_idempotent() {
        let mut source = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut source, 37, 23);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(&[250, 240, 230, 10, 20, 30, 100, 150, 200][..]);
            encoder.set_trns(&[0, 128, 255][..]);
            encoder.set_pixel_dims(Some(png::PixelDimensions {
                xppu: 3780,
                yppu: 3780,
                unit: png::Unit::Meter,
            }));
            encoder.set_filter(png::Filter::Up);
            encoder.set_compression(png::Compression::NoCompression);
            let mut writer = encoder.write_header().unwrap();
            let indices: Vec<_> = (0..37 * 23).map(|i| ((i + i / 37) % 3) as u8).collect();
            writer.write_image_data(&indices).unwrap();
            writer.finish().unwrap();
        }
        let reference = crate::decode_baseline(&source).unwrap();
        let analysis = crate::analysis::Analysis::for_reference(
            &reference,
            crate::resources::DEFAULT_MEMORY_BYTES,
        )
        .unwrap();
        let mut candidates = vec![crate::optimize::Candidate {
            id: "png-order-fixture".into(),
            parameters: crate::optimize::Parameters::Png {
                colors: 3,
                coherence: 0,
                filter: "up",
                mapping: "exact_cell_bounds",
                palette_order: "as_generated",
            },
            bytes: source.len(),
            data: source.clone(),
            score: analysis.score(&reference.pixels).unwrap(),
        }];
        crate::quality::refine_png_candidates(
            &source,
            &mut candidates,
            crate::quality::Strength::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        let selected =
            crate::quality::best(candidates.iter(), crate::quality::Strength::default()).unwrap();
        assert!(selected.bytes < source.len());
        assert_eq!(
            crate::decode_baseline(&selected.data)
                .unwrap()
                .rgba()
                .unwrap(),
            reference.rgba().unwrap()
        );
        let reader = png::Decoder::new(Cursor::new(&selected.data))
            .read_info()
            .unwrap();
        assert_eq!(reader.info().pixel_dims.unwrap().xppu, 3780);
        let once = selected.data.clone();
        crate::quality::refine_png_candidates(
            &source,
            &mut candidates,
            crate::quality::Strength::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            crate::quality::best(candidates.iter(), crate::quality::Strength::default())
                .unwrap()
                .data,
            once
        );
    }

    #[test]
    fn permutations_preserve_all_index_depths_alpha_and_hidden_rgb_at_odd_widths() {
        for count in [2, 3, 16, 256] {
            let colors: Vec<_> = (0..count)
                .map(|i| [(i * 71) as u8, (i * 37) as u8, (i * 19) as u8])
                .collect();
            let alpha: Vec<_> = (0..count)
                .map(|i| {
                    if i % 3 == 0 {
                        0
                    } else if i % 3 == 1 {
                        128
                    } else {
                        255
                    }
                })
                .collect();
            let indices: Vec<_> = (0..37 * 23)
                .map(|i| ((i * 13 + i / 37) % count) as u8)
                .collect();
            let (original, _) = palette::encode_indexed(&indices, &colors, &alpha, 37, 23).unwrap();
            let image = decode(&original).unwrap().unwrap();
            assert_eq!(image.indices, indices);
            let mut order: Vec<_> = (0..count).collect();
            order.reverse();
            let (reordered, _) = encode_order(&image, &order).unwrap();
            assert_eq!(
                crate::decode_baseline(&original).unwrap().rgba().unwrap(),
                crate::decode_baseline(&reordered).unwrap().rgba().unwrap()
            );
        }
    }
}
