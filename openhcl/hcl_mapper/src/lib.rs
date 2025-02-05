// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a mapper implementation for the page pool that uses [`MshvVtlLow`].

use anyhow::Context;
use hcl::ioctl::MshvVtlLow;
use hvdef::HV_PAGE_SIZE;
use page_pool_alloc::Mapper;
use page_pool_alloc::PoolType;
use sparse_mmap::SparseMapping;

/// A mapper that uses [`MshvVtlLow`] to map pages.
#[derive(Debug)]
pub struct HclMapper;

impl Mapper for HclMapper {
    fn map(
        &self,
        base_pfn: u64,
        size_pages: u64,
        pool_type: PoolType,
    ) -> Result<SparseMapping, anyhow::Error> {
        let len = (size_pages * HV_PAGE_SIZE) as usize;
        let gpa_fd = MshvVtlLow::new().context("failed to open gpa fd")?;
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
            .map_file(0, len, gpa_fd.get(), file_offset, true)
            .context("unable to map allocation")?;

        Ok(mapping)
    }
}
