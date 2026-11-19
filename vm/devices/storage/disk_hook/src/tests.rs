// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use disk_prwrap::DiskWithReservations;
use futures::poll;
use guestmem::GuestMemory;
use pal_async::async_test;
use parking_lot::Mutex;
use scsi_buffers::OwnedRequestBuffers;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use test_with_tracing::test;

#[async_test]
async fn ram_io_preserves_scatter_gather_data_and_sector_bounds() {
    for sector_size in [512, 2048, 4096] {
        let disk = HookDisk::new(32768, sector_size).into_disk();
        assert_eq!(disk.sector_size(), sector_size);
        assert_eq!(disk.physical_sector_size(), sector_size);
        assert_eq!(disk.sector_count(), 32768 / u64::from(sector_size));
        assert_eq!(disk.disk_id(), None);
        assert!(!disk.is_read_only());
        assert!(disk.is_fua_respected());
        assert_eq!(disk.unmap_behavior(), UnmapBehavior::Unspecified);

        let mem = GuestMemory::allocate(8192);
        let data: Vec<_> = (0..8192).map(|i| (i % 251) as u8).collect();
        mem.write_at(0, &data).unwrap();
        let write = OwnedRequestBuffers::linear(0, 8192, false);
        disk.write_vectored(&write.buffer(&mem), 2, true)
            .await
            .unwrap();
        mem.fill_at(0, 0, 8192).unwrap();
        let read = OwnedRequestBuffers::new(&[1, 0]);
        disk.read_vectored(&read.buffer(&mem), 2).await.unwrap();
        let mut actual = vec![0; 8192];
        mem.read_at(4096, &mut actual[..4096]).unwrap();
        mem.read_at(0, &mut actual[4096..]).unwrap();
        assert_eq!(actual, data);
        disk.sync_cache().await.unwrap();
        storage_tests::sector_range::test_disk_sector_range_conformance(&disk).await;
    }
}

#[async_test]
async fn large_ram_disks_allocate_only_written_sectors() {
    let sector_count = 1 << 40;
    let disk = HookDisk::new(sector_count * 4096, 4096).into_disk();
    assert_eq!(disk.sector_count(), sector_count);
    let mem = GuestMemory::allocate(4096);
    mem.write_at(0, &[0x5a; 4096]).unwrap();
    let write = OwnedRequestBuffers::linear(0, 4096, false);
    disk.write_vectored(&write.buffer(&mem), sector_count - 1, false)
        .await
        .unwrap();
    let read = OwnedRequestBuffers::linear(0, 4096, true);
    disk.read_vectored(&read.buffer(&mem), sector_count - 2)
        .await
        .unwrap();
    let mut actual = [0xff; 4096];
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0; 4096]);
    disk.read_vectored(&read.buffer(&mem), sector_count - 1)
        .await
        .unwrap();
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0x5a; 4096]);
}

#[test]
fn new_rejects_invalid_geometry() {
    for (size, sector_size) in [(0, 512), (513, 512), (4096, 0), (4096, 256), (4096, 768)] {
        assert!(std::panic::catch_unwind(|| HookDisk::new(size, sector_size)).is_err());
    }
}

#[test]
fn into_disk_rejects_invalid_physical_sector_sizes() {
    for physical_sector_size in [0, 256, 768] {
        assert!(
            std::panic::catch_unwind(|| {
                HookDisk::new(4096, 512)
                    .with_physical_sector_size(physical_sector_size)
                    .into_disk()
            })
            .is_err()
        );
    }
}

#[test]
fn metadata_overrides_survive_conversion() {
    let disk = HookDisk::new(8192, 512)
        .with_physical_sector_size(4096)
        .with_disk_id([42; 16])
        .with_read_only(true)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .with_optimal_unmap_sectors(8)
        .into_disk();
    assert_eq!(disk.sector_count(), 16);
    assert_eq!(disk.sector_size(), 512);
    assert_eq!(disk.physical_sector_size(), 4096);
    assert_eq!(disk.disk_id(), Some([42; 16]));
    assert!(disk.is_read_only());
    assert!(!disk.is_fua_respected());
    assert_eq!(disk.unmap_behavior(), UnmapBehavior::Ignored);
    assert_eq!(disk.optimal_unmap_sectors(), 8);
}

