mod optimize;
mod resources;

use image::GenericImageView;
use picoli_core::{
    Diagnostics, MAX_INPUT_BYTES, decode_with_budget, diagnostics, encode_jpeg_baseline,
    encode_png_lossless,
};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

const USAGE: &str = "Picoli - local JPG and PNG compressor\n\nUsage: picoli optimize INPUT NEW_OUTPUT_DIR [--strength 0..100] [--jobs 4] [--memory-mib N]\n       picoli baseline INPUT NEW_OUTPUT_DIR [--target-bytes N] [--jobs 4] [--memory-mib N]";

#[derive(Debug)]
struct Args {
    optimize: bool,
    input: PathBuf,
    output: PathBuf,
    target: usize,
    strength: u8,
    jobs: usize,
    memory_mib: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Candidate {
    format: &'static str,
    quality: Option<u8>,
    bytes: usize,
    sha256: String,
    meets_byte_budget: bool,
    diagnostics: Diagnostics,
    elapsed_ms: u128,
}

fn sha256(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        result.push(HEX[usize::from(byte >> 4)] as char);
        result.push(HEX[usize::from(byte & 15)] as char);
    }
    result
}

fn args() -> Result<Args, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let optimize = match args.next().as_deref().and_then(|s| s.to_str()) {
        Some("baseline") => false,
        Some("optimize") => true,
        _ => return Err(USAGE.into()),
    };
    let input = args.next().ok_or(USAGE)?.into();
    let output = args.next().ok_or(USAGE)?.into();
    let mut result = Args {
        optimize,
        input,
        output,
        target: 65_000, // Legacy baseline experiment only, never the optimizer default.
        strength: picoli_core::quality::DEFAULT_STRENGTH,
        jobs: std::thread::available_parallelism().map_or(1, |n| n.get().min(4)),
        memory_mib: None,
    };
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(USAGE)?.into_string().map_err(|_| USAGE)?;
        match flag.to_str() {
            Some("--target-bytes") if !optimize => result.target = value.parse()?,
            Some("--strength") if optimize => result.strength = value.parse()?,
            Some("--jobs") => result.jobs = value.parse()?,
            Some("--memory-mib") => result.memory_mib = Some(value.parse()?),
            _ => return Err(USAGE.into()),
        }
    }
    picoli_core::quality::Strength::new(result.strength)?;
    if result.target == 0
        || result.jobs == 0
        || result.memory_mib.is_some_and(|m| !(64..=4096).contains(&m))
    {
        return Err("target/jobs must be positive; memory-mib must be 64..=4096".into());
    }
    Ok(result)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn run(args: Args) -> Result<(), Box<dyn Error>> {
    if args.optimize {
        return optimize::run(args);
    }
    let total_start = Instant::now();
    let mut source = Vec::new();
    File::open(&args.input)?
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut source)?;
    let host_memory = resources::memory_budget(args.memory_mib)?;
    let memory_bytes = host_memory.budget_bytes;
    let reference = decode_with_budget(&source, memory_bytes)?;
    let plan = picoli_core::resources::Plan::with_metadata(
        reference.pixels.width(),
        reference.pixels.height(),
        source.len(),
        reference.jpeg_metadata.len(),
        memory_bytes,
        args.jobs,
    )?;
    let jobs = plan.workers.min(100);
    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&args.output)?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;
    eprintln!(
        "Evaluating 100 JPEG baselines with {jobs} workers; quality acceptance remains pending."
    );
    let candidates: Result<Vec<Candidate>, picoli_core::Error> = pool.install(|| {
        (1u8..=100)
            .into_par_iter()
            .map(|quality| {
                let start = Instant::now();
                let encoded = encode_jpeg_baseline(&reference, quality)?;
                let reconstructed = decode_with_budget(&encoded, memory_bytes)?;
                Ok(Candidate {
                    format: "jpeg",
                    quality: Some(quality),
                    bytes: encoded.len(),
                    sha256: sha256(&encoded),
                    meets_byte_budget: encoded.len() <= args.target,
                    diagnostics: diagnostics(&reference.pixels, &reconstructed.pixels)?,
                    elapsed_ms: start.elapsed().as_millis(),
                })
            })
            .collect()
    });
    let candidates = candidates?;
    let selected = candidates
        .iter()
        .filter(|c| c.meets_byte_budget)
        .min_by(|a, b| {
            a.diagnostics
                .rgb_rmse_0_255
                .total_cmp(&b.diagnostics.rgb_rmse_0_255)
                .then(
                    a.diagnostics
                        .gradient_mae_normalized
                        .total_cmp(&b.diagnostics.gradient_mae_normalized),
                )
                .then(a.bytes.cmp(&b.bytes))
                .then(a.quality.cmp(&b.quality))
        });
    if let Some(best) = selected {
        let encoded =
            encode_jpeg_baseline(&reference, best.quality.ok_or("JPEG quality missing")?)?;
        if sha256(&encoded) != best.sha256 {
            return Err("non-deterministic JPEG re-encode".into());
        }
        write_new(&args.output.join("jpeg-budget-baseline.jpg"), &encoded)?;
    }
    let png_start = Instant::now();
    let png = encode_png_lossless(&reference)?;
    let png_reconstructed = decode_with_budget(&png, memory_bytes)?;
    let png_candidate = Candidate {
        format: "png",
        quality: None,
        bytes: png.len(),
        sha256: sha256(&png),
        meets_byte_budget: png.len() <= args.target,
        diagnostics: diagnostics(&reference.pixels, &png_reconstructed.pixels)?,
        elapsed_ms: png_start.elapsed().as_millis(),
    };
    write_new(&args.output.join("png-lossless-reference.png"), &png)?;
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let report = serde_json::json!({
        "schema_version": 1,
        "engine": "picoli-rust-baseline",
        "engine_version": env!("CARGO_PKG_VERSION"),
        "implementation_status": "baseline_only",
        "production_eligible": false,
        "source": {"sha256": sha256(&source), "bytes": source.len(),
            "format": reference.source_format, "dimensions": reference.pixels.dimensions()},
        "configuration": {"target_bytes": args.target, "requested_jobs": args.jobs,
            "effective_jobs": jobs, "host_memory": host_memory, "resource_plan": plan,
            "selection": "lowest RGB RMSE among all 100 integer qualities within byte budget; diagnostic only"},
        "build": {"rustc": env!("PICOLI_BUILD_RUSTC")},
        "environment": {"runtime_rustc_observation": rustc, "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH},
        "jpeg_backend": "image 0.25.10 JpegEncoder, emitted SOF sampling 1x1/1x1/1x1 (4:4:4)",
        "jpeg_candidates": candidates,
        "selected_jpeg": selected,
        "png_lossless": png_candidate,
        "png_track": "canonical lossless PNG from original decoded RGB; no custom quantizer implemented",
        "gates": {"quality": "not_evaluated", "tinypng": "not_evaluated",
            "reference_integrity": "strict_entropy_consumption_checked",
            "qr_payload": "not_evaluated", "word_compatibility": "not_evaluated",
            "overall_65000_task": "not_passed_quality_gate_pending"},
        "elapsed_ms": total_start.elapsed().as_millis()
    });
    write_new(
        &args.output.join("report.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    // Evaluation-only contact sheet uses supplied sample regions only when its hash matches.
    // These coordinates never enter the optimizer or candidate selection.
    if sha256(&source) == "efa8b980a3886dbae78ed9cd5dd142012db8bfd37a2a1b4b8610b32bbd042ebe" {
        let regions = [
            (330, 595, 490, 150),
            (1160, 275, 135, 135),
            (1195, 740, 240, 235),
        ];
        for (label, pixels) in [
            ("original", &reference.pixels),
            ("png", &png_reconstructed.pixels),
        ] {
            for (i, &(x, y, w, h)) in regions.iter().enumerate() {
                let crop = image::DynamicImage::ImageRgb8(pixels.view(x, y, w, h).to_image());
                let mut bytes = std::io::Cursor::new(Vec::new());
                crop.write_to(&mut bytes, image::ImageFormat::Png)?;
                write_new(
                    &args.output.join(format!("{label}-roi-{i}.png")),
                    &bytes.into_inner(),
                )?;
            }
        }
        if selected.is_some() {
            let encoded = fs::read(args.output.join("jpeg-budget-baseline.jpg"))?;
            let decoded = decode_with_budget(&encoded, memory_bytes)?;
            for (i, &(x, y, w, h)) in regions.iter().enumerate() {
                let crop =
                    image::DynamicImage::ImageRgb8(decoded.pixels.view(x, y, w, h).to_image());
                let mut bytes = std::io::Cursor::new(Vec::new());
                crop.write_to(&mut bytes, image::ImageFormat::Png)?;
                write_new(
                    &args.output.join(format!("jpeg-roi-{i}.png")),
                    &bytes.into_inner(),
                )?;
            }
        }
    }
    println!(
        "Source: {} bytes, {}x{}",
        source.len(),
        reference.pixels.width(),
        reference.pixels.height()
    );
    match selected {
        Some(c) => println!(
            "JPEG byte-budget baseline: {} bytes, q={}; NOT quality-approved",
            c.bytes,
            c.quality.unwrap_or(0)
        ),
        None => println!("JPEG: no integer-quality candidate fits the byte budget"),
    }
    println!(
        "PNG pixel-lossless baseline: {} bytes; fits budget: {}",
        png.len(),
        png.len() <= args.target
    );
    println!("Report: {}", args.output.join("report.json").display());
    println!("TinyPNG/visual/Word gates remain not evaluated. No image was uploaded.");
    Ok(())
}

fn main() {
    if std::env::args_os().len() == 2
        && std::env::args_os()
            .nth(1)
            .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return;
    }
    if let Err(error) = args().and_then(run) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
