//! Single-threaded baseline and experimental compression interfaces.
use picoli_core::{decode_baseline, encode_jpeg_baseline, encode_png_lossless};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn baseline_encode(input: &[u8], format: &str, quality: u8) -> Result<Vec<u8>, JsValue> {
    let reference = decode_baseline(input).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let result = match format {
        "jpeg" => encode_jpeg_baseline(&reference, quality),
        "png" => encode_png_lossless(&reference),
        _ => return Err(JsValue::from_str("format must be jpeg or png")),
    };
    result.map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Header-only adaptive one-worker working-set estimate; not free RAM/RSS.
/// The returned JSON has width, height, format, required_memory_bytes and memory_mib.
/// Actual compression still validates the complete image.
#[wasm_bindgen]
pub fn compression_memory_plan(input: &[u8]) -> Result<String, JsValue> {
    let plan = picoli_core::compression_memory_plan(input)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    serde_json::to_string(&plan).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Optional compression strength percentage; input format and dimensions stay unchanged.
/// When `memory_mib` is omitted, the working-set budget adapts automatically to the image.
/// Proxy error limits are experimental and do not prove perceptual equivalence.
#[wasm_bindgen]
pub fn compress_experimental(
    input: &[u8],
    strength_percent: Option<u32>,
    memory_mib: Option<u32>,
) -> Result<Vec<u8>, JsValue> {
    let percent = strength_percent.unwrap_or(u32::from(picoli_core::quality::DEFAULT_STRENGTH));
    if percent > 100 {
        return Err(JsValue::from_str("strength must be 0..=100 percent"));
    }
    let strength = picoli_core::quality::Strength::new(percent as u8)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let budget = match memory_mib {
        Some(mib) if mib >= 16 => u64::from(mib)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| JsValue::from_str("WASM memory budget overflow"))?,
        Some(_) => {
            return Err(JsValue::from_str(
                "WASM memory budget must be at least 16 MiB",
            ));
        }
        None => {
            let plan = picoli_core::compression_memory_plan(input)
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            plan.budget_bytes()
                .ok_or_else(|| JsValue::from_str("WASM memory budget overflow"))?
        }
    };
    let candidate = picoli_core::quality::compress_with_budget(input, strength, budget)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(candidate.data)
}

#[wasm_bindgen]
pub fn baseline_capabilities() -> String {
    serde_json::json!({
        "status": "baseline_only",
        "production_eligible": false,
        "formats": ["jpeg", "png"],
        "threads": 1,
        "max_input_bytes": picoli_core::MAX_INPUT_BYTES,
        "admission": "estimated_memory_not_fixed_megapixels",
        "default_memory_budget_bytes": picoli_core::resources::DEFAULT_MEMORY_BYTES,
        "perceptual_optimizer": "experimental_uncalibrated",
        "experimental_formats": ["jpeg", "png"],
        "default_strength_percent": picoli_core::quality::DEFAULT_STRENGTH,
        "strength_range": [0, 100],
        "tinypng_comparison": "not_evaluated"
    })
    .to_string()
}