#[async_test]
async fn sync_hook_records_exact_operation_parameters() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let hook_operations = operations.clone();
    let disk = HookDisk::new(4096, 512)
        .with_sync_hook(move |operation| {
            hook_operations.lock().push(operation);
            Ok(())
        })
        .into_disk();
    let mem = GuestMemory::allocate(512);
    let write = OwnedRequestBuffers::linear(0, 512, false);
    let read = OwnedRequestBuffers::linear(0, 512, true);
    disk.write_vectored(&write.buffer(&mem), 3, true)
        .await
        .unwrap();
    disk.read_vectored(&read.buffer(&mem), 3).await.unwrap();
    disk.sync_cache().await.unwrap();
    disk.unmap(2, 2, true).await.unwrap();
    assert!(matches!(
        disk.eject().await,
        Err(DiskError::UnsupportedEject)
    ));
    assert_eq!(
        *operations.lock(),
        [
            DiskOperation::Write {
                sector: 3,
                len: 512,
                fua: true
            },
            DiskOperation::Read {
                sector: 3,
                len: 512
            },
            DiskOperation::SyncCache,
            DiskOperation::Unmap {
                sector: 2,
                count: 2,
                block_level_only: true
            },
            DiskOperation::Eject,
        ]
    );
}

#[async_test]
async fn sync_hook_errors_prevent_all_backend_operations() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let backend_operations = operations.clone();
    let inner = HookDisk::new(1024, 512)
        .with_sync_hook(move |operation| {
            backend_operations.lock().push(operation);
            Ok(())
        })
        .into_disk();
    let disk = HookDisk::wrap(inner)
        .with_sync_hook(|_| Err(DiskError::AbortDueToPreemptAndAbort))
        .into_disk();
    let mem = GuestMemory::allocate(512);
    let write = OwnedRequestBuffers::linear(0, 512, false);
    let read = OwnedRequestBuffers::linear(0, 512, true);
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 0, false).await,
        Err(DiskError::AbortDueToPreemptAndAbort)
    ));
    assert!(matches!(
        disk.read_vectored(&read.buffer(&mem), 0).await,
        Err(DiskError::AbortDueToPreemptAndAbort)
    ));
    assert!(matches!(
        disk.sync_cache().await,
        Err(DiskError::AbortDueToPreemptAndAbort)
    ));
    assert!(matches!(
        disk.unmap(0, 1, false).await,
        Err(DiskError::AbortDueToPreemptAndAbort)
    ));
    assert!(matches!(
        disk.eject().await,
        Err(DiskError::AbortDueToPreemptAndAbort)
    ));
    assert!(operations.lock().is_empty());
}

#[async_test]
async fn sync_hook_accepts_non_clone_captures() {
    let calls = Mutex::new(0);
    let disk = HookDisk::new(1024, 512)
        .with_sync_hook(move |_| {
            let mut calls = calls.lock();
            *calls += 1;
            if *calls == 2 {
                return Err(DiskError::ReadOnly);
            }
            Ok(())
        })
        .into_disk();
    disk.sync_cache().await.unwrap();
    assert!(matches!(disk.sync_cache().await, Err(DiskError::ReadOnly)));
    disk.sync_cache().await.unwrap();
}

#[async_test]
async fn async_hook_accepts_non_clone_captures() {
    let calls = Mutex::new(0);
    let disk = HookDisk::new(1024, 512)
        .with_async_hook(move |_| {
            let fail = {
                let mut calls = calls.lock();
                *calls += 1;
                *calls == 2
            };
            async move {
                if fail {
                    Err(DiskError::ReadOnly)
                } else {
                    Ok(())
                }
            }
        })
        .into_disk();
    disk.sync_cache().await.unwrap();
    assert!(matches!(disk.sync_cache().await, Err(DiskError::ReadOnly)));
    disk.sync_cache().await.unwrap();
}

