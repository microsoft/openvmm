// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::dma::DmaRegion;
use crate::queues::Queues;
use futures::FutureExt;
use gdma_defs::Sge;
use gdma_defs::Wqe;
use gdma_defs::WqeParams;
use guestmem::GuestMemory;
use pal_async::DefaultPool;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::msi::MsiTarget;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;
use task_control::AsyncRun;
use task_control::TaskControl;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

pub const SQ: u64 = 0x1000;
pub const RQ: u64 = 0x2000;
pub const CQ: u64 = 0x3000;
pub const EQ: u64 = 0x4000;

pub fn region(gpa: u64) -> DmaRegion {
    DmaRegion::new(vec![gpa], 0, gdma_defs::PAGE_SIZE64).unwrap()
}

pub fn queues(pool: &DefaultPool, msix: Option<&MsixEmulator>) -> Arc<Queues> {
    let driver = VmTaskDriverSource::new(SingleDriverBackend::new(pool.driver())).simple();
    let (disconnected, _) = MsixEmulator::new(4, 64, &MsiTarget::disconnected());
    Arc::new(Queues::new(
        GuestMemory::allocate(0x10000),
        driver,
        msix.unwrap_or(&disconnected),
    ))
}

pub fn write_wqe(queues: &Queues, gpa: u64, oob: &[u8], address: u64, size: u32) -> u32 {
    let mut wqe = Wqe::new_zeroed();
    let oob_size = match oob.len() {
        0..=8 => gdma_defs::CLIENT_OOB_8,
        9..=24 => gdma_defs::CLIENT_OOB_24,
        25..=32 => gdma_defs::CLIENT_OOB_32,
        _ => panic!("invalid test oob"),
    };
    wqe.header.params = WqeParams::new()
        .with_num_sgl_entries(1)
        .with_inline_client_oob_size(oob_size);
    wqe.data[..oob.len()].copy_from_slice(oob);
    let sge = Sge {
        address,
        mem_key: 0,
        size,
    };
    let offset = wqe.header.sgl_offset();
    wqe.data[offset..offset + size_of::<Sge>()].copy_from_slice(sge.as_bytes());
    let len = wqe.header.total_len();
    queues.gm.write_at(gpa, &wqe.as_bytes()[..len]).unwrap();
    len as u32
}

/// Requests stop from inside a device operation, without yielding its poll.
/// The callback is one-shot; ready work remains finite even without checkpoints.
#[derive(Default)]
pub struct StopHook(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl StopHook {
    pub fn arm<T: AsyncRun<S>, S: Send + 'static>(
        &self,
        control: &Arc<Mutex<TaskControl<T, S>>>,
    ) -> mesh::OneshotReceiver<()> {
        let control = Arc::downgrade(control);
        let (send, recv) = mesh::oneshot();
        *self.0.lock() = Some(Box::new(move || {
            let control = control.upgrade().unwrap();
            assert!(
                control
                    .lock()
                    .poll_stop(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            send.send(());
        }));
        recv
    }

    pub fn fire(&self) {
        let callback = self.0.lock().take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

pub fn stopped<T: AsyncRun<S>, S: Send + 'static>(
    pool: &mut DefaultPool,
    control: &Arc<Mutex<TaskControl<T, S>>>,
    signal: mesh::OneshotReceiver<()>,
) {
    let mut timer = PolledTimer::new(&pool.driver());
    pool.run_until(async {
        let stop = async {
            signal.await.unwrap();
            assert!(poll_fn(|cx| control.lock().poll_stop(cx)).await);
        };
        futures::select! {
            _ = stop.fuse() => {},
            _ = timer.sleep(Duration::from_secs(5)).fuse() => panic!("GDMA worker did not stop"),
        }
    });
}
