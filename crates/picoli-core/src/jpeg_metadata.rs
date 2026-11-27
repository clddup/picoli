//! Preserve supported JPEG display metadata without changing its color domain.
//! These bytes are intentionally opaque: EXIF can contain private information.
use crate::Error;

#[derive(Clone, Default)]
pub struct JpegMetadata {
    data: Vec<u8>,
    pub(crate) has_icc: bool,
    pub(crate) has_exif: bool,
    has_jfif: bool,
}

impl std::fmt::Debug for JpegMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JpegMetadata")
            .field("bytes", &self.len())
            .field("has_icc", &self.has_icc)
            .field("has_exif", &self.has_exif)
            .finish()
    }
}

fn display_segment(marker: u8, payload: &[u8]) -> bool {
    (marker == 0xe1 && payload.starts_with(b"Exif\0\0"))
        || (marker == 0xe2 && payload.starts_with(b"ICC_PROFILE\0"))
        || (marker == 0xe0 && payload.starts_with(b"JFIF\0"))
}

// Returns the marker and the exclusive segment end. Entropy data is handled by
// extract(), not this header helper. All lengths are checked before slicing.
fn segment(bytes: &[u8], start: usize) -> Result<(u8, usize, &[u8]), Error> {
    let mut pos = start;
    if bytes.get(pos) != Some(&0xff) {
        return Err(Error::Unsupported("malformed JPEG marker"));
    }
    while bytes.get(pos) == Some(&0xff) {
        pos += 1;
    }
    let marker = *bytes
        .get(pos)
        .ok_or(Error::Unsupported("truncated JPEG marker"))?;
    pos += 1;
    if marker == 0 || marker == 0xd8 {
        return Err(Error::Unsupported("unexpected JPEG marker"));
    }
    if matches!(marker, 0x01 | 0xd0..=0xd7 | 0xd9) {
        return Ok((marker, pos, &[]));
    }
    let length = bytes
        .get(pos..pos + 2)
        .ok_or(Error::Unsupported("truncated JPEG length"))?;
    let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
    if length < 2 {
        return Err(Error::Unsupported("invalid JPEG length"));
    }
    let end = pos
        .checked_add(length)
        .ok_or(Error::Limit("JPEG length overflow"))?;
    let payload = bytes
        .get(pos + 2..end)
        .ok_or(Error::Unsupported("truncated JPEG segment"))?;
    Ok((marker, end, payload))
}

fn validate_exif(payload: &[u8]) -> Result<(), Error> {
    let tiff = payload.get(6..).ok_or(Error::Unsupported("invalid EXIF"))?;
    let little = match tiff.get(..4) {
        Some(b"II\x2a\0") => true,
        Some(b"MM\0\x2a") => false,
        _ => return Err(Error::Unsupported("invalid EXIF TIFF header")),
    };
    let word = |p: &[u8]| {
        if little {
            u16::from_le_bytes([p[0], p[1]])
        } else {
            u16::from_be_bytes([p[0], p[1]])
        }
    };
    let dword = |p: &[u8]| {
        if little {
            u32::from_le_bytes([p[0], p[1], p[2], p[3]])
        } else {
            u32::from_be_bytes([p[0], p[1], p[2], p[3]])
        }
    };
    let offset = tiff
        .get(4..8)
        .ok_or(Error::Unsupported("truncated EXIF offset"))?;
    let offset = dword(offset) as usize;
    let directory = tiff
        .get(offset..)
        .filter(|_| offset >= 8)
        .ok_or(Error::Unsupported("invalid EXIF directory"))?;
    let count = word(
        directory
            .get(..2)
            .ok_or(Error::Unsupported("truncated EXIF directory"))?,
    ) as usize;
    let entries = directory
        .get(2..2 + count * 12 + 4)
        .ok_or(Error::Unsupported("truncated EXIF entries"))?;
    let mut seen = false;
    for entry in entries[..count * 12].chunks_exact(12) {
        if word(&entry[..2]) == 0x112 {
            if seen
                || word(&entry[2..4]) != 3
                || dword(&entry[4..8]) != 1
                || word(&entry[8..10]) != 1
            {
                return Err(Error::Unsupported(
                    "JPEG EXIF rotation/reflection or invalid orientation is not supported",
                ));
            }
            seen = true;
        }
    }
    Ok(())
}

