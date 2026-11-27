use serde::Serialize;
use std::error::Error;
use sysinfo::System;

#[derive(Debug, Serialize)]
pub(super) struct HostMemory {
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub budget_bytes: u64,
    pub automatic: bool,
}

pub(super) fn memory_budget(requested_mib: Option<usize>) -> Result<HostMemory, Box<dyn Error>> {
    let mut system = System::new();
    system.refresh_memory();
    let available_bytes = system.available_memory();
    let total_bytes = system.total_memory();
    let budget_bytes = match requested_mib {
        Some(mib) => {
            let requested = mib as u64 * 1024 * 1024;
            if available_bytes > 0 {
                requested.min(available_bytes / 2)
            } else {
                requested
            }
        }
        None => (available_bytes / 2)
            .min(total_bytes / 4)
            .min(2 * 1024 * 1024 * 1024),
    };
    if budget_bytes < 64 * 1024 * 1024 {
        return Err("insufficient available memory or host memory cannot be detected; use an explicit safe --memory-mib budget".into());
    }
    Ok(HostMemory {
        available_bytes,
        total_bytes,
        budget_bytes,
        automatic: requested_mib.is_none(),
    })
}