#[async_test]
async fn async_hook_gates_concurrent_calls_before_backend_io() {
    let operations = Arc::new(Mutex::new(Vec::new()));
    let hook_operations = operations.clone();
    let release = Arc::new(event_listener::Event::new());
    let hook_release = release.clone();
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let hook_backend_calls = backend_calls.clone();
    let inner = HookDisk::new(1024, 512)
        .with_sync_hook(move |_| {
            hook_backend_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .into_disk();
    let disk = HookDisk::wrap(inner)
        .with_async_hook(move |operation| {
            let release = hook_release.clone();
            let operations = hook_operations.clone();
            async move {
                let listener = release.listen();
                operations.lock().push(operation);
                listener.await;
                Ok(())
            }
        })
        .into_disk();
    let cloned = disk.clone();
    let mut first = pin!(disk.sync_cache());
    let mut second = pin!(cloned.sync_cache());
    assert!(poll!(&mut first).is_pending());
    assert!(poll!(&mut second).is_pending());
    assert_eq!(*operations.lock(), [DiskOperation::SyncCache; 2]);
    assert_eq!(backend_calls.load(Ordering::Relaxed), 0);
    release.notify(usize::MAX);
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(backend_calls.load(Ordering::Relaxed), 2);
}

#[async_test]
async fn async_hook_failure_leaves_storage_unchanged() {
    let backing = HookDisk::new(1024, 512).into_disk();
    let disk = HookDisk::wrap(backing.clone())
        .with_async_hook(|_| async { Err(DiskError::ReadOnly) })
        .into_disk();
    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &[0x5a; 512]).unwrap();
    let write = OwnedRequestBuffers::linear(0, 512, false);
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 1, false).await,
        Err(DiskError::ReadOnly)
    ));
    let read = OwnedRequestBuffers::linear(0, 512, true);
    backing.read_vectored(&read.buffer(&mem), 1).await.unwrap();
    let mut actual = [0xff; 512];
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0; 512]);
}

