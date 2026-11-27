//! Working-set estimates rather than a fixed megapixel admission rule.
use crate::Error;
use serde::Serialize;

/// Conservative fallback for hosts without an available-memory API. Native CLI
/// supplies its OS-derived budget; a WASM host can supply a larger tested budget.
pub const DEFAULT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Plan {
    pub memory_budget_bytes: u64,
    pub pixels: u64,
    pub estimated_shared_bytes: u64,
    pub estimated_worker_bytes: u64,
    pub workers: usize,
}

impl Plan {
    pub fn new(
        width: u32,
        height: u32,
        input_bytes: usize,
        memory_budget_bytes: u64,
        requested_workers: usize,
    ) -> Result<Self, Error> {
        Self::with_metadata(
            width,
            height,
            input_bytes,
            0,
            memory_budget_bytes,
            requested_workers,
        )
    }

    pub fn with_metadata(
        width: u32,
        height: u32,
        input_bytes: usize,
        metadata_bytes: usize,
        memory_budget_bytes: u64,
        requested_workers: usize,
    ) -> Result<Self, Error> {
        if width == 0 || height == 0 || requested_workers == 0 {
            return Err(Error::Argument(
                "dimensions and worker count must be positive",
            ));
        }
        let pixels = u64::from(width) * u64::from(height);
        let estimated_shared_bytes = pixels
            .checked_mul(24)
            .and_then(|n| n.checked_add((input_bytes as u64).checked_mul(2)?))
            .and_then(|n| n.checked_add((metadata_bytes as u64).checked_mul(32)?))
            .ok_or(Error::Limit("shared working-set estimate overflow"))?;
        let estimated_worker_bytes = pixels
            .checked_mul(32)
            .and_then(|n| n.checked_add(4 * 1024 * 1024))
            .and_then(|n| n.checked_add((metadata_bytes as u64).checked_mul(16)?))
            .ok_or(Error::Limit("worker working-set estimate overflow"))?;
        let available = memory_budget_bytes.saturating_sub(estimated_shared_bytes);
        let workers = (available / estimated_worker_bytes).min(requested_workers as u64) as usize;
        if workers == 0 {
            return Err(Error::Limit(
                "estimated decode/analysis/search memory exceeds host budget",
            ));
        }
        Ok(Self {
            memory_budget_bytes,
            pixels,
            estimated_shared_bytes,
            estimated_worker_bytes,
            workers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrency_adapts_without_a_fixed_pixel_cutoff() {
        assert_eq!(
            Plan::new(1000, 1000, 1000, 256 * 1024 * 1024, 4)
                .unwrap()
                .workers,
            4
        );
        assert_eq!(
            Plan::new(1000, 1000, 1000, 64 * 1024 * 1024, 4)
                .unwrap()
                .workers,
            1
        );
        assert!(Plan::new(4464, 3173, 6_212_113, 2 * 1024 * 1024 * 1024, 4).is_ok());
        assert!(Plan::new(4464, 3173, 6_212_113, 128 * 1024 * 1024, 4).is_err());
        assert!(Plan::new(0, 1, 0, u64::MAX, 1).is_err());
    }
}