impl JpegMetadata {
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn extract(bytes: &[u8], memory_budget: u64) -> Result<Self, Error> {
        if !bytes.starts_with(&[0xff, 0xd8]) {
            return Err(Error::Unsupported("JPEG missing SOI"));
        }
        let mut result = Self::default();
        let mut pos = 2;
        let mut scanned = false;
        let mut entropy = false;
        let mut jfif_seen = false;
        let mut icc_chunks = [false; 256];
        let mut icc_count = 0u8;
        let mut icc_received = 0u16;
        while pos < bytes.len() {
            if entropy {
                // FF00 is a data byte; restart markers stay inside a scan.
                while pos < bytes.len() {
                    if bytes[pos] != 0xff {
                        pos += 1;
                        continue;
                    }
                    let start = pos;
                    while bytes.get(pos) == Some(&0xff) {
                        pos += 1;
                    }
                    let marker = *bytes
                        .get(pos)
                        .ok_or(Error::Unsupported("truncated JPEG scan"))?;
                    if marker == 0 || (0xd0..=0xd7).contains(&marker) {
                        pos += 1;
                    } else {
                        pos = start;
                        break;
                    }
                }
            }
            let start = pos;
            let (marker, end, payload) = segment(bytes, start)?;
            if marker == 0xd9 {
                if end != bytes.len() {
                    return Err(Error::Unsupported("trailing JPEG data"));
                }
                if icc_received != u16::from(icc_count) {
                    return Err(Error::Unsupported("incomplete JPEG ICC chunks"));
                }
                return Ok(result);
            }
            if marker == 0xe2 && payload.starts_with(b"MPF\0") {
                return Err(Error::Unsupported(
                    "multi-picture/gain-map JPEG is not supported",
                ));
            }
            if display_segment(marker, payload) {
                if scanned {
                    return Err(Error::Unsupported(
                        "JPEG display metadata after first scan is not supported",
                    ));
                }
                let keep = match marker {
                    0xe1 => {
                        if result.has_exif {
                            return Err(Error::Unsupported("duplicate JPEG EXIF"));
                        }
                        validate_exif(payload)?;
                        result.has_exif = true;
                        true
                    }
                    0xe2 => {
                        if payload.len() < 14 {
                            return Err(Error::Unsupported("truncated JPEG ICC chunk"));
                        }
                        let (index, count) = (payload[12], payload[13]);
                        if index == 0
                            || count == 0
                            || index > count
                            || icc_chunks[usize::from(index)]
                            || (icc_count != 0 && icc_count != count)
                        {
                            return Err(Error::Unsupported("invalid or duplicate JPEG ICC chunk"));
                        }
                        icc_count = count;
                        icc_received += 1;
                        icc_chunks[usize::from(index)] = true;
                        result.has_icc = true;
                        true
                    }
                    0xe0 => {
                        if jfif_seen || payload.len() < 14 {
                            return Err(Error::Unsupported("invalid or duplicate JFIF"));
                        }
                        jfif_seen = true;
                        let x = u16::from_be_bytes([payload[8], payload[9]]);
                        let y = u16::from_be_bytes([payload[10], payload[11]]);
                        result.has_jfif = payload[7] != 0 || x != y;
                        result.has_jfif
                    }
                    _ => false,
                };
                if keep {
                    let size = result
                        .len()
                        .checked_add(end - start)
                        .ok_or(Error::Limit("JPEG metadata overflow"))?;
                    // Reserve for retained candidates, metadata parsers and workers;
                    // final admission also includes the image's full working set.
                    if size as u64 > memory_budget / 32 {
                        return Err(Error::Limit("JPEG metadata exceeds host budget"));
                    }
                    result.data.extend_from_slice(&bytes[start..end]);
                }
            }
            entropy = marker == 0xda;
            scanned |= entropy;
            pos = end;
        }
        Err(Error::Unsupported("JPEG missing EOI"))
    }

    pub(crate) fn capacity(&self) -> usize {
        self.data.capacity()
    }

    pub(crate) fn attach(&self, encoded: Vec<u8>) -> Result<Vec<u8>, Error> {
        if self.is_empty() {
            return Ok(encoded);
        }
        if !encoded.starts_with(&[0xff, 0xd8]) {
            return Err(Error::Encoding("encoder omitted JPEG SOI".into()));
        }
        let mut out = Vec::with_capacity(encoded.len() + self.len());
        out.extend_from_slice(&encoded[..2]);
        out.extend_from_slice(&self.data);
        let mut pos = 2;
        while pos < encoded.len() {
            let (marker, end, payload) = segment(&encoded, pos)?;
            if matches!(marker, 0xda | 0xd9) {
                out.extend_from_slice(&encoded[pos..]);
                return Ok(out);
            }
            let replace = display_segment(marker, payload) && (marker != 0xe0 || self.has_jfif);
            if !replace {
                out.extend_from_slice(&encoded[pos..end]);
            }
            pos = end;
        }
        Err(Error::Encoding("encoder omitted JPEG scan".into()))
    }
}