#[async_test]
async fn cancelling_async_hook_does_not_issue_backend_io() {
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let hook_backend_calls = backend_calls.clone();
    let backing = HookDisk::new(1024, 512)
        .with_sync_hook(move |_| {
            hook_backend_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .into_disk();
    let disk = HookDisk::wrap(backing)
        .with_async_hook(|_| async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .into_disk();
    {
        let mut flush = pin!(disk.sync_cache());
        assert!(poll!(&mut flush).is_pending());
    }
    assert_eq!(backend_calls.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn last_hook_setter_wins() {
    let disks = [
        HookDisk::new(1024, 512)
            .with_sync_hook(|_| Err(DiskError::ReadOnly))
            .with_async_hook(|_| async { Ok(()) }),
        HookDisk::new(1024, 512)
            .with_async_hook(|_| async { Err(DiskError::ReadOnly) })
            .with_sync_hook(|_| Ok(())),
        HookDisk::new(1024, 512)
            .with_sync_hook(|_| Err(DiskError::ReadOnly))
            .with_sync_hook(|_| Ok(())),
        HookDisk::new(1024, 512)
            .with_async_hook(|_| async { Err(DiskError::ReadOnly) })
            .with_async_hook(|_| std::future::ready(Ok(()))),
    ];
    for disk in disks {
        disk.into_disk().sync_cache().await.unwrap();
    }
}

#[async_test]
async fn cloned_disks_share_storage_and_hooks_without_cloning_captures() {
    let calls = Arc::new(AtomicUsize::new(0));
    let weak = Arc::downgrade(&calls);
    let hook_calls = calls.clone();
    let disk = HookDisk::new(1024, 512)
        .with_sync_hook(move |_| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .into_disk();
    let cloned = disk.clone();
    assert_eq!(Arc::strong_count(&calls), 2);
    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &[0x5a; 512]).unwrap();
    let write = OwnedRequestBuffers::linear(0, 512, false);
    disk.write_vectored(&write.buffer(&mem), 1, false)
        .await
        .unwrap();
    mem.fill_at(0, 0, 512).unwrap();
    let read = OwnedRequestBuffers::linear(0, 512, true);
    cloned.read_vectored(&read.buffer(&mem), 1).await.unwrap();
    let mut actual = [0; 512];
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0x5a; 512]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    drop(calls);
    drop(disk);
    assert!(weak.upgrade().is_some());
    drop(cloned);
    assert!(weak.upgrade().is_none());
}

#[async_test]
async fn unmap_advertising_preserves_backing_behavior_and_ignored_unmaps_skip_hook() {
    for behavior in [
        UnmapBehavior::Ignored,
        UnmapBehavior::Unspecified,
        UnmapBehavior::Zeroes,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = calls.clone();
        let disk = HookDisk::new(1024, 512)
            .with_unmap_behavior(behavior)
            .with_sync_hook(move |operation| {
                if matches!(operation, DiskOperation::Unmap { .. }) {
                    hook_calls.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            })
            .into_disk();
        let mem = GuestMemory::allocate(512);
        mem.write_at(0, &[0x5a; 512]).unwrap();
        let write = OwnedRequestBuffers::linear(0, 512, false);
        disk.write_vectored(&write.buffer(&mem), 1, false)
            .await
            .unwrap();
        disk.unmap(1, 1, false).await.unwrap();
        let read = OwnedRequestBuffers::linear(0, 512, true);
        disk.read_vectored(&read.buffer(&mem), 1).await.unwrap();
        let mut actual = [0; 512];
        mem.read_at(0, &mut actual).unwrap();
        if behavior == UnmapBehavior::Ignored {
            assert_eq!(actual, [0x5a; 512]);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
        } else {
            assert_eq!(actual, [0; 512]);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
        assert!(matches!(
            disk.unmap(2, 1, false).await,
            Err(DiskError::IllegalBlock)
        ));
    }
}

#[async_test]
async fn read_only_override_prevents_mutation() {
    let backing = HookDisk::new(1024, 512).into_disk();
    let disk = HookDisk::wrap(backing.clone())
        .with_read_only(true)
        .into_disk();
    let mem = GuestMemory::allocate(512);
    mem.write_at(0, &[0x5a; 512]).unwrap();
    let write = OwnedRequestBuffers::linear(0, 512, false);
    backing
        .write_vectored(&write.buffer(&mem), 0, false)
        .await
        .unwrap();
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 1, false).await,
        Err(DiskError::ReadOnly)
    ));
    assert!(matches!(
        disk.unmap(0, 1, false).await,
        Err(DiskError::ReadOnly)
    ));
    let read = OwnedRequestBuffers::linear(0, 512, true);
    disk.read_vectored(&read.buffer(&mem), 0).await.unwrap();
    let mut actual = [0; 512];
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0x5a; 512]);
    disk.read_vectored(&read.buffer(&mem), 1).await.unwrap();
    mem.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [0; 512]);

    let disk = HookDisk::wrap(disk).with_read_only(false).into_disk();
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 1, false).await,
        Err(DiskError::ReadOnly)
    ));
}

#[async_test]
async fn wrapper_forwards_metadata_and_reservations() {
    let backing = HookDisk::new(8192, 512)
        .with_physical_sector_size(4096)
        .with_disk_id([42; 16])
        .with_read_only(true)
        .with_fua_respected(false)
        .with_unmap_behavior(UnmapBehavior::Ignored)
        .with_optimal_unmap_sectors(8)
        .into_disk();
    let backing = Disk::new(DiskWithReservations::new(backing)).unwrap();
    let disk = HookDisk::wrap(backing.clone()).into_disk();
    assert_eq!(disk.sector_count(), backing.sector_count());
    assert_eq!(disk.sector_size(), backing.sector_size());
    assert_eq!(disk.physical_sector_size(), backing.physical_sector_size());
    assert_eq!(disk.disk_id(), backing.disk_id());
    assert_eq!(disk.is_read_only(), backing.is_read_only());
    assert_eq!(disk.is_fua_respected(), backing.is_fua_respected());
    assert_eq!(disk.unmap_behavior(), backing.unmap_behavior());
    assert_eq!(
        disk.optimal_unmap_sectors(),
        backing.optimal_unmap_sectors()
    );
    assert!(std::ptr::eq(disk.pr().unwrap(), backing.pr().unwrap()));
}

