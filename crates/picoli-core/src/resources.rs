//! Working-set estimates rather than a fixed megapixel admission rule.
use crate::Error;
use serde::Serialize;

/// Conservative fallback for hosts that request the fixed baseline budget.
pub const DEFAULT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CompressionMemoryPlan {
    pub width: u32,
    pub height: u32,
    pub format: &'static str,
    pub required_memory_bytes: u64,
    pub memory_mib: u32,
}

impl CompressionMemoryPlan {
    pub(crate) fn with_metadata(
        width: u32,
        height: u32,
        format: &'static str,
        input_bytes: usize,
        metadata_bytes: usize,
    ) -> Result<Self, Error> {
        let plan = Plan::with_metadata(width, height, input_bytes, metadata_bytes, u64::MAX, 1)?;
        let required_memory_bytes = plan
            .estimated_shared_bytes
            .checked_add(plan.estimated_worker_bytes)
            .ok_or(Error::Limit("working-set estimate overflow"))?;
        let mib = required_memory_bytes.div_ceil(1024 * 1024);
        let tier = mib
            .checked_next_power_of_two()
            .ok_or(Error::Limit("working-set estimate overflow"))?
            .max(128);
        let memory_mib =
            u32::try_from(tier).map_err(|_| Error::Limit("working-set estimate overflow"))?;
        Ok(Self {
            width,
            height,
            format,
            required_memory_bytes,
            memory_mib,
        })
    }

    pub fn budget_bytes(self) -> Option<u64> {
        u64::from(self.memory_mib).checked_mul(1024 * 1024)
    }
}

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
    fn automatic_plan_scales_adaptively_without_artificial_1024_cap() {
        assert_eq!(DEFAULT_MEMORY_BYTES, 128 * 1024 * 1024);
        assert_eq!(
            CompressionMemoryPlan::with_metadata(1, 1, "png", 100, 0)
                .unwrap()
                .memory_mib,
            128
        );
        for (width, tier) in [
            (2000, 128u32),
            (4000, 256),
            (9000, 512),
            (19000, 1024),
            (38000, 2048),
        ] {
            let base = Plan::new(width, 1000, 0, u64::MAX, 1).unwrap();
            let input_bytes = ((u64::from(tier) * 1024 * 1024
                - base.estimated_shared_bytes
                - base.estimated_worker_bytes)
                / 2) as usize;
            let plan =
                CompressionMemoryPlan::with_metadata(width, 1000, "png", input_bytes, 0).unwrap();
            assert_eq!(plan.memory_mib, tier);
            assert_eq!(plan.required_memory_bytes, u64::from(tier) * 1024 * 1024);
            let above =
                CompressionMemoryPlan::with_metadata(width, 1000, "png", input_bytes + 1, 0);
            assert_eq!(above.unwrap().memory_mib, tier * 2);
        }
    }

    #[test]
    fn automatic_plan_covers_real_high_res_inputs_and_retained_metadata() {
        let plan = CompressionMemoryPlan::with_metadata(4464, 3173, "png", 6_212_113, 0).unwrap();
        assert_eq!(plan.required_memory_bytes, 809_817_762);
        assert_eq!(plan.memory_mib, 1024);
        let plan_25mp = CompressionMemoryPlan::with_metadata(5000, 5000, "png", 0, 0).unwrap();
        assert_eq!(plan_25mp.memory_mib, 2048);
        let plan_48mp = CompressionMemoryPlan::with_metadata(8064, 6048, "jpeg", 0, 0).unwrap();
        assert_eq!(plan_48mp.memory_mib, 4096);
        let metadata =
            CompressionMemoryPlan::with_metadata(1000, 1000, "jpeg", 1000, 1024).unwrap();
        let expected = Plan::with_metadata(1000, 1000, 1000, 1024, u64::MAX, 1).unwrap();
        assert_eq!(
            metadata.required_memory_bytes,
            expected.estimated_shared_bytes + expected.estimated_worker_bytes
        );
    }

    #[test]
    fn automatic_plan_rejects_zero_and_overflow() {
        for (width, height) in [(0, 1), (1, 0)] {
            assert!(matches!(
                CompressionMemoryPlan::with_metadata(width, height, "png", 0, 0),
                Err(Error::Argument(_))
            ));
        }
        for (width, height, input, metadata) in [
            (u32::MAX, u32::MAX, 0, 0),
            (1, 1, usize::MAX, 0),
            (1, 1, 0, usize::MAX),
        ] {
            assert!(matches!(
                CompressionMemoryPlan::with_metadata(width, height, "png", input, metadata),
                Err(Error::Limit(_))
            ));
        }
    }

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
