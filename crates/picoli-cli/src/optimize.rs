use super::{Args, sha256, write_new};
use picoli_core::{
    MAX_INPUT_BYTES,
    analysis::Analysis,
    decode_with_budget,
    optimize::{self, Candidate},
    quality::{self, Strength},
};
use rayon::prelude::*;
use std::{
    error::Error,
    fs::{self, File},
    io::Read,
    time::Instant,
};

pub(super) fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let strength = Strength::new(args.strength)?;
    let mut source = Vec::new();
    File::open(&args.input)?
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut source)?;
    let host_memory = super::resources::memory_budget(args.memory_mib)?;
    let reference = decode_with_budget(&source, host_memory.budget_bytes)?;
    let plan = picoli_core::resources::Plan::with_metadata(
        reference.pixels.width(),
        reference.pixels.height(),
        source.len(),
        reference.jpeg_metadata.len(),
        host_memory.budget_bytes,
        args.jobs,
    )?;
    let analysis = Analysis::for_reference(&reference, host_memory.budget_bytes)?;
    let jobs = if strength.percent() == 0 {
        Vec::new()
    } else {
        optimize::jobs(reference.source_format)?
    };
    let threads = plan.workers.min(jobs.len().max(1));
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&args.output)?;
    eprintln!(
        "Strength {}%; {} {} search jobs, {threads} workers; no file-size target",
        strength.percent(),
        jobs.len(),
        reference.source_format
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
    let evaluated: Result<Vec<Option<Candidate>>, picoli_core::Error> = pool.install(|| {
        jobs.par_iter()
            .map(|&job| quality::run_job(&reference, &analysis, job, strength))
            .collect()
    });
    let evaluated = evaluated?;
    let rejected: Vec<_> = jobs
        .iter()
        .zip(&evaluated)
        .filter(|(_, c)| c.is_none())
        .map(|(job, _)| format!("{job:?}"))
        .collect();
    let mut results = vec![quality::original(&reference, &analysis, &source)?];
    for mut candidate in evaluated.into_iter().flatten() {
        quality::preserve_png_density(&source, &mut candidate)?;
        results.push(candidate);
    }
    let pixels = u64::from(reference.pixels.width()) * u64::from(reference.pixels.height());
    if reference.source_format == "jpeg" {
        quality::refine_jpeg_scan_huffman_candidates(
            &reference,
            &analysis,
            &mut results,
            strength,
        )?;
        quality::refine_jpeg_table_fast(&source, &reference, &analysis, &mut results, strength)?;
        let winner_sampling =
            quality::best(results.iter(), strength).and_then(|c| match &c.parameters {
                optimize::Parameters::Jpeg { sampling, .. } => Some(*sampling),
                optimize::Parameters::JpegTableAllocated { control, .. } => {
                    match control.as_ref() {
                        optimize::Parameters::Jpeg { sampling, .. } => Some(*sampling),
                        _ => None,
                    }
                }
                _ => None,
            });
        if winner_sampling != Some(420) && pixels <= 2_500_000 {
            quality::refine_jpeg_chroma_fast(
                &source,
                &reference,
                &analysis,
                &mut results,
                strength,
            )?;
        }
    } else {
        quality::refine_png_candidates(&source, &mut results, strength)?;
        if pixels <= 300_000 {
            quality::refine_png_rate_candidates(
                &source,
                &reference,
                &analysis,
                &mut results,
                strength,
            )?;
        }
        quality::refine_png_deflate_fast(&source, &reference, &analysis, &mut results, strength)?;
    }
    let extension = if reference.source_format == "jpeg" {
        "jpg"
    } else {
        "png"
    };
    for c in &results {
        write_new(
            &args.output.join(format!("{}.{}", c.id, extension)),
            &c.data,
        )?;
    }
    let selected =
        quality::best(results.iter(), strength).ok_or("no candidate meets error bounds")?;
    quality::verify_transparency(&reference, &selected.data, host_memory.budget_bytes)?;
    write_new(
        &args.output.join(format!("selected.{extension}")),
        &selected.data,
    )?;
    let report = serde_json::json!({
        "status":"experimental_default_not_perceptually_calibrated", "production_eligible":false,
        "strength_percent":strength.percent(), "error_limits":strength.limits(),
        "threads":threads,"host_memory":host_memory,"resource_plan":plan,
        "resource_measurement":"estimates_not_peak_RSS",
        "elapsed_ms":start.elapsed().as_millis(),
        "source":{"sha256":sha256(&source),"bytes":source.len(),"format":reference.source_format,"dimensions":reference.pixels.dimensions()},
        "selected":{"id":selected.id,"bytes":selected.bytes,"sha256":sha256(&selected.data),"parameters":selected.parameters,"score":selected.score},
        "candidates":results,"rejected_profiles":rejected,
        "transparency":"per_pixel_alpha_preserved_no_matte; encoded_alpha_checked_when_present",
        "has_transparency":reference.alpha.is_some(),
        "quality_gate":"proxy_limits_met_human_equivalence_not_proven", "tinypng":"not_evaluated",
        "selection":"smallest same-format file within predeclared strength error bounds; original retained as exact fallback; no target byte count"
    });
    write_new(
        &args.output.join("report.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "{}: {} -> {} bytes, {}, strength {}%",
        reference.source_format,
        source.len(),
        selected.bytes,
        selected.id,
        strength.percent()
    );
    println!(
        "Results: {}. Visual quality and competitor equivalence require separate evaluation.",
        args.output.display()
    );
    Ok(())
}

#[cfg(test)]
mod table_parameter_tests {
    use picoli_core::optimize::Parameters;

    #[test]
    fn allocated_jpeg_serializes_literal_table_coding_and_model_provenance() {
        let parameters = Parameters::JpegTableAllocated {
            control: Box::new(Parameters::Original {
                source_format: "jpeg",
            }),
            luma_quantization_table: vec![8; 64],
            rewritten_frequencies: vec![1, 17],
            scans_per_component: 4,
            eob_runs: true,
            scan_huffman: true,
            model: "v80_source_fdct_histogram_independent_ac_critical16_full_legal_q",
            allocation: "single_luma_tables",
            exchanges: 2,
        };
        let value = serde_json::to_value(parameters).unwrap();
        assert_eq!(value["format"], "jpeg_table_allocated");
        assert_eq!(
            value["luma_quantization_table"],
            serde_json::json!(vec![8; 64])
        );
        assert_eq!(value["rewritten_frequencies"], serde_json::json!([1, 17]));
        assert_eq!(value["scans_per_component"], 4);
        assert_eq!(value["eob_runs"], true);
        assert_eq!(value["scan_huffman"], true);
        assert_eq!(value["allocation"], "single_luma_tables");
        assert_eq!(value["exchanges"], 2);
        assert_eq!(
            value["model"],
            "v80_source_fdct_histogram_independent_ac_critical16_full_legal_q"
        );
        assert_eq!(value["control"]["source_format"], "jpeg");
    }
}