#[async_test]
async fn wrapper_propagates_backend_errors() {
    let backing = HookDisk::new(1024, 512)
        .with_sync_hook(|_| Err(DiskError::ReservationConflict))
        .into_disk();
    let disk = HookDisk::wrap(backing).into_disk();
    let mem = GuestMemory::allocate(512);
    let write = OwnedRequestBuffers::linear(0, 512, false);
    let read = OwnedRequestBuffers::linear(0, 512, true);
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 0, false).await,
        Err(DiskError::ReservationConflict)
    ));
    assert!(matches!(
        disk.read_vectored(&read.buffer(&mem), 0).await,
        Err(DiskError::ReservationConflict)
    ));
    assert!(matches!(
        disk.sync_cache().await,
        Err(DiskError::ReservationConflict)
    ));
    assert!(matches!(
        disk.unmap(0, 1, false).await,
        Err(DiskError::ReservationConflict)
    ));
    assert!(matches!(
        disk.eject().await,
        Err(DiskError::ReservationConflict)
    ));
}

#[async_test]
async fn guest_memory_errors_propagate() {
    let disk = HookDisk::new(1024, 512).into_disk();
    let mem = GuestMemory::empty();
    let write = OwnedRequestBuffers::linear(0, 512, false);
    let read = OwnedRequestBuffers::linear(0, 512, true);
    assert!(matches!(
        disk.read_vectored(&read.buffer(&mem), 0).await,
        Err(DiskError::MemoryAccess(_))
    ));
    assert!(matches!(
        disk.write_vectored(&write.buffer(&mem), 0, false).await,
        Err(DiskError::MemoryAccess(_))
    ));
}

#[derive(Inspect)]
#[inspect(skip)]
struct ResizingDisk(Arc<ResizeState>);

struct ResizeState {
    sector_count: AtomicU64,
    changed: event_listener::Event,
}

impl DiskIo for ResizingDisk {
    fn disk_type(&self) -> &str {
        "resize-test"
    }

    fn sector_count(&self) -> u64 {
        self.0.sector_count.load(Ordering::Relaxed)
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn physical_sector_size(&self) -> u32 {
        512
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }

    async fn read_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
    ) -> Result<(), DiskError> {
        Err(DiskError::InvalidInput)
    }

    async fn write_vectored(
        &self,
        _buffers: &RequestBuffers<'_>,
        _sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::InvalidInput)
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Err(DiskError::InvalidInput)
    }

    async fn wait_resize(&self, sector_count: u64) -> u64 {
        loop {
            let listener = self.0.changed.listen();
            let current = self.sector_count();
            if current != sector_count {
                return current;
            }
            listener.await;
        }
    }
}

#[async_test]
async fn wrapper_forwards_resize_notifications_through_disk_adapter() {
    let state = Arc::new(ResizeState {
        sector_count: AtomicU64::new(2),
        changed: event_listener::Event::new(),
    });
    let backing = Disk::new(ResizingDisk(state.clone())).unwrap();
    let disk = HookDisk::wrap(backing).into_disk();
    let cloned = disk.clone();
    let mut resize = pin!(disk.wait_resize(2));
    assert!(poll!(&mut resize).is_pending());
    state.sector_count.store(4, Ordering::Relaxed);
    state.changed.notify(usize::MAX);
    assert_eq!(resize.await, 4);
    assert_eq!(cloned.sector_count(), 4);
    assert_eq!(cloned.wait_resize(2).await, 4);
}
