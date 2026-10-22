// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::BindingReportState;
use super::EnumerationResult;
use super::Vtl2DeviceState;
use super::discard_pending_mana_buffers;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::TestMapper;
use std::sync::Arc;
use test_with_tracing::test;
use user_driver::DmaClient;
use user_driver::vfio::VfioDmaClients;
use vmcore::save_restore::SaveRestore;

const PAGE_SIZE: usize = 4096;
const GDMA_BUFFER_SIZE: usize = 6 * PAGE_SIZE;

fn test_pool() -> PagePool {
    PagePool::new(
        &[MemoryRange::from_4k_gpn_range(0..128)],
        TestMapper::new(128).unwrap(),
    )
    .unwrap()
}

#[test]
fn discard_pending_mana_buffers_releases_restored_allocations() {
    let mut pool = test_pool();
    let persistent = Arc::new(pool.allocator("nic_0000:00:00.0".into()).unwrap());
    let gdma_buffer = persistent.allocate_dma_buffer(GDMA_BUFFER_SIZE).unwrap();
    let original_base_pfn = gdma_buffer.pfns()[0];

    // Underhill snapshots persistent DMA allocations when keepalive is
    // enabled, before MANA decides that there is no device state to save.
    let state = pool.save().unwrap();

    let mut restored_pool = test_pool();
    restored_pool.restore(state).unwrap();
    let persistent: Arc<dyn DmaClient> =
        Arc::new(restored_pool.allocator("nic_0000:00:00.0".into()).unwrap());
    let ephemeral: Arc<dyn DmaClient> = Arc::new(
        restored_pool
            .allocator("nic_0000:00:00.0_ephemeral".into())
            .unwrap(),
    );
    let dma_clients = VfioDmaClients::Split {
        persistent: persistent.clone(),
        ephemeral,
    };

    discard_pending_mana_buffers(&dma_clients).unwrap();

    let replacement = persistent.allocate_dma_buffer(GDMA_BUFFER_SIZE).unwrap();
    assert_eq!(replacement.pfns()[0], original_base_pfn);
    restored_pool.validate_restore(false).unwrap();
}

#[test]
fn enumeration_after_startup_from_missing_is_accepted() {
    let shutdown_active = false;
    let mut state = Vtl2DeviceState::Missing;
    state.device_started();
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::ReportBinding,
    );
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );

    // A duplicate enumeration request does not report another binding.
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::Ignored
    );
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );
}

#[test]
fn enumeration_before_startup_from_missing_is_accepted() {
    let shutdown_active = false;
    let mut state = Vtl2DeviceState::Missing;
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::WaitForDevice
    );
    assert_eq!(state, Vtl2DeviceState::DeviceEnumerated);
    state.device_started();
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );

    // A duplicate enumeration request does not report another binding.
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::Ignored
    );
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );
}

#[test]
fn enumeration_in_initial_present_is_ignored() {
    let shutdown_active = false;
    let mut state = Vtl2DeviceState::Present {
        binding_report: BindingReportState::Done,
    };
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::Ignored
    );
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );
}

#[test]
fn enumeration_in_reconfiguring_is_ignored() {
    let shutdown_active = false;
    let mut state = Vtl2DeviceState::Reconfiguring;
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::Ignored
    );
    assert_eq!(state, Vtl2DeviceState::Reconfiguring);
    // Finish Reconfiguring
    state.device_started();
    assert_eq!(
        state,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        }
    );
    assert_eq!(
        state.device_enumerated(shutdown_active),
        EnumerationResult::Ignored
    );
}

#[test]
fn enumeration_during_shutdown_is_ignored() {
    let shutdown_active = true;
    for original in [
        Vtl2DeviceState::DeviceEnumerated,
        Vtl2DeviceState::Missing,
        Vtl2DeviceState::Reconfiguring,
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::WaitingForEnumeration,
        },
        Vtl2DeviceState::Present {
            binding_report: BindingReportState::Done,
        },
    ] {
        let mut state = original;
        assert_eq!(
            state.device_enumerated(shutdown_active),
            EnumerationResult::Ignored
        );
        assert_eq!(state, original);
    }
}
