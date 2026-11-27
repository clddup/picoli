//! Optional-pass working-set admission estimates, not allocator/RSS hard limits.
use super::solver::Point;
use crate::{
    Reference,
    analysis::Analysis,
    optimize::{Candidate, Parameters},
};
use std::mem::size_of;

// BTree entries contain two i128 moments, eleven i64 rate sums and the key.
// Allow partially filled nodes/internal edges as well as 2KiB per map for the
// first/root node and map headers. Check allowance before every new key.
pub const HISTOGRAM_ENTRY_BYTES: u64 = 384;
pub const PROJECTED_ENTRY_BYTES: u64 = 576;
const MAP_ROOT_BYTES: u64 = 64 * 2048;
const FIXED_SCRATCH: u64 = 256 * 1024;

pub fn sampling_factors(sampling: u16) -> Option<(u64, u64)> {
    match sampling {
        444 => Some((1, 1)),
        422 => Some((2, 1)),
        420 => Some((2, 2)),
        _ => None,
    }
}
// v71 validated padded MCU component payload; do NOT use ceil(pixel_count/64).
pub fn payload(width: u32, height: u32, sampling: u16) -> Option<u64> {
    let (h, v) = sampling_factors(sampling)?;
    u64::from(width)
        .div_ceil(8 * h)
        .checked_mul(u64::from(height).div_ceil(8 * v))?
        .checked_mul(h * v + 2)?
        .checked_mul(128)
}
// v71 validated Vec<i16> raw_coeff MCU row, row/row_up,
// first_row_upsample_dest and upsample_dest. Width matters even at height=1.
pub fn decoder_working(width: u32, sampling: u16) -> Option<u64> {
    let (h, v) = sampling_factors(sampling)?;
    let columns = u64::from(width).div_ceil(8 * h);
    let mut bytes = columns.checked_mul(8)?.checked_mul(h)?.checked_mul(2)?;
    for (ch, cv) in [(h, v), (1, 1), (1, 1)] {
        let stride = columns.checked_mul(ch)?.checked_mul(8)?;
        bytes = bytes.checked_add(stride.checked_mul(cv)?.checked_mul(16)?)?;
        if h != 1 || v != 1 {
            let ratio = (h / ch) * (v / cv);
            bytes = bytes.checked_add(stride.checked_mul(cv)?.checked_mul(4)?)?;
            bytes =
                bytes.checked_add(stride.checked_mul(cv)?.checked_mul(ratio)?.checked_mul(2)?)?;
            bytes = bytes.checked_add(stride.checked_mul(ratio)?.checked_mul(16)?)?;
        }
    }
    Some(bytes)
}

pub(super) fn parameter_heap(parameters: &Parameters) -> Option<u64> {
    match parameters {
        Parameters::JpegTableAllocated {
            control,
            luma_quantization_table,
            rewritten_frequencies,
            ..
        } => (size_of::<Parameters>() as u64)
            .checked_add((luma_quantization_table.capacity() as u64).checked_mul(2)?)?
            .checked_add(
                (rewritten_frequencies.capacity() as u64).checked_mul(size_of::<usize>() as u64)?,
            )?
            .checked_add(parameter_heap(control)?),
        Parameters::JpegChromaRefined { control, .. } => {
            (size_of::<Parameters>() as u64).checked_add(parameter_heap(control)?)
        }
        _ => Some(0),
    }
}

#[derive(Debug)]
pub struct Admission {
    pub coefficients: u64,
    pub output_limit: usize,
    pub histogram_entries: usize,
    #[cfg(test)]
    pub fixed_bytes: u64,
}

/// Charge everything retained by the caller plus a conservative simultaneous
/// superset of stage-local peaks. In reality the original snapshot/RGB are
/// dropped before recapture and each encoder frame before candidate decoding.
/// Histograms and all full legal-Q curves are bounded independently of content.
pub fn preflight(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &Vec<Candidate>,
    current: &Candidate,
    sampling: u16,
) -> Option<Admission> {
    preflight_model(
        source, reference, analysis, candidates, current, sampling, false,
    )
}

pub fn preflight_coupled(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &Vec<Candidate>,
    current: &Candidate,
    sampling: u16,
) -> Option<Admission> {
    preflight_model(
        source, reference, analysis, candidates, current, sampling, true,
    )
}

