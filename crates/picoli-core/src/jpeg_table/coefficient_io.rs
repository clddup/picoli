//! Exact current-file capture and configured coefficient serialization.
use crate::{Error, Reference};
use image::RgbImage;
use jpeg_encoder::{Encoder, EncodingError, JpegColorType, QuantizedComponent, QuantizedImage};
use std::io::{self, Write};
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::{JpegDecoder, QuantizedCoefficients};

#[derive(Clone, Copy)]
pub struct Coding {
    pub scans: u8,
    pub eob_runs: bool,
    pub scan_huffman: bool,
}
pub struct Captured {
    pub frame: QuantizedCoefficients,
    pub pixels: RgbImage,
}
#[cfg(test)]
pub fn capture(bytes: &[u8], budget: u64) -> Result<Captured, Error> {
    let side = usize::try_from((budget / 56).min(65535))
        .map_err(|_| Error::Limit("capture side budget overflow"))?;
    capture_limited(bytes, side, side, budget / 8)
}
fn capture_limited(
    bytes: &[u8],
    width: usize,
    height: usize,
    payload: u64,
) -> Result<Captured, Error> {
    let options = DecoderOptions::default()
        .set_strict_mode(true)
        .set_max_width(width)
        .set_max_height(height)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.set_save_coefficients(
        usize::try_from(payload).map_err(|_| Error::Limit("capture payload overflow"))?,
    );
    let rgb = decoder
        .decode()
        .map_err(|e| Error::InvalidJpeg(e.to_string()))?;
    let frame = decoder.take_coefficients().ok_or(Error::Encoding(
        "incomplete current coefficient snapshot".into(),
    ))?;
    drop(decoder);
    let width = u32::try_from(frame.width).map_err(|_| Error::Limit("capture width overflow"))?;
    let height =
        u32::try_from(frame.height).map_err(|_| Error::Limit("capture height overflow"))?;
    let pixels = RgbImage::from_raw(width, height, rgb)
        .ok_or(Error::Encoding("invalid capture RGB size".into()))?;
    Ok(Captured { frame, pixels })
}

// Check the generated SOF before the decoder can allocate progressive planes or
// sampling-dependent scratch. The decoder remains responsible for JPEG validity.
fn generated_header(bytes: &[u8], width: u32, height: u32, sampling: u16) -> Result<(), Error> {
    let bad = || Error::Encoding("JPEG table generated header/layout mismatch".into());
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(bad());
    }
    let y_sampling = match sampling {
        444 => 0x11,
        422 => 0x21,
        420 => 0x22,
        _ => return Err(Error::Argument("JPEG table unsupported generated sampling")),
    };
    let mut pos = 2usize;
    let mut found = false;
    while pos < bytes.len() {
        if bytes[pos] != 0xff {
            return Err(bad());
        }
        while bytes.get(pos) == Some(&0xff) {
            pos += 1;
        }
        let marker = *bytes.get(pos).ok_or_else(bad)?;
        pos += 1;
        if marker == 0xda {
            return if found { Ok(()) } else { Err(bad()) };
        }
        let length_bytes = bytes.get(pos..pos + 2).ok_or_else(bad)?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(bad());
        }
        let end = pos.checked_add(length).ok_or_else(bad)?;
        let payload = bytes.get(pos + 2..end).ok_or_else(bad)?;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if found
                || marker != 0xc2
                || payload.len() != 15
                || payload[0] != 8
                || u32::from(u16::from_be_bytes([payload[1], payload[2]])) != height
                || u32::from(u16::from_be_bytes([payload[3], payload[4]])) != width
                || payload[5] != 3
                || payload[7] != y_sampling
                || payload[10] != 0x11
                || payload[13] != 0x11
            {
                return Err(bad());
            }
            found = true;
        }
        pos = end;
    }
    Err(bad())
}

