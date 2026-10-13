// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common CVM code for the MSHV backend.

use crate::Error;
use crate::ErrorInner;
use crate::KernelError;
use crate::MshvPartitionInner;
use headervec::HeaderVec;
use hvdef::HvPartitionPropertyCode;

#[derive(Debug, Error)]
pub(crate) enum CvmError {
    #[error("launch is already in progress")]
    LaunchInProgress,
    #[error("launch previously failed")]
    LaunchFailed,
    #[error("unsupported initial page import type: {0:?}")]
    UnsupportedPageImportType(virt::InitialPageImportType),
    #[error("invalid initial page range")]
    InvalidPageRange,
    #[error("failed to map guest memory")]
    MapGuestMemory(#[source] KernelError),
    #[error("failed to import pages")]
    ImportIsolatedPages(#[source] KernelError),
    #[error("failed to complete SNP isolated import")]
    CompleteIsolatedImport(#[source] KernelError),
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, inspect::Inspect)]
pub(crate) enum IsolatedLaunchState {
    NotStarted,
    Started,
    Finished,
    Failed,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub(super) struct ImportIsolatedPagesHeader {
    pub(super) page_type: u8,
    pub(super) rsvd: [u8; 7],
    pub(super) page_count: u64,
}

pub const IMPORT_CHUNK_PAGES: usize = 256;

impl MshvPartitionInner {
    pub(super) fn accept_initial_pages_inner_common(
        &self,
        pages: &[virt::InitialPageImport],
    ) -> Result<(), Error> {
        self.vmfd
            .set_partition_property(
                HvPartitionPropertyCode::IsolationState.0,
                mshv_bindings::hv_partition_isolation_state_HV_PARTITION_ISOLATION_SECURE as u64,
            )
            .map_err(|e| ErrorInner::SetPartitionProperty(e.into()))?;

        {
            let mut memory = self.memory.lock();
            for range in memory.ranges.iter_mut().flatten() {
                if !range.mapped {
                    self.vmfd
                        .map_user_memory(range.region)
                        .map_err(|e| CvmError::MapGuestMemory(e.into()))?;
                    range.mapped = true;
                }
            }
        }

        let mut batch_type = None;
        let mut batch = Vec::with_capacity(IMPORT_CHUNK_PAGES);
        for page in ordered_import_pages(pages, true) {
            let Some(page_type) = isolated_page_type(page.import_type)? else {
                continue;
            };
            validate_page_range(page.range)?;
            for pfn in
                page.range.start() / hvdef::HV_PAGE_SIZE..page.range.end() / hvdef::HV_PAGE_SIZE
            {
                if batch_type != Some(page_type) || batch.len() == IMPORT_CHUNK_PAGES {
                    if let Some(current_type) = batch_type {
                        self.import_isolated_pages(current_type, &batch)?;
                        batch.clear();
                    }
                    batch_type = Some(page_type);
                }
                batch.push(pfn);
            }
        }
        if let Some(page_type) = batch_type {
            self.import_isolated_pages(page_type, &batch)?;
        }

        Ok(())
    }

    fn import_isolated_pages(&self, page_type: u8, pfns: &[u64]) -> Result<(), Error> {
        if pfns.is_empty() {
            return Ok(());
        }

        let mut buf =
            HeaderVec::<ImportIsolatedPagesHeader, u64, 0>::new(ImportIsolatedPagesHeader {
                page_type,
                rsvd: [0; 7],
                page_count: pfns.len() as u64,
            });
        buf.extend_tail_from_slice(pfns);
        // SAFETY: The custom header has the same C layout as
        // `mshv_import_isolated_pages`, followed by `page_count` u64 PFNs.
        let args = unsafe {
            &*buf
                .as_ptr()
                .cast::<mshv_bindings::mshv_import_isolated_pages>()
        };
        self.vmfd
            .import_isolated_pages(args)
            .map_err(|e| CvmError::ImportIsolatedPages(e.into()))?;
        Ok(())
    }
}

/// Returns launch pages in the order required by the active launch source.
///
/// IGVM page directives must retain file order so MSHV reproduces the launch
/// digest signed by the ID block. Loader-generated launches have no external
/// measurement order, so they retain the existing deterministic GPA sort.
pub(crate) fn ordered_import_pages(
    pages: &[virt::InitialPageImport],
    preserve_measurement_order: bool,
) -> Vec<virt::InitialPageImport> {
    let mut ordered_pages = pages.to_vec();
    if !preserve_measurement_order {
        ordered_pages.sort_by_key(|page| page.range.start());
    }
    ordered_pages
}

pub(crate) fn isolated_page_type(
    import_type: virt::InitialPageImportType,
) -> Result<Option<u8>, Error> {
    Ok(Some(match import_type {
        virt::InitialPageImportType::Normal => mshv_bindings::MSHV_ISOLATED_PAGE_NORMAL as u8,
        virt::InitialPageImportType::NormalUnmeasured => {
            mshv_bindings::MSHV_ISOLATED_PAGE_UNMEASURED as u8
        }
        virt::InitialPageImportType::VpContext => mshv_bindings::MSHV_ISOLATED_PAGE_VMSA as u8,
        virt::InitialPageImportType::Secrets => mshv_bindings::MSHV_ISOLATED_PAGE_SECRETS as u8,
        virt::InitialPageImportType::Cpuid => mshv_bindings::MSHV_ISOLATED_PAGE_CPUID as u8,
        virt::InitialPageImportType::Shared => return Ok(None),
        virt::InitialPageImportType::CpuidExtendedState => {
            return Err(CvmError::UnsupportedPageImportType(import_type).into());
        }
    }))
}

pub(crate) fn validate_page_range(range: memory_range::MemoryRange) -> Result<(), Error> {
    if range.is_empty()
        || !range.start().is_multiple_of(hvdef::HV_PAGE_SIZE)
        || !range.end().is_multiple_of(hvdef::HV_PAGE_SIZE)
    {
        return Err(CvmError::InvalidPageRange.into());
    }
    Ok(())
}
