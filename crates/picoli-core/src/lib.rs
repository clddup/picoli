//! Portable image compression primitives and experimental content-aware search.
#[cfg(test)]
mod alpha_tests;
pub mod analysis;
#[cfg(test)]
mod bounded_score_tests;
mod jpeg;
mod jpeg_metadata;
#[cfg(test)]
mod jpeg_metadata_tests;
mod jpeg_rdo;
#[cfg(test)]
mod jpeg_rdo_legacy;
mod jpeg_search;
#[cfg(test)]
mod jpeg_search_tests;
mod jpeg_table;
pub use jpeg_metadata::JpegMetadata;
#[cfg(test)]
mod jpeg_integrity_tests;
pub mod optimize;
mod palette;
mod png_deflate;
mod png_rate;
mod predictive;
pub mod quality;
mod reorder;
pub mod resources;
use image::{
    ColorType, DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits, RgbImage,
    codecs::{
        jpeg::JpegEncoder,
        png::{CompressionType, FilterType, PngEncoder},
    },
};
use serde::Serialize;
use std::io::Cursor;
use thiserror::Error;

pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported baseline input: {0}")]
    Unsupported(&'static str),
    #[error("input exceeds resource limit: {0}")]
    Limit(&'static str),
    #[error("invalid argument: {0}")]
    Argument(&'static str),
    #[error("JPEG decoding failed: {0}")]
    InvalidJpeg(String),
    #[error("encoding failed: {0}")]
    Encoding(String),
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct Reference {
    pub pixels: RgbImage,
    /// None means fully opaque; Some preserves every original alpha byte.
    pub alpha: Option<image::GrayImage>,
    pub source_format: &'static str,
    /// Supported JPEG display metadata; may contain private EXIF information.
    /// Pixels stay in this profile's encoded RGB domain, not normalized sRGB.
    pub jpeg_metadata: JpegMetadata,
}

impl Reference {
    pub fn rgba(&self) -> Result<image::RgbaImage, Error> {
        if self
            .alpha
            .as_ref()
            .is_some_and(|a| a.dimensions() != self.pixels.dimensions())
        {
            return Err(Error::Argument("alpha dimensions differ from RGB"));
        }
        Ok(image::RgbaImage::from_fn(
            self.pixels.width(),
            self.pixels.height(),
            |x, y| {
                let p = self.pixels.get_pixel(x, y);
                image::Rgba([
                    p[0],
                    p[1],
                    p[2],
                    self.alpha.as_ref().map_or(255, |a| a.get_pixel(x, y)[0]),
                ])
            },
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    pub rgb_rmse_0_255: f64,
    pub gradient_mae_normalized: f64,
    pub pixels_exact: bool,
    pub production_quality_gate: &'static str,
}

/// Decode only the explicitly supported baseline subset. No hidden alpha flattening.
pub fn decode_baseline(bytes: &[u8]) -> Result<Reference, Error> {
    decode_with_budget(bytes, resources::DEFAULT_MEMORY_BYTES)
}

/// Estimate one compression worker's working set from validated headers only.
///
/// This does not decode pixels, build analysis, measure free RAM/browser RSS, or
/// validate the complete entropy stream. Compression still validates the input.
///
/// # Errors
/// Returns an error for unsupported/invalid headers, input over 32 MiB, or an
/// estimate above the maximum automatic budget of 1024 MiB.
pub fn compression_memory_plan(bytes: &[u8]) -> Result<resources::CompressionMemoryPlan, Error> {
    let (decoder, jpeg_metadata, source_format) =
        baseline_decoder(bytes, resources::MAX_AUTOMATIC_MEMORY_BYTES)
            .map_err(resources::automatic_budget_error)?;
    let (width, height) = decoder.dimensions();
    resources::CompressionMemoryPlan::with_metadata(
        width,
        height,
        source_format,
        bytes.len(),
        jpeg_metadata.len(),
    )
}

// Shared header and display-metadata validation; never reads decoded pixels.
fn baseline_decoder(
    bytes: &[u8],
    memory_budget_bytes: u64,
) -> Result<(impl ImageDecoder + '_, JpegMetadata, &'static str), Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit("encoded input > 32 MiB"));
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader
        .format()
        .ok_or(Error::Unsupported("unknown image format"))?;
    let source_format = match format {
        ImageFormat::Jpeg => {
            inspect_jpeg_header(bytes)?;
            "jpeg"
        }
        ImageFormat::Png => {
            inspect_png_chunks(bytes)?;
            "png"
        }
        _ => return Err(Error::Unsupported("only static JPEG/PNG are supported")),
    };
    let jpeg_metadata = if format == ImageFormat::Jpeg {
        JpegMetadata::extract(bytes, memory_budget_bytes)?
    } else {
        JpegMetadata::default()
    };
    let mut limits = Limits::default();
    let max_side = (memory_budget_bytes / 56).min(u64::from(u32::MAX)) as u32;
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(memory_budget_bytes);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    resources::Plan::with_metadata(
        width,
        height,
        bytes.len(),
        jpeg_metadata.len(),
        memory_budget_bytes,
        1,
    )?;
    let icc = decoder.icc_profile()?;
    let exif = decoder.exif_metadata()?;
    if icc.is_some() != jpeg_metadata.has_icc || exif.is_some() != jpeg_metadata.has_exif {
        return Err(Error::Unsupported(
            "incomplete or unsupported color/orientation metadata",
        ));
    }
    if let Some(icc) = icc {
        let profile = moxcms::ColorProfile::new_from_slice(&icc)
            .map_err(|_| Error::Unsupported("invalid or unsupported JPEG ICC profile"))?;
        if !matches!(profile.color_space, moxcms::DataColorSpace::Rgb)
            || decoder.color_type() != ColorType::Rgb8
        {
            return Err(Error::Unsupported("only RGB ICC on RGB JPEG is supported"));
        }
    }
    if !matches!(
        decoder.color_type(),
        ColorType::Rgb8 | ColorType::L8 | ColorType::Rgba8 | ColorType::La8
    ) {
        return Err(Error::Unsupported(
            "only 8-bit RGB/grayscale with optional alpha is supported",
        ));
    }
    Ok((decoder, jpeg_metadata, source_format))
}

pub fn decode_with_budget(bytes: &[u8], memory_budget_bytes: u64) -> Result<Reference, Error> {
    let (decoder, jpeg_metadata, source_format) = baseline_decoder(bytes, memory_budget_bytes)?;
    let (width, height) = decoder.dimensions();
    let max_side = (memory_budget_bytes / 56).min(u64::from(u32::MAX)) as u32;
    let mut alpha = None;
    let pixels = if source_format == "jpeg" {
        // The project-local decoder patch tracks actual entropy consumption.
        // Strict mode rejects synthetic zero-fill used as coefficient data;
        // valid lookahead and JPEG end padding remain supported.
        drop(decoder);
        if !bytes.ends_with(&[0xff, 0xd9]) {
            return Err(Error::Unsupported("JPEG must end with an EOI marker"));
        }
        let options = zune_core::options::DecoderOptions::default()
            .set_strict_mode(true)
            .set_max_width(max_side as usize)
            .set_max_height(max_side as usize)
            .jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
        let mut strict = zune_jpeg::JpegDecoder::new_with_options(
            zune_core::bytestream::ZCursor::new(bytes),
            options,
        );
        let raw = strict
            .decode()
            .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
        RgbImage::from_raw(width, height, raw)
            .ok_or(Error::Unsupported("unexpected JPEG RGB output size"))?
    } else {
        let decoded = DynamicImage::from_decoder(decoder)?;
        alpha = match &decoded {
            DynamicImage::ImageRgba8(im) if im.pixels().any(|p| p[3] != 255) => {
                Some(image::GrayImage::from_fn(width, height, |x, y| {
                    image::Luma([im.get_pixel(x, y)[3]])
                }))
            }
            DynamicImage::ImageLumaA8(im) if im.pixels().any(|p| p[1] != 255) => {
                Some(image::GrayImage::from_fn(width, height, |x, y| {
                    image::Luma([im.get_pixel(x, y)[1]])
                }))
            }
            _ => None,
        };
        // Removing a constant fully opaque alpha channel changes no pixel opacity.
        decoded.to_rgb8()
    };
    Ok(Reference {
        pixels,
        alpha,
        source_format,
        jpeg_metadata,
    })
}

// Inspect PNG headers before decoding. Structural validation and CRC checks remain
// the decoder's job; this pass rejects unsupported semantics, including APNG.
fn inspect_png_chunks(bytes: &[u8]) -> Result<(), Error> {
    let mut offset = 8usize;
    while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
        let len = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .map_err(|_| Error::Unsupported("truncated PNG chunk"))?,
        ) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|x| x.checked_add(len))
            .ok_or(Error::Limit("PNG chunk length overflow"))?;
        if end > bytes.len() {
            return Err(Error::Unsupported("truncated PNG chunk"));
        }
        match &bytes[offset + 4..offset + 8] {
            b"acTL" => return Err(Error::Unsupported("APNG is not supported")),
            b"eXIf" => return Err(Error::Unsupported("PNG EXIF needs orientation handling")),
            b"gAMA" | b"cHRM" | b"iCCP" | b"cICP" | b"mDCV" | b"cLLI" => {
                return Err(Error::Unsupported(
                    "PNG color metadata needs production color management",
                ));
            }
            b"IEND" => break,
            _ => {}
        }
        offset = end;
    }
    Ok(())
}

fn inspect_jpeg_header(bytes: &[u8]) -> Result<(), Error> {
    let mut pos = 2usize;
    while pos < bytes.len() {
        if bytes[pos] != 0xff {
            return Err(Error::Unsupported("malformed JPEG header"));
        }
        while pos < bytes.len() && bytes[pos] == 0xff {
            pos += 1;
        }
        let marker = *bytes
            .get(pos)
            .ok_or(Error::Unsupported("truncated JPEG marker"))?;
        pos += 1;
        if matches!(marker, 0xda | 0xd9) {
            break;
        }
        if matches!(marker, 0x01 | 0xd0..=0xd8) {
            continue;
        }
        let header = bytes
            .get(pos..pos + 2)
            .ok_or(Error::Unsupported("truncated JPEG segment"))?;
        let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
        if length < 2 {
            return Err(Error::Unsupported("invalid JPEG segment length"));
        }
        let payload = bytes
            .get(pos + 2..pos + length)
            .ok_or(Error::Unsupported("truncated JPEG payload"))?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
            && (payload.len() < 6 || payload[0] != 8 || !matches!(payload[5], 1 | 3))
        {
            return Err(Error::Unsupported(
                "only 8-bit grayscale/three-component JPEG supported",
            ));
        }
        pos += length;
    }
    Ok(())
}

pub fn encode_jpeg_baseline(reference: &Reference, quality: u8) -> Result<Vec<u8>, Error> {
    if reference.alpha.is_some() {
        return Err(Error::Unsupported(
            "JPEG cannot preserve transparency without an explicit matte",
        ));
    }
    if !(1..=100).contains(&quality) {
        return Err(Error::Argument("JPEG quality must be 1..=100"));
    }
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, quality).encode(
        reference.pixels.as_raw(),
        reference.pixels.width(),
        reference.pixels.height(),
        ExtendedColorType::Rgb8,
    )?;
    reference.jpeg_metadata.attach(encoded)
}

