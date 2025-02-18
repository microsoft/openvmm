// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a mapper implementation for the page pool that uses the hcl ioctl
//! crate to map guest memory.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

use anyhow::Context;
use hcl::ioctl::MshvVtlLow;
use inspect::Inspect;
use page_pool_alloc::PoolSource;
use std::os::fd::AsFd;

/// A mapper that uses [`MshvVtlLow`] to map pages.
#[derive(Inspect)]
pub struct HclMapper {
    #[inspect(skip)]
    fd: MshvVtlLow,
    gpa_bias: u64,
    is_shared: bool,
}

impl HclMapper {
    /// Creates an instance for mapping shared memory, with shared memory
    /// appearing to the guest starting at `vtom`.
    pub fn new_shared(vtom: u64) -> anyhow::Result<Self> {
        Self::new_inner(vtom, true)
    }

    /// Creates an instance for mapping private memory.
    pub fn new_private() -> anyhow::Result<Self> {
        Self::new_inner(0, false)
    }

    fn new_inner(gpa_bias: u64, is_shared: bool) -> anyhow::Result<Self> {
        let fd = MshvVtlLow::new().context("failed to open gpa fd")?;
        Ok(Self {
            fd,
            gpa_bias,
            is_shared,
        })
    }
}

impl PoolSource for HclMapper {
    fn address_bias(&self) -> u64 {
        self.gpa_bias
    }

    fn mapping_file_offset(&self, address: u64) -> usize {
        address.wrapping_add(if self.is_shared {
            MshvVtlLow::SHARED_MEMORY_FLAG
        } else {
            0
        }) as usize
    }

    fn mappable(&self) -> sparse_mmap::MappableRef<'_> {
        self.fd.get().as_fd()
    }
}
