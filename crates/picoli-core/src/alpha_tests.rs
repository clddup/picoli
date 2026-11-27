use crate::{
    Reference,
    analysis::Analysis,
    decode_baseline, palette,
    quality::{self, Strength},
};
use image::{Rgb, Rgba, RgbaImage};

fn source(image: &RgbaImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::NoCompression);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(image.as_raw()).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

fn encode_exact_mapping(image: &RgbaImage) -> Vec<u8> {
    let reference = decode_baseline(&source(image)).unwrap();
    let (data, _) = palette::encode_for_reference(
        &reference,
        &vec![0; image.width() as usize * image.height() as usize],
        &[[0, 0, 0]],
        &reference.pixels,
    )
    .unwrap();
    assert_eq!(decode_baseline(&data).unwrap().rgba().unwrap(), *image);
    data
}

fn over(image: &RgbaImage, background: u8) -> image::RgbImage {
    image::RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y);
        let a = u32::from(p[3]);
        Rgb(std::array::from_fn(|c| {
            ((u32::from(p[c]) * a + u32::from(background) * (255 - a) + 127) / 255) as u8
        }))
    })
}

#[test]
fn low_color_transparency_can_choose_indexed_trns() {
    let colors = [
        [230, 30, 40, 255],
        [20, 180, 80, 128],
        [50, 80, 240, 64],
        [17, 33, 99, 0],
    ];
    let image = RgbaImage::from_fn(64, 64, |x, y| {
        let mut n = (x + y * 64)
            .wrapping_mul(747796405)
            .wrapping_add(2891336453);
        n ^= n >> 16;
        Rgba(colors[(n % 4) as usize])
    });
    let bytes = encode_exact_mapping(&image);
    assert_eq!(
        bytes[25], 3,
        "indexed tRNS should be smaller for this low-color fixture"
    );
    assert!(bytes.windows(4).any(|w| w == b"tRNS"));
}

#[test]
fn more_than_256_rgba_pairs_fall_back_without_coarsening_alpha() {
    let image = RgbaImage::from_fn(32, 16, |x, y| {
        Rgba([
            (x * 7) as u8,
            (y * 13) as u8,
            190,
            ((x + y * 32) % 256) as u8,
        ])
    });
    let bytes = encode_exact_mapping(&image);
    assert_eq!(
        bytes[25], 6,
        "true RGBA fallback must retain all alpha/color combinations"
    );
}

#[test]
fn fully_transparent_images_preserve_hidden_rgb_and_do_not_panic() {
    let image = RgbaImage::from_fn(24, 24, |x, y| {
        Rgba([(x * 9) as u8, (y * 9) as u8, ((x + y) * 5) as u8, 0])
    });
    let original = source(&image);
    let result = quality::compress(&original, Strength::default()).unwrap();
    assert_eq!(
        decode_baseline(&result.data).unwrap().rgba().unwrap(),
        image
    );
    assert_eq!(result.score.loss, 0.0);
    assert!(result.bytes <= original.len());
}

#[test]
fn all_256_alpha_levels_are_exact_and_rgba_can_beat_indexed() {
    let image = RgbaImage::from_fn(64, 16, |x, y| {
        Rgba([30, 130, 220, ((x + y * 64) % 256) as u8])
    });
    let bytes = encode_exact_mapping(&image);
    assert_eq!(
        bytes[25], 6,
        "do not force indexed output when RGBA compresses smaller"
    );
    let original = source(&image);
    let selected = quality::compress(&original, Strength::default()).unwrap();
    assert_eq!(
        decode_baseline(&selected.data)
            .unwrap()
            .alpha
            .unwrap()
            .into_raw(),
        image.pixels().map(|p| p[3]).collect::<Vec<_>>()
    );
}

#[test]
fn encoded_alpha_and_hidden_rgb_are_verified_not_assumed() {
    let image = RgbaImage::from_fn(2, 1, |x, _| {
        if x == 0 {
            Rgba([11, 22, 33, 0])
        } else {
            Rgba([200, 50, 20, 128])
        }
    });
    let reference = decode_baseline(&source(&image)).unwrap();
    let mut bad_alpha = image.clone();
    bad_alpha.get_pixel_mut(1, 0)[3] = 127;
    assert!(
        quality::verify_transparency(
            &reference,
            &source(&bad_alpha),
            crate::resources::DEFAULT_MEMORY_BYTES
        )
        .is_err()
    );
    let mut bad_hidden = image.clone();
    bad_hidden.get_pixel_mut(0, 0)[0] = 99;
    assert!(
        quality::verify_transparency(
            &reference,
            &source(&bad_hidden),
            crate::resources::DEFAULT_MEMORY_BYTES
        )
        .is_err()
    );
}

#[test]
fn antialiased_edges_match_over_black_gray_and_white() {
    let image = RgbaImage::from_fn(48, 48, |x, y| {
        let d = (x as i32 - 24).abs().max((y as i32 - 24).abs());
        let a = ((20 - d) * 32).clamp(0, 255) as u8;
        if a == 0 {
            Rgba([11, 22, 33, 0])
        } else {
            Rgba([240, 50, 90, a])
        }
    });
    let original = source(&image);
    let reference = decode_baseline(&original).unwrap();
    let analysis =
        Analysis::for_reference(&reference, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
    let mapped = palette::train(&reference, &analysis, 4);
    let (indices, rgb) = palette::map(&reference, &analysis, &mapped, 64);
    let (bytes, _) = palette::encode_for_reference(&reference, &indices, &mapped, &rgb).unwrap();
    let Reference { pixels, alpha, .. } = decode_baseline(&bytes).unwrap();
    assert_eq!(alpha, reference.alpha);
    let decoded = Reference {
        pixels,
        alpha,
        source_format: "png",
        jpeg_metadata: Default::default(),
    }
    .rgba()
    .unwrap();
    for background in [0, 128, 255] {
        assert_eq!(over(&decoded, background), over(&image, background));
    }
    let default = quality::compress(&original, Strength::default()).unwrap();
    assert!(default.bytes < original.len());
    assert_eq!(
        decode_baseline(&default.data).unwrap().rgba().unwrap(),
        image
    );
}
