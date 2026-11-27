//! One optional exact-pixel PNG recompression attempt at fixed libdeflate level12.
//! Compression algorithms are provided by upstream C libdeflate via libdeflater.
use crate::{
    Error, Reference,
    analysis::Analysis,
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};
use std::ops::Range;

// Optional-stage allowance for the fixed upstream context and scratch; this is
// admission accounting, not an allocator or process memory ceiling.
const BACKEND_ALLOWANCE: u64 = 32 * 1024 * 1024;

struct Png {
    chunks: Vec<([u8; 4], Range<usize>)>,
    idat: Vec<u8>,
    chunk_size: usize,
    filtered_size: usize,
}
fn parse(input: &[u8]) -> Result<Option<Png>, Error> {
    if input.len() < 33 || input.get(..8) != Some(b"\x89PNG\r\n\x1a\n") {
        return Err(Error::Encoding(
            "PNG deflate finisher received invalid PNG".into(),
        ));
    }
    let mut chunks = Vec::new();
    let mut idat = Vec::new();
    let mut chunk_size = 1;
    let mut supported = true;
    let mut pos = 8usize;
    while pos + 12 <= input.len() {
        let size = u32::from_be_bytes(input[pos..pos + 4].try_into().unwrap()) as usize;
        let end = pos
            .checked_add(size)
            .and_then(|p| p.checked_add(12))
            .ok_or_else(|| Error::Encoding("PNG chunk overflow".into()))?;
        if end > input.len() {
            return Err(Error::Encoding("Truncated PNG chunk".into()));
        }
        let kind: [u8; 4] = input[pos + 4..pos + 8].try_into().unwrap();
        let crc = u32::from_be_bytes(input[end - 4..end].try_into().unwrap());
        if crc32fast::hash(&input[pos + 4..end - 4]) != crc {
            return Err(Error::Encoding("PNG chunk CRC mismatch".into()));
        }
        supported &= matches!(
            &kind,
            b"IHDR" | b"PLTE" | b"tRNS" | b"IDAT" | b"IEND" | b"pHYs"
        );
        if &kind == b"IDAT" {
            idat.extend_from_slice(&input[pos + 8..end - 4]);
            chunk_size = chunk_size.max(size);
        }
        chunks.push((kind, pos..end));
        pos = end;
        if &kind == b"IEND" {
            break;
        }
    }
    if pos != input.len()
        || chunks.first().map(|c| c.0) != Some(*b"IHDR")
        || chunks.last().map(|c| c.0) != Some(*b"IEND")
        || idat.is_empty()
    {
        return Err(Error::Encoding("Incomplete PNG chunks".into()));
    }
    // No unsupported input becomes a new global processing error: this is an
    // optional <=8-bit, noninterlaced pass, not a general standalone PNG reader.
    if !supported || input[28] != 0 || !matches!(input[24], 1 | 2 | 4 | 8) {
        return Ok(None);
    }
    let width = u32::from_be_bytes(input[16..20].try_into().unwrap()) as usize;
    let height = u32::from_be_bytes(input[20..24].try_into().unwrap()) as usize;
    let samples = match input[25] {
        0 | 3 => 1usize,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => return Ok(None),
    };
    let filtered_size = width
        .checked_mul(samples)
        .and_then(|n| n.checked_mul(usize::from(input[24])))
        .and_then(|n| n.checked_add(7))
        .map(|n| n / 8)
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_mul(height));
    let Some(filtered_size) = filtered_size else {
        return Ok(None);
    };
    Ok(Some(Png {
        chunks,
        idat,
        chunk_size,
        filtered_size,
    }))
}
fn add_idat(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    output.extend_from_slice(b"IDAT");
    output.extend_from_slice(bytes);
    let mut crc = crc32fast::Hasher::new();
    crc.update(b"IDAT");
    crc.update(bytes);
    output.extend_from_slice(&crc.finalize().to_be_bytes());
}
pub(crate) struct Trial {
    pub data: Vec<u8>,
    pub zlib_bytes: usize,
}
#[cfg(test)]
pub(crate) fn candidate(input: &[u8], budget: u64) -> Result<Option<Trial>, Error> {
    candidate_with_level(input, budget, 12)
}

