// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use hvdef::Vtl;
use inspect::InspectMut;
use memory_range::MemoryRange;
use std::sync::Arc;
use virt::PageVisibility;
use virt_mshv_vtl::UhPartition;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmm_core::partition_unit::VmPartition;

/// Wraps `Arc<UhPartition>` and implements [`VmPartition`].
#[derive(InspectMut)]
#[inspect(transparent)]
pub struct WrappedPartition(pub Arc<UhPartition>);

#[async_trait]
impl VmPartition for WrappedPartition {
    fn reset(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("reset not supported")
    }

    fn scrub_vtl(&mut self, _vtl: Vtl) -> anyhow::Result<()> {
        unreachable!()
    }

    fn accept_initial_pages(
        &mut self,
        _pages: Vec<(MemoryRange, PageVisibility)>,
    ) -> anyhow::Result<()> {
        unreachable!()
    }
}

impl SaveRestore for WrappedPartition {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}
