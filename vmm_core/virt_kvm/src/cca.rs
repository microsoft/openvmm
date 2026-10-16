// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arm CCA Realm population support for KVM partitions.
//!
//! Loader-provided private pages are copied from their userspace mappings into
//! Realm memory with `KVM_ARM_RMI_POPULATE`. KVM may report partial progress by
//! updating the ioctl argument, so population continues until each range is
//! complete. Runtime RIPAS changes are handled by the memory module.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use crate::memory::private_memory_range_from_slots;
use virt::InitialPageImportType;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
/// Progress of the one-shot Realm initial population sequence.
pub(crate) enum CcaLaunchState {
    /// No population request has been issued.
    NotStarted,
    /// Population is in progress.
    Populating,
    /// Every initial private range was populated successfully.
    Populated,
    /// Population failed and cannot be retried on this partition.
    Failed,
}

impl virt::AcceptInitialPages for KvmPartition {
    type Error = KvmError;

    fn accept_initial_pages(&self, pages: &[virt::InitialPageImport]) -> Result<(), Self::Error> {
        self.inner.cca_populate_initial_pages(pages)
    }
}

impl KvmPartitionInner {
    /// Populates initial Realm pages once and records the terminal state.
    fn cca_populate_initial_pages(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        {
            let mut state = self.cca_launch_state.lock();
            match *state {
                CcaLaunchState::NotStarted => *state = CcaLaunchState::Populating,
                CcaLaunchState::Populating => return Err(KvmError::CcaPopulateInProgress),
                CcaLaunchState::Populated => return Ok(()),
                CcaLaunchState::Failed => return Err(KvmError::CcaPopulateFailed),
            }
        }

        tracing::info!(page_ranges = pages.len(), "starting CCA initial population");
        match self.cca_populate_initial_pages_inner(pages) {
            Ok(()) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Populated;
                tracing::info!("finished CCA initial population");
                Ok(())
            }
            Err(err) => {
                *self.cca_launch_state.lock() = CcaLaunchState::Failed;
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed CCA initial population"
                );
                Err(err)
            }
        }
    }

    /// Populates all supported initial imports, honoring partial ioctl progress.
    fn cca_populate_initial_pages_inner(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), KvmError> {
        crate::memory::check_private_memory_extensions(
            &self.kvm,
            crate::memory::KvmGuestMemfdPrivateState::GuestMemfdDefault,
        )
        .map_err(map_cca_capability_error)?;

        let pages = pages.to_vec();

        let memory = self.memory.lock();
        for page in &pages {
            let flags = cca_populate_flags(page.import_type)?;
            let private_range = private_memory_range_from_slots(page.range, &memory.ranges)
                .map_err(map_cca_private_range_error)?;
            let segments = crate::memory::guest_memfd_range_segments(page.range, &memory.ranges)
                .map_err(map_cca_private_range_error)?;
            let mut populate = kvm::KvmArmRmiPopulate {
                base: private_range.gpa.start(),
                size: private_range.gpa.len(),
                source_uaddr: private_range.hva as u64,
                flags,
                reserved: 0,
            };

            while populate.size != 0 {
                let previous = populate;
                tracing::trace!(
                    gpa = populate.base,
                    len = populate.size,
                    source_uaddr = populate.source_uaddr,
                    flags = populate.flags,
                    import_type = ?page.import_type,
                    tag = page.tag,
                    "KVM_ARM_RMI_POPULATE"
                );
                self.kvm.arm_rmi_populate(&mut populate)?;
                if populate.size >= previous.size {
                    return Err(KvmError::CcaPopulateNoProgress);
                }
            }
            self.discard_stale_private_memory_backing(&segments, true, "CCA initial population")?;
        }

        Ok(())
    }
}

/// Returns the RMI populate flags for a loader import type.
fn cca_populate_flags(import_type: InitialPageImportType) -> Result<u32, KvmError> {
    match import_type {
        InitialPageImportType::Normal => Ok(kvm::KVM_ARM_RMI_POPULATE_FLAGS_MEASURE_UAPI),
        InitialPageImportType::NormalUnmeasured => Ok(0),
        InitialPageImportType::Shared
        | InitialPageImportType::VpContext
        | InitialPageImportType::Secrets
        | InitialPageImportType::Cpuid
        | InitialPageImportType::CpuidExtendedState => {
            Err(KvmError::UnsupportedCcaPageImportType(import_type))
        }
    }
}

/// Converts a generic private-slot lookup failure into a CCA population error.
fn map_cca_private_range_error(err: crate::memory::MemoryError) -> KvmError {
    match err {
        crate::memory::MemoryError::InvalidPrivateMemoryRange => KvmError::InvalidCcaPopulateRange,
        err => err.into(),
    }
}

/// Converts generic memory-conversion validation errors into CCA exit errors.
pub(crate) fn map_cca_conversion_error(err: crate::memory::MemoryError) -> KvmError {
    match err {
        crate::memory::MemoryError::InvalidMapGpaRange => KvmError::InvalidCcaMemoryFault,
        err => err.into(),
    }
}

/// Converts private-memory capability errors into CCA-specific errors.
fn map_cca_capability_error(err: crate::memory::MemoryError) -> KvmError {
    match err {
        crate::memory::MemoryError::Kvm(kvm::Error::MissingCapability(capability)) => {
            KvmError::MissingCcaCapability(capability)
        }
        err => err.into(),
    }
}