fn preflight_model(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &Vec<Candidate>,
    current: &Candidate,
    sampling: u16,
    coupled: bool,
) -> Option<Admission> {
    let (width, height) = reference.pixels.dimensions();
    if width == 0 || height == 0 || width > 65535 || height > 65535 {
        return None;
    }
    let coefficients = payload(width, height, sampling)?;
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    let blocks = u64::from(width)
        .div_ceil(8)
        .checked_mul(u64::from(height).div_ceil(8))?;
    let raw_fdct = blocks.checked_mul(128)?;
    // The writer emits no display metadata. Attaching metadata can remove a
    // generated JFIF (18 bytes); 32 bytes slack preserves every smaller full JPEG.
    let output_limit = current.data.len().checked_add(32)?;
    let attached_cap = (output_limit as u64).checked_add(reference.jpeg_metadata.len() as u64)?;
    // Capacity, not len: every old candidate stays resident, including losers.
    let retained = candidates.iter().try_fold(0u64, |n, c| {
        n.checked_add(c.data.capacity() as u64)?
            .checked_add(c.id.capacity() as u64)?
            .checked_add(parameter_heap(&c.parameters)?)
    })?;
    // Source is a borrowed slice; charge twice its length for the caller's input
    // buffer/read growth, without claiming visibility into arbitrary host storage.
    let resident = (source.len() as u64)
        .checked_mul(2)?
        .checked_add(reference.pixels.as_raw().capacity() as u64)?
        .checked_add(reference.jpeg_metadata.capacity() as u64)?
        .checked_add(analysis.weights.capacity() as u64)?
        // Retain the pre-compaction 12B/pixel reserve: storage changes alone
        // must not enable extra optional searches at an unchanged host budget.
        .checked_add(
            (analysis.colors.capacity() as u64).checked_mul(size_of::<[i32; 3]>() as u64)?,
        )?
        .checked_add(retained)?
        // Include candidate-vector reallocation when appending the sole result.
        .checked_add(
            (candidates.capacity().max(1) as u64).checked_mul(3 * size_of::<Candidate>() as u64)?,
        )?;
    let curve_bytes =
        (66 * 256 * size_of::<Point>() as u64).checked_add(64 * size_of::<Vec<Point>>() as u64)?;
    let score_scratch = u64::from(width).checked_mul(12)?.checked_add(
        u64::from(width)
            .div_ceil(16)
            .checked_mul(u64::from(height).div_ceil(16))?
            .checked_mul(16)?,
    )?;
    let fixed = resident
        .checked_add(raw_fdct)?
        // Captured padded planes AND serializer's owned zigzag AlignedBlocks;
        // also covers an ordinary progressive decode plane after frame release.
        .checked_add(coefficients.checked_mul(2)?)?
        .checked_add(pixels.checked_mul(3)?)?
        .checked_add(decoder_working(width, sampling)?)?
        // Both serialized alternatives plus active writer/metadata attachment.
        .checked_add(attached_cap.checked_mul(3)?)?
        // Metadata extraction, image decoder validation/ICC and strict decoder
        // copies can overlap. Keep the existing metadata parser reserve factor.
        .checked_add((reference.jpeg_metadata.len() as u64).checked_mul(32)?)?
        .checked_add(score_scratch)?
        .checked_add(curve_bytes)?
        .checked_add(MAP_ROOT_BYTES)?
        .checked_add(FIXED_SCRATCH)?;
    // Leave v81 fixed/entry arithmetic EXACT for its original prefix admission.
    // Coupled repair adds 8B/pixel factors, 8B/pixel joint field and a16B cache
    // per actual luma block. A projected map node has larger signed-key moments:
    // reserve an extra1KiB per root, in addition to576B per dynamic new key.
    let (fixed, entry_bytes, max_entries) = if coupled {
        (
            fixed
                .checked_add(pixels.checked_mul(16)?)?
                .checked_add(blocks.checked_mul(16)?)?
                .checked_add(64 * 1024)?,
            PROJECTED_ENTRY_BYTES,
            blocks.checked_mul(63)?,
        )
    } else {
        (
            fixed,
            HISTOGRAM_ENTRY_BYTES,
            blocks.min(32769).checked_mul(63)?,
        )
    };
    let available = analysis.memory_budget_bytes.checked_sub(fixed)?;
    let entries = (available / entry_bytes).min(max_entries);
    // Every nonempty image needs at least one entry in each AC-frequency map.
    if entries < 63 {
        return None;
    }
    Some(Admission {
        coefficients,
        output_limit,
        histogram_entries: usize::try_from(entries).ok()?,
        #[cfg(test)]
        fixed_bytes: fixed,
    })
}