pub(crate) fn candidate_with_level(
    input: &[u8],
    budget: u64,
    level_num: i32,
) -> Result<Option<Trial>, Error> {
    let Some(parsed) = parse(input)? else {
        return Ok(None);
    };
    if parsed.filtered_size as u64 > budget / 4 {
        return Ok(None);
    }
    let filtered =
        miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&parsed.idat, parsed.filtered_size)
            .map_err(|e| Error::Encoding(e.to_string()))?;
    if filtered.len() != parsed.filtered_size {
        return Err(Error::Encoding("PNG filtered row length mismatch".into()));
    }
    let level = libdeflater::CompressionLvl::new(level_num)
        .map_err(|e| Error::Encoding(format!("Invalid fixed deflate level: {e:?}")))?;
    let Some(mut compressor) = libdeflater::Compressor::try_new(level) else {
        return Ok(None);
    };
    let bound = compressor.zlib_compress_bound(filtered.len());
    let mut compressed = Vec::new();
    if compressed.try_reserve_exact(bound).is_err() {
        return Ok(None);
    }
    compressed.resize(bound, 0);
    let size = compressor
        .zlib_compress(&filtered, &mut compressed)
        .map_err(|e| Error::Encoding(format!("Deflate compression failed: {e:?}")))?;
    compressed.truncate(size);
    drop(compressor);
    drop(filtered);
    let zlib_bytes = compressed.len();
    let non_idat = parsed
        .chunks
        .iter()
        .filter(|c| &c.0 != b"IDAT")
        .map(|c| c.1.len())
        .sum::<usize>();
    let capacity = compressed
        .len()
        .checked_add(non_idat)
        .and_then(|n| {
            n.checked_add(
                compressed
                    .len()
                    .div_ceil(parsed.chunk_size)
                    .checked_mul(12)?,
            )
        })
        .and_then(|n| n.checked_add(8))
        .ok_or_else(|| Error::Encoding("PNG output size overflow".into()))?;
    let mut data = Vec::with_capacity(capacity);
    data.extend_from_slice(&input[..8]);
    let mut written = false;
    for (kind, range) in parsed.chunks {
        if &kind == b"IDAT" {
            if !written {
                for part in compressed.chunks(parsed.chunk_size) {
                    add_idat(&mut data, part);
                }
                written = true;
            }
        } else {
            data.extend_from_slice(&input[range]);
        }
    }
    Ok(Some(Trial { data, zlib_bytes }))
}
fn estimate(pixels: u64, source: u64, control: u64, retained: u64) -> Option<u64> {
    // Conservative optional-stage admission, not an allocator ceiling. Include
    // resident source/analysis, decoded current frame, growing PNG/inflate
    // buffers, retained candidate capacities and a separate backend allowance.
    pixels
        .checked_mul(64)?
        .checked_add(source)?
        .checked_add(control.checked_mul(4)?)?
        .checked_add(retained)?
        .checked_add(BACKEND_ALLOWANCE)
}
fn admitted(peak: Option<u64>, budget: u64) -> bool {
    peak.is_some_and(|n| n <= budget)
}
pub(crate) fn refine(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    refine_impl(
        source,
        reference,
        analysis,
        candidates,
        strength,
        Some(12),
        true,
    )
}

pub(crate) fn refine_fast(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    refine_impl(
        source,
        reference,
        analysis,
        candidates,
        strength,
        None,
        cfg!(debug_assertions),
    )
}

fn refine_impl(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
    fixed_level: Option<i32>,
    verify_decode: bool,
) -> Result<(), Error> {
    if strength.percent() == 0 || reference.source_format != "png" {
        return Ok(());
    }
    let Some(winner) = quality::best(candidates.iter(), strength) else {
        return Ok(());
    };
    if matches!(winner.parameters, Parameters::PngDeflatePacked { .. }) {
        return Ok(());
    }
    if winner.format() != "png" {
        return Ok(());
    }
    let retained = candidates
        .iter()
        .try_fold(0u64, |n, c| n.checked_add(c.data.capacity() as u64));
    let pixels = u64::from(reference.pixels.width()) * u64::from(reference.pixels.height());
    let peak = retained.and_then(|n| estimate(pixels, source.len() as u64, winner.bytes as u64, n));
    if !admitted(peak, analysis.memory_budget_bytes) {
        return Ok(());
    }
    // Use level 12 on small/medium filtered payloads (fast enough at <=512 KiB
    // and avoids level-10 regressions on repetitive graphics), and level 10
    // (near-optimal parsing, ~3x faster than level 12) on larger payloads.
    let level = match fixed_level {
        Some(lvl) => lvl,
        None => {
            let parsed_len = parse(&winner.data)?.map_or(usize::MAX, |p| p.filtered_size);
            if parsed_len <= 512 * 1024 { 12 } else { 10 }
        }
    };
    let Some(trial) = candidate_with_level(&winner.data, analysis.memory_budget_bytes, level)?
    else {
        return Ok(());
    };
    if trial.data.len() >= winner.bytes {
        return Ok(());
    }
    if verify_decode {
        let before = crate::decode_with_budget(&winner.data, analysis.memory_budget_bytes)?;
        crate::png_rate::verify_current(&before, &trial.data, analysis.memory_budget_bytes)?;
    }
    let policy = match level {
        10 => "libdeflate10_v1",
        _ => "libdeflate12_v1",
    };
    candidates.push(Candidate {
        id: format!("{}-deflate-packed", winner.id),
        parameters: Parameters::PngDeflatePacked {
            policy,
            zlib_bytes: trial.zlib_bytes,
        },
        bytes: trial.data.len(),
        data: trial.data,
        score: winner.score.clone(),
    });
    Ok(())
}
#[cfg(test)]
mod tests;
