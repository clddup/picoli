//! Resource-admitted integration; raw candidate is not a standalone validator.
use crate::{
    Error, Reference,
    analysis::Analysis,
    optimize::{Candidate, Parameters},
    quality::{self, Strength},
};

fn estimated_peak(pixels: u64, source: u64, control: u64, retained: u64) -> Option<u64> {
    // 24p resident reference/analysis allowance + 40p transient allowance for
    // <=8-bit packed frame, original filtered rows, bounded row alternatives,
    // growing encoded output and validation/rebuild buffers. Decoder is dropped
    // before branching, and raw/original/IDAT/validation buffers before rebuild.
    // Include actual retained candidate capacities, input and four control-file
    // allowances, plus8MiB for compressor snapshots/metadata/row state.
    // This is a conservative admission estimate, NOT an allocator hard limit.
    pixels
        .checked_mul(64)?
        .checked_add(source)?
        .checked_add(control.checked_mul(4)?)?
        .checked_add(retained)?
        .checked_add(8 * 1024 * 1024)
}
fn admitted(estimate: Option<u64>, budget: u64) -> bool {
    estimate.is_some_and(|x| x <= budget)
}
pub(crate) fn refine(
    source: &[u8],
    reference: &Reference,
    analysis: &Analysis,
    candidates: &mut Vec<Candidate>,
    strength: Strength,
) -> Result<(), Error> {
    if strength.percent() == 0 || reference.source_format != "png" {
        return Ok(());
    }
    let Some(winner) = quality::best(candidates.iter(), strength) else {
        return Ok(());
    };
    if matches!(winner.parameters, Parameters::PngRateFiltered { .. }) {
        return Ok(());
    }
    if winner.format() != "png"
        || winner.data.len() < 33
        || &winner.data[..8] != b"\x89PNG\r\n\x1a\n"
    {
        return Err(Error::Encoding(
            "PNG rate finisher received an invalid current PNG".into(),
        ));
    }
    // The working-set estimate is for <=8-bit PNG. Do not change a previously
    // accepted input into an error simply because this optional pass cannot run.
    if winner.data[24] > 8 {
        return Ok(());
    }
    let pixels = u64::from(reference.pixels.width()) * u64::from(reference.pixels.height());
    let retained = candidates
        .iter()
        .try_fold(0u64, |sum, c| sum.checked_add(c.data.capacity() as u64));
    let estimate = retained
        .and_then(|n| estimated_peak(pixels, source.len() as u64, winner.data.len() as u64, n));
    if !admitted(estimate, analysis.memory_budget_bytes) {
        return Ok(());
    }
    // Keep the full strict current-file boundary. Decode candidate rows with
    // exact expanded RGBA comparison and finish all chunks, without allocating
    // a second full RGB/Alpha frame. Raw candidate() alone is still not proof.
    let before = crate::decode_with_budget(&winner.data, analysis.memory_budget_bytes)?;
    let Some(trial) = super::candidate(
        &winner.data,
        usize::try_from(analysis.memory_budget_bytes).unwrap_or(usize::MAX),
    )
    .map_err(Error::Encoding)?
    else {
        return Ok(());
    };
    super::streamed::verify(&before, &trial.data, analysis.memory_budget_bytes)?;
    if trial.data.len() >= winner.bytes {
        return Ok(());
    }
    let refined = Candidate {
        id: format!("{}-rate-prefix-zlib", winner.id),
        parameters: Parameters::PngRateFiltered {
            policy: "fast_prefix_l1_final_l9_64k_v1",
            groups: trial.choices.len(),
            zlib_bytes: trial.final_zlib_bytes,
            model_zlib_bytes: trial.prefix_sizes.last().copied().unwrap_or(0),
        },
        bytes: trial.data.len(),
        data: trial.data,
        score: winner.score.clone(),
    };
    candidates.push(refined);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Vec<u8> {
        let (width, height) = (79u32, 83u32);
        let mut bytes = Vec::new();
        let data: Vec<_> = (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| [x as u8, y as u8, (x + y) as u8]))
            .collect();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_filter(png::Filter::NoFilter);
            encoder.set_deflate_compression(png::DeflateCompression::Level(9));
            encoder.set_pixel_dims(Some(png::PixelDimensions {
                xppu: 3780,
                yppu: 3780,
                unit: png::Unit::Meter,
            }));
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&data)
                .unwrap();
        }
        bytes
    }
    #[test]
    fn admission_checks_exact_boundary_overflow_and_retained_capacity() {
        let peak = estimated_peak(1000, 200, 300, 400).unwrap();
        assert!(admitted(Some(peak), peak));
        assert!(!admitted(Some(peak), peak - 1));
        assert!(estimated_peak(u64::MAX, 0, 0, 0).is_none());
        assert!(!admitted(None, u64::MAX));
        assert_eq!(estimated_peak(1000, 200, 300, 401), Some(peak + 1));
        assert!(estimated_peak(1, 1, u64::MAX, 0).is_none());
    }
    #[test]
    fn actual_finisher_is_lossless_smaller_and_idempotent() {
        let input = source();
        let reference = crate::decode_baseline(&input).unwrap();
        let analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
        let mut candidates = vec![quality::original(&reference, &analysis, &input).unwrap()];
        refine(
            &input,
            &reference,
            &analysis,
            &mut candidates,
            Strength::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        let best = quality::best(candidates.iter(), Strength::default()).unwrap();
        assert!(best.bytes < input.len());
        assert_eq!(best.score.loss, 0.0);
        assert_eq!(
            crate::decode_baseline(&best.data).unwrap().rgba().unwrap(),
            reference.rgba().unwrap()
        );
        let a = png::Decoder::new(Cursor::new(&input)).read_info().unwrap();
        let b = png::Decoder::new(Cursor::new(&best.data))
            .read_info()
            .unwrap();
        let dims = |p: Option<png::PixelDimensions>| p.map(|p| (p.xppu, p.yppu, p.unit as u8));
        assert_eq!(dims(a.info().pixel_dims), dims(b.info().pixel_dims));
        refine(
            &input,
            &reference,
            &analysis,
            &mut candidates,
            Strength::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
    }
    #[test]
    fn strength_zero_and_insufficient_stage_budget_keep_old_bytes() {
        let input = source();
        let reference = crate::decode_baseline(&input).unwrap();
        let mut analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
        let mut candidates = vec![quality::original(&reference, &analysis, &input).unwrap()];
        refine(
            &input,
            &reference,
            &analysis,
            &mut candidates,
            Strength::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].data, input);
        analysis.memory_budget_bytes = 1; // Directly exercise optional-stage admission.
        refine(
            &input,
            &reference,
            &analysis,
            &mut candidates,
            Strength::default(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].data, input);
    }
    use std::io::Cursor;
}
