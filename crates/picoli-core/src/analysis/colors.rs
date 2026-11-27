//! Lossless storage for the integer analysis transform, not pixel quantization.
use image::RgbImage;

#[derive(Debug)]
pub(crate) struct ColorBuffer {
    values: Vec<[i16; 3]>,
}

fn packed(rgb: [u8; 3]) -> [i16; 3] {
    // For RGB8, Y is 0..=255 and Co/Cg are -255..=255. Every value fits
    // exactly; widen again before ANY subtraction, multiplication or blending.
    super::color(rgb).map(|value| value as i16)
}

impl ColorBuffer {
    pub(super) fn new(image: &RgbImage) -> Self {
        Self {
            values: image.pixels().map(|pixel| packed(pixel.0)).collect(),
        }
    }

    #[inline]
    pub(crate) fn at(&self, index: usize) -> [i32; 3] {
        self.values[index].map(i32::from)
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.values.capacity()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = [i32; 3]> + '_ {
        self.values.iter().map(|value| value.map(i32::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_transform_roundtrips_the_complete_rgb8_cube() {
        assert_eq!(std::mem::size_of::<[i16; 3]>(), 6);
        for r in 0..=255u16 {
            for g in 0..=255u16 {
                for b in 0..=255u16 {
                    let rgb = [r as u8, g as u8, b as u8];
                    assert_eq!(packed(rgb).map(i32::from), crate::analysis::color(rgb));
                }
            }
        }
    }

    #[test]
    fn rolling_weights_match_the_frozen_full_frame_algorithm() {
        for (width, height) in [
            (1, 1),
            (1, 19),
            (257, 1),
            (2, 2),
            (3, 17),
            (17, 3),
            (35, 33),
            (129, 257),
        ] {
            for pattern in 0..3 {
                let image = RgbImage::from_fn(width, height, |x, y| {
                    image::Rgb(match pattern {
                        0 => [(x * 7) as u8, (y * 11) as u8, (x + y * 13) as u8],
                        1 => {
                            let v = if (x / 3 + y / 5) % 2 == 0 { 0 } else { 255 };
                            [v, 255 - v, v]
                        }
                        _ => [
                            ((x * 71) ^ (y * 193)) as u8,
                            ((x * 199) ^ (y * 37)) as u8,
                            ((x * 11) ^ (y * 211)) as u8,
                        ],
                    })
                });
                let wide: Vec<_> = image
                    .pixels()
                    .map(|p| crate::analysis::color(p.0))
                    .collect();
                let (w, h) = (width as usize, height as usize);
                let mut raw = vec![1u8; wide.len()];
                for y in 0..h {
                    for x in 0..w {
                        let i = y * w + x;
                        let mut activity = 0;
                        for j in [y * w + (x + 1).min(w - 1), (y + 1).min(h - 1) * w + x] {
                            activity += (wide[i][0] - wide[j][0]).unsigned_abs();
                            activity += ((wide[i][1] - wide[j][1]).unsigned_abs()
                                + (wide[i][2] - wide[j][2]).unsigned_abs())
                                / 4;
                        }
                        raw[i] = 1 + (activity / 24).min(7) as u8;
                    }
                }
                let mut expected = raw.clone();
                for y in 0..h {
                    for x in 0..w {
                        let i = y * w + x;
                        for j in [
                            y * w + x.saturating_sub(1),
                            y * w + (x + 1).min(w - 1),
                            y.saturating_sub(1) * w + x,
                            (y + 1).min(h - 1) * w + x,
                        ] {
                            expected[i] = expected[i].max(raw[j]);
                        }
                    }
                }
                let compact = ColorBuffer::new(&image);
                assert_eq!(super::super::content_weights(&compact, w, h), expected);
                assert_eq!(
                    crate::analysis::Analysis::new(&image).unwrap().weights,
                    expected
                );
            }
        }
    }

    #[test]
    fn buffer_access_and_iteration_return_wide_exact_values() {
        let image = RgbImage::from_fn(257, 19, |x, y| {
            image::Rgb([(x * 13) as u8, (y * 29) as u8, (x + y * 71) as u8])
        });
        let buffer = ColorBuffer::new(&image);
        assert_eq!(buffer.len(), image.pixels().len());
        assert!(buffer.capacity() >= buffer.len());
        for (i, (pixel, value)) in image.pixels().zip(buffer.iter()).enumerate() {
            assert_eq!(value, crate::analysis::color(pixel.0));
            assert_eq!(buffer.at(i), value);
        }
    }
}