pub fn encode_png_lossless(reference: &Reference) -> Result<Vec<u8>, Error> {
    if !reference.jpeg_metadata.is_empty() {
        return Err(Error::Unsupported(
            "JPEG display metadata cannot be silently discarded during PNG conversion",
        ));
    }
    let mut encoded = Vec::new();
    if reference.alpha.is_some() {
        let rgba = reference.rgba()?;
        PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive)
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                ExtendedColorType::Rgba8,
            )?;
    } else {
        PngEncoder::new_with_quality(&mut encoded, CompressionType::Best, FilterType::Adaptive)
            .write_image(
                reference.pixels.as_raw(),
                reference.pixels.width(),
                reference.pixels.height(),
                ExtendedColorType::Rgb8,
            )?;
    }
    if let Some(exact) = palette::encode_exact(reference)?
        && exact.len() < encoded.len()
    {
        encoded = exact;
    }
    Ok(encoded)
}

/// Diagnostic only: not calibrated for visual acceptance or TinyPNG claims.
pub fn diagnostics(reference: &RgbImage, candidate: &RgbImage) -> Result<Diagnostics, Error> {
    if reference.dimensions() != candidate.dimensions() || reference.is_empty() {
        return Err(Error::Argument(
            "diagnostic images must have equal nonzero dimensions",
        ));
    }
    let mut square_error = 0.0;
    let mut gradient_error = 0.0;
    let width = reference.width() as usize;
    let height = reference.height() as usize;
    let a = reference.as_raw();
    let b = candidate.as_raw();
    let luminance = |data: &[u8], idx: usize| -> i32 {
        (77 * i32::from(data[idx]) + 150 * i32::from(data[idx + 1]) + 29 * i32::from(data[idx + 2]))
            >> 8
    };
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 3;
            for c in 0..3 {
                let delta = f64::from(a[idx + c]) - f64::from(b[idx + c]);
                square_error += delta * delta;
            }
            let center_a = luminance(a, idx);
            let center_b = luminance(b, idx);
            if x + 1 < width {
                let delta = (luminance(a, idx + 3) - center_a) - (luminance(b, idx + 3) - center_b);
                gradient_error += f64::from(delta.abs());
            }
            if y + 1 < height {
                let delta = (luminance(a, idx + width * 3) - center_a)
                    - (luminance(b, idx + width * 3) - center_b);
                gradient_error += f64::from(delta.abs());
            }
        }
    }
    let pixels = (width * height) as f64;
    Ok(Diagnostics {
        rgb_rmse_0_255: (square_error / (pixels * 3.0)).sqrt(),
        gradient_mae_normalized: gradient_error / (pixels * 2.0 * 255.0),
        pixels_exact: a == b,
        production_quality_gate: "not_evaluated",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference() -> Reference {
        Reference {
            pixels: RgbImage::from_fn(16, 12, |x, y| {
                image::Rgb([(x * 15) as u8, (y * 20) as u8, ((x + y) * 8) as u8])
            }),
            alpha: None,
            source_format: "generated",
            jpeg_metadata: Default::default(),
        }
    }
    fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = bytes.len();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&crc32fast::hash(&bytes[start..]).to_be_bytes());
    }

    // Deliberately invalid image data: preflight must not inflate or decode it.
    fn header_only_png(width: u32, height: u32, depth: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[depth, 2, 0, 0, 0]);
        png_chunk(&mut bytes, b"IHDR", &ihdr);
        png_chunk(&mut bytes, b"IDAT", b"not a deflate stream");
        png_chunk(&mut bytes, b"IEND", &[]);
        bytes
    }

    #[test]
    fn memory_preflight_reads_png_headers_not_pixels() {
        for (width, height, tier) in [(1, 1, 128), (4464, 3173, 1024)] {
            let bytes = header_only_png(width, height, 8);
            let plan = compression_memory_plan(&bytes).unwrap();
            assert_eq!(
                (plan.width, plan.height, plan.format),
                (width, height, "png")
            );
            assert_eq!(plan.memory_mib, tier);
            let expected = resources::Plan::new(
                width,
                height,
                bytes.len(),
                resources::MAX_AUTOMATIC_MEMORY_BYTES,
                1,
            )
            .unwrap();
            assert_eq!(
                plan.required_memory_bytes,
                expected.estimated_shared_bytes + expected.estimated_worker_bytes
            );
        }
        assert!(decode_baseline(&header_only_png(1, 1, 8)).is_err());
    }

    #[test]
    fn memory_preflight_keeps_header_and_input_restrictions() {
        assert!(compression_memory_plan(b"not an image").is_err());
        assert!(compression_memory_plan(b"GIF89a").is_err());
        assert!(matches!(
            compression_memory_plan(&vec![0; MAX_INPUT_BYTES + 1]),
            Err(Error::Limit(_))
        ));
        for (width, height, depth) in [(0, 1, 8), (1, 0, 8), (u32::MAX, u32::MAX, 8), (1, 1, 16)] {
            assert!(compression_memory_plan(&header_only_png(width, height, depth)).is_err());
        }
        let excess = compression_memory_plan(&header_only_png(5000, 5000, 8)).unwrap_err();
        assert!(matches!(excess, Error::Limit(_)));
        let message = excess.to_string();
        for expected in ["automatic", "budget", "1024"] {
            assert!(message.contains(expected), "{message}");
        }
        let mut bad_crc = header_only_png(1, 1, 8);
        bad_crc[29] ^= 1;
        assert!(compression_memory_plan(&bad_crc).is_err());
        assert!(compression_memory_plan(&bad_crc[..24]).is_err());
        for kind in [b"acTL", b"eXIf", b"iCCP"] {
            let mut bytes = header_only_png(1, 1, 8);
            let iend = bytes.split_off(bytes.len() - 12);
            png_chunk(&mut bytes, kind, &[]);
            bytes.extend_from_slice(&iend);
            assert!(matches!(
                compression_memory_plan(&bytes),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn memory_preflight_reads_jpeg_headers_not_entropy() {
        let mut bytes = encode_jpeg_baseline(&reference(), 60).unwrap();
        let sos = bytes.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let length = usize::from(u16::from_be_bytes([bytes[sos + 2], bytes[sos + 3]]));
        bytes.truncate(sos + 2 + length);
        bytes.extend_from_slice(&[0xff, 0xd9]);
        assert_eq!(compression_memory_plan(&bytes).unwrap().format, "jpeg");
        assert!(decode_baseline(&bytes).is_err());
        bytes.truncate(bytes.len() - 2);
        assert!(compression_memory_plan(&bytes).is_err());
    }

    #[test]
    fn memory_preflight_includes_retained_jpeg_metadata() {
        let plain = encode_jpeg_baseline(&reference(), 60).unwrap();
        let payload = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0";
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&plain[2..]);
        let plan = compression_memory_plan(&bytes).unwrap();
        assert_eq!((plan.width, plan.height, plan.format), (16, 12, "jpeg"));
        assert_eq!(plan.memory_mib, 128);
        let expected = resources::Plan::with_metadata(
            16,
            12,
            bytes.len(),
            payload.len() + 4,
            resources::MAX_AUTOMATIC_MEMORY_BYTES,
            1,
        )
        .unwrap();
        assert_eq!(
            plan.required_memory_bytes,
            expected.estimated_shared_bytes + expected.estimated_worker_bytes
        );
        // The retained EXIF parser must still reject rotation in preflight.
        bytes[6 + 24] = 6;
        assert!(matches!(
            compression_memory_plan(&bytes),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn lossless_png_roundtrip_is_pixel_exact() {
        let source = reference();
        let encoded = encode_png_lossless(&source).expect("encode PNG");
        let decoded = decode_baseline(&encoded).expect("decode PNG");
        let score = diagnostics(&source.pixels, &decoded.pixels).expect("metrics");
        assert!(score.pixels_exact);
        assert_eq!(score.rgb_rmse_0_255, 0.0);
        assert_eq!(score.production_quality_gate, "not_evaluated");
    }
    #[test]
    fn jpeg_roundtrip_preserves_dimensions_not_claimed_lossless() {
        let source = reference();
        let encoded = encode_jpeg_baseline(&source, 60).expect("encode JPEG");
        let decoded = decode_baseline(&encoded).expect("decode JPEG");
        assert_eq!(decoded.pixels.dimensions(), source.pixels.dimensions());
        assert!(
            !diagnostics(&source.pixels, &decoded.pixels)
                .expect("metrics")
                .pixels_exact
        );
        assert!(encode_jpeg_baseline(&source, 0).is_err());
    }
    #[test]
    fn preserves_alpha_instead_of_flattening() {
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&[255, 0, 0, 128], 1, 1, ExtendedColorType::Rgba8)
            .expect("RGBA fixture");
        let decoded = decode_baseline(&bytes).unwrap();
        assert_eq!(decoded.rgba().unwrap().into_raw(), vec![255, 0, 0, 128]);
        let png = encode_png_lossless(&decoded).unwrap();
        assert_eq!(
            decode_baseline(&png).unwrap().rgba().unwrap().into_raw(),
            vec![255, 0, 0, 128]
        );
        assert!(matches!(
            encode_jpeg_baseline(&decoded, 80),
            Err(Error::Unsupported(_))
        ));
    }
    #[test]
    fn accepts_fully_opaque_alpha_without_changing_colors_or_opacity() {
        let mut bytes = Vec::new();
        let rgba = [255, 0, 0, 255, 0, 100, 200, 255];
        PngEncoder::new(&mut bytes)
            .write_image(&rgba, 2, 1, ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = decode_baseline(&bytes).unwrap();
        assert!(decoded.alpha.is_none());
        assert_eq!(decoded.pixels.into_raw(), vec![255, 0, 0, 0, 100, 200]);
        let mut mixed = rgba;
        mixed[7] = 254;
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&mixed, 2, 1, ExtendedColorType::Rgba8)
            .unwrap();
        let decoded = decode_baseline(&bytes).unwrap();
        assert_eq!(decoded.alpha.unwrap().into_raw(), vec![255, 254]);
    }
    #[test]
    fn rejects_exif_after_png_image_data() {
        let mut encoded = encode_png_lossless(&reference()).expect("PNG fixture");
        let iend = encoded.split_off(encoded.len() - 12);
        let exif = b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        encoded.extend_from_slice(&(exif.len() as u32).to_be_bytes());
        let mut chunk = b"eXIf".to_vec();
        chunk.extend_from_slice(exif);
        encoded.extend_from_slice(&chunk);
        encoded.extend_from_slice(&crc32fast::hash(&chunk).to_be_bytes());
        encoded.extend_from_slice(&iend);
        assert!(matches!(
            decode_baseline(&encoded),
            Err(Error::Unsupported("PNG EXIF needs orientation handling"))
        ));
    }
    #[test]
    fn rejects_jpeg_without_final_eoi() {
        let encoded = encode_jpeg_baseline(&reference(), 60).expect("JPEG fixture");
        assert!(decode_baseline(&encoded[..encoded.len() - 2]).is_err());
    }
    #[test]
    fn rejects_truncated_jpeg_scan_even_with_replacement_eoi() {
        let source = Reference {
            pixels: RgbImage::from_fn(128, 128, |x, y| {
                image::Rgb([
                    ((x * 37 + y * 29) % 256) as u8,
                    ((x * 71 + y * 13) % 256) as u8,
                    ((x * 17 + y * 43) % 256) as u8,
                ])
            }),
            alpha: None,
            source_format: "generated",
            jpeg_metadata: Default::default(),
        };
        let encoded = encode_jpeg_baseline(&source, 80).expect("JPEG fixture");
        let sos = encoded
            .windows(2)
            .position(|w| w == [0xff, 0xda])
            .expect("SOS");
        let segment = usize::from(u16::from_be_bytes([encoded[sos + 2], encoded[sos + 3]]));
        let scan = sos + 2 + segment;
        let mut damaged = encoded[..scan + (encoded.len() - scan) / 4].to_vec();
        damaged.extend_from_slice(&[0xff, 0xd9]);
        assert!(decode_baseline(&damaged).is_err());
        assert!(decode_baseline(&encoded[..encoded.len() - 2]).is_err());
    }
    #[test]
    fn jpeg_backend_sampling_is_really_444() {
        let encoded = encode_jpeg_baseline(&reference(), 60).expect("JPEG fixture");
        let sof = encoded
            .windows(2)
            .position(|w| w == [0xff, 0xc0])
            .expect("SOF0");
        assert_eq!(
            [encoded[sof + 11], encoded[sof + 14], encoded[sof + 17]],
            [0x11; 3]
        );
    }
    #[test]
    fn rejects_bad_dimensions_and_input() {
        assert!(decode_baseline(b"not an image").is_err());
        assert!(diagnostics(&reference().pixels, &RgbImage::new(1, 1)).is_err());
        assert!(diagnostics(&RgbImage::new(0, 0), &RgbImage::new(0, 0)).is_err());
    }
}