pub fn capture_generated(
    bytes: &[u8],
    reference: &Reference,
    sampling: u16,
    payload: u64,
) -> Result<Captured, Error> {
    let (width, height) = reference.pixels.dimensions();
    generated_header(bytes, width, height, sampling)?;
    let captured = capture_limited(bytes, width as usize, height as usize, payload)?;
    let frame = &captured.frame;
    let (h, v) = super::resources::sampling_factors(sampling)
        .ok_or(Error::Argument("JPEG table unsupported generated sampling"))?;
    if frame.width != width as usize
        || frame.height != height as usize
        || frame.input_colorspace != ColorSpace::YCbCr
        || !frame.progressive
        || frame.components.len() != 3
        || frame.components[0].horizontal_sampling_factor as u64 != h
        || frame.components[0].vertical_sampling_factor as u64 != v
        || frame.components[1..]
            .iter()
            .any(|c| c.horizontal_sampling_factor != 1 || c.vertical_sampling_factor != 1)
        || frame
            .components
            .iter()
            .any(|c| c.quantization_table.iter().any(|q| !(1..=255).contains(q)))
        || frame.components[1].quantization_table != frame.components[2].quantization_table
    {
        return Err(Error::Encoding(
            "JPEG table generated coefficient layout mismatch".into(),
        ));
    }
    Ok(captured)
}

fn encode(
    reference: &Reference,
    frame: &QuantizedCoefficients,
    c: Coding,
    writer: impl Write,
) -> Result<(), EncodingError> {
    let bad = EncodingError::InvalidConfiguration;
    if !matches!(c.scans, 2 | 4) {
        return Err(bad("Coefficient scans must be2/4"));
    }
    if reference.alpha.is_some()
        || frame.width as u64 != u64::from(reference.pixels.width())
        || frame.height as u64 != u64::from(reference.pixels.height())
    {
        return Err(bad("Coefficient source geometry/opacity mismatch"));
    }
    let color_type = match (frame.input_colorspace, frame.components.len()) {
        (ColorSpace::Luma, 1) => JpegColorType::Luma,
        (ColorSpace::YCbCr, 3) => JpegColorType::Ycbcr,
        _ => return Err(bad("Coefficients require Luma or YCbCr")),
    };
    let components: Vec<_> = frame
        .components
        .iter()
        .map(|p| {
            Ok(QuantizedComponent {
                horizontal_sampling_factor: u8::try_from(p.horizontal_sampling_factor)
                    .map_err(|_| bad("Coefficient sampling overflows u8"))?,
                vertical_sampling_factor: u8::try_from(p.vertical_sampling_factor)
                    .map_err(|_| bad("Coefficient sampling overflows u8"))?,
                quantization_table: &p.quantization_table,
                block_width: p.block_width,
                block_height: p.block_height,
                coefficients: &p.coefficients,
            })
        })
        .collect::<Result<_, EncodingError>>()?;
    let mut encoder = Encoder::new(writer, 100);
    encoder.set_progressive_scans(c.scans);
    encoder.set_optimized_huffman_tables(true);
    encoder.set_progressive_eob_runs(c.eob_runs);
    encoder.set_progressive_scan_huffman(c.scan_huffman);
    encoder.encode_quantized(QuantizedImage {
        width: u16::try_from(frame.width).map_err(|_| bad("JPEG width exceeds65535"))?,
        height: u16::try_from(frame.height).map_err(|_| bad("JPEG height exceeds65535"))?,
        color_type,
        components: &components,
    })
}

#[cfg(test)]
pub fn serialize(
    reference: &Reference,
    frame: &QuantizedCoefficients,
    c: Coding,
) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    encode(reference, frame, c, &mut data).map_err(|e| Error::Encoding(e.to_string()))?;
    reference.jpeg_metadata.attach(data)
}

struct BoundedOutput {
    data: Vec<u8>,
    limit: usize,
    exceeded: bool,
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.data.len() {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "optional JPEG output ceiling",
            ));
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// None is only an expected output-size ceiling, not a swallowed codec error.
/// Reserving once prevents Vec growth from escaping the accounted output cap.
pub fn serialize_bounded(
    reference: &Reference,
    frame: &QuantizedCoefficients,
    coding: Coding,
    limit: usize,
) -> Result<Option<Vec<u8>>, Error> {
    let mut data = Vec::new();
    data.try_reserve_exact(limit)
        .map_err(|_| Error::Limit("JPEG output allocation failed"))?;
    let mut output = BoundedOutput {
        data,
        limit,
        exceeded: false,
    };
    match encode(reference, frame, coding, &mut output) {
        Ok(()) => reference.jpeg_metadata.attach(output.data).map(Some),
        Err(EncodingError::IoError(_)) if output.exceeded => Ok(None),
        Err(e) => Err(Error::Encoding(e.to_string())),
    }
}
