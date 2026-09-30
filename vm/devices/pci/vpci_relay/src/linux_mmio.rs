// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO access types for Linux environments.

use crate::CreateMemoryAccess;
use anyhow::Context as _;
use hcl::ioctl::MshvHvcall;
use std::sync::Arc;
use vpci_client::MemoryAccess;

/// Accesses MMIO space directly via `/dev/mem`.
pub struct DirectMmio(fs_err::File);

impl DirectMmio {
    /// Opens `/dev/mem` for MMIO access.
    pub fn new() -> anyhow::Result<Self> {
        let dev_mem = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mem")
            .context("failed to open /dev/mem")?;
        Ok(Self(dev_mem))
    }
}

impl CreateMemoryAccess for DirectMmio {
    fn create_memory_access(&self, gpa: u64) -> anyhow::Result<Box<dyn MemoryAccess>> {
        let mapping = sparse_mmap::SparseMapping::new(0x2000)
            .context("failed to create sparse mapping for vpci mmio")?;
        mapping
            .map_file(0, 0x2000, &self.0, gpa, true)
            .context("failed to map /dev/mem for vpci mmio")?;

        Ok(Box::new(DirectMmioInstance(gpa, mapping)))
    }
}

struct DirectMmioInstance(u64, sparse_mmap::SparseMapping);

impl MemoryAccess for DirectMmioInstance {
    fn gpa(&mut self) -> u64 {
        self.0
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) {
        let offset = addr
            .checked_sub(self.gpa())
            .and_then(|o| o.try_into().ok())
            .unwrap_or(!0);
        let res = match data.len() {
            1 => self
                .1
                .read_volatile::<[u8; 1]>(offset)
                .map(|v| data.copy_from_slice(&v)),
            2 => self
                .1
                .read_volatile::<[u8; 2]>(offset)
                .map(|v| data.copy_from_slice(&v)),
            4 => self
                .1
                .read_volatile::<[u8; 4]>(offset)
                .map(|v| data.copy_from_slice(&v)),
            _ => panic!("size must be 1-, 2-, or 4-bytes"),
        };
        if let Err(err) = res {
            tracelimit::error_ratelimited!(
                addr,
                error = &err as &dyn std::error::Error,
                "vpci mmio read failure"
            );
            data.fill(!0);
        }
    }

    fn write(&mut self, addr: u64, value: &[u8]) {
        let offset = addr
            .checked_sub(self.gpa())
            .and_then(|o| o.try_into().ok())
            .unwrap_or(!0);
        let res = match value.len() {
            1 => self
                .1
                .write_volatile(offset, &<[u8; 1]>::try_from(value).unwrap()),
            2 => self
                .1
                .write_volatile(offset, &<[u8; 2]>::try_from(value).unwrap()),
            4 => self
                .1
                .write_volatile(offset, &<[u8; 4]>::try_from(value).unwrap()),
            _ => panic!("size must be 1-, 2-, or 4-bytes"),
        };
        if let Err(err) = res {
            tracelimit::error_ratelimited!(
                addr,
                value,
                error = &err as &dyn std::error::Error,
                "vpci mmio write failure"
            );
        }
    }
}

/// MMIO access via hypercalls.
pub struct HypercallMmio(Arc<MshvHvcall>);

impl HypercallMmio {
    /// Opens a hypercall interface for MMIO access.
    pub fn new() -> anyhow::Result<Self> {
        let mshv_hvcall = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv_hvcall.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallMemoryMappedIoRead,
            hvdef::HypercallCode::HvCallMemoryMappedIoWrite,
        ]);
        Ok(Self(Arc::new(mshv_hvcall)))
    }
}

impl CreateMemoryAccess for HypercallMmio {
    fn create_memory_access(&self, gpa: u64) -> anyhow::Result<Box<dyn MemoryAccess>> {
        Ok(Box::new(HypercallMmioInstance(gpa, self.0.clone())))
    }
}

struct HypercallMmioInstance(u64, Arc<MshvHvcall>);

impl MemoryAccess for HypercallMmioInstance {
    fn gpa(&mut self) -> u64 {
        self.0
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) {
        if let Err(err) = self.1.mmio_read(addr, data) {
            tracelimit::error_ratelimited!(
                addr,
                error = &err as &dyn std::error::Error,
                "vpci mmio read failure"
            );
            data.fill(!0);
        }
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        if let Err(err) = self.1.mmio_write(addr, data) {
            tracelimit::error_ratelimited!(
                addr,
                data,
                error = &err as &dyn std::error::Error,
                "vpci mmio write failure"
            );
        }
    }
}
