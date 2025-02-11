// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a mapper implementation for the page pool that uses the hcl ioctl
//! crate to map guest memory.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

use anyhow::Context;
use hcl::ioctl::MshvVtlLow;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::Response;
use page_pool_alloc::Mapper;
use page_pool_alloc::PoolType;
use sparse_mmap::SparseMapping;

/// A mapper that uses [`MshvVtlLow`] to map pages.
#[derive(Inspect)]
#[inspect(extra = "HclMapper::inspect_extra")]
pub struct HclMapper {
    #[inspect(skip)]
    fd: MshvVtlLow,
}

impl HclMapper {
    /// Creates a new [`HclMapper`].
    pub fn new() -> Result<Self, anyhow::Error> {
        let fd = MshvVtlLow::new().context("failed to open gpa fd")?;
        Ok(Self { fd })
    }

    fn inspect_extra(&self, resp: &mut Response<'_>) {
        resp.field("type", "hcl_mapper");
    }
}

impl Mapper for HclMapper {
    fn map(
        &self,
        base_pfn: u64,
        size_pages: u64,
        pool_type: PoolType,
    ) -> Result<SparseMapping, anyhow::Error> {
        let len = (size_pages * HV_PAGE_SIZE) as usize;
        let mapping = SparseMapping::new(len).context("failed to create mapping")?;
        let gpa = base_pfn * HV_PAGE_SIZE;

        // When the pool references shared memory, on hardware isolated
        // platforms the file_offset must have the shared bit set as these
        // are decrypted pages. Setting this bit is okay on non-hardware
        // isolated platforms, as it does nothing.
        let file_offset = match pool_type {
            PoolType::Private => gpa,
            PoolType::Shared => {
                tracing::trace!("setting MshvVtlLow::SHARED_MEMORY_FLAG");
                gpa | MshvVtlLow::SHARED_MEMORY_FLAG
            }
        };

        tracing::trace!(gpa, file_offset, len, "mapping allocation");

        mapping
            .map_file(0, len, self.fd.get(), file_offset, true)
            .context("unable to map allocation")?;

        Ok(mapping)
    }
}
