// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deterministic cancellation fixtures for channel workers.

use crate::RawAsyncChannel;
use futures::FutureExt;
use pal_async::DefaultDriver;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc;
use std::time::Duration;
use task_control::AsyncRun;
use task_control::TaskControl;
use vmbus_ring::CONTROL_WORD_COUNT;
use vmbus_ring::FlatRingMem;
use vmbus_ring::RingMem;

/// Ring memory that gates the host's first packet read.
#[derive(Clone)]
pub struct GatedRingMem {
    mem: FlatRingMem,
    gate: Option<Arc<Gate>>,
}

struct Gate {
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

/// Controls the first read without relying on executor scheduling.
pub struct ReadGate {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    notify: mpsc::Sender<()>,
    gate: Arc<Gate>,
}

/// Creates channels whose host pauses while reading its first incoming packet.
pub fn gated_channels(
    size: usize,
) -> (
    RawAsyncChannel<GatedRingMem>,
    RawAsyncChannel<GatedRingMem>,
    ReadGate,
) {
    let (entered, recv_entered) = mpsc::channel();
    let (release, recv_release) = mpsc::channel();
    let gate = Arc::new(Gate {
        entered: Mutex::new(Some(entered.clone())),
        release: Mutex::new(recv_release),
    });
    let incoming = GatedRingMem {
        mem: FlatRingMem::new(size),
        gate: Some(gate.clone()),
    };
    let outgoing = GatedRingMem {
        mem: FlatRingMem::new(size),
        gate: None,
    };
    let (host, guest) = crate::connected_channels(incoming, outgoing);
    (
        host,
        guest,
        ReadGate {
            entered: recv_entered,
            release,
            notify: entered,
            gate,
        },
    )
}

impl RingMem for GatedRingMem {
    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        self.mem.control()
    }

    fn read_at(&self, addr: usize, data: &mut [u8]) {
        if let Some(gate) = &self.gate {
            let entered = gate.entered.lock().take();
            if let Some(entered) = entered {
                entered.send(()).unwrap();
                gate.release
                    .lock()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        }
        self.mem.read_at(addr, data);
    }

    fn write_at(&self, addr: usize, data: &[u8]) {
        self.mem.write_at(addr, data);
    }

    fn len(&self) -> usize {
        self.mem.len()
    }
}

impl ReadGate {
    /// Allows the paused packet read to proceed without requesting stop.
    pub fn resume(&self) {
        self.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        self.release.send(()).unwrap();
    }

    /// Pauses the next packet read after the preceding gate was released.
    pub fn rearm(&self) {
        assert!(
            self.gate
                .entered
                .lock()
                .replace(self.notify.clone())
                .is_none()
        );
    }

    /// Requests stop after the first loop checkpoint but before packet admission.
    pub async fn stop<T: AsyncRun<S>, S: Send + 'static>(
        &self,
        task: &mut TaskControl<T, S>,
        driver: &DefaultDriver,
    ) {
        self.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(task.stop().now_or_never().is_none());
        self.release.send(()).unwrap();
        stop_with_timeout(task, driver).await;
    }
}

/// Stops a worker with a bounded deadline, preserving its state for assertions.
pub async fn stop_with_timeout<T: AsyncRun<S>, S: Send + 'static>(
    task: &mut TaskControl<T, S>,
    driver: &DefaultDriver,
) {
    let mut timer = pal_async::timer::PolledTimer::new(driver);
    match futures::future::select(
        std::pin::pin!(task.stop()),
        std::pin::pin!(timer.sleep(Duration::from_secs(5))),
    )
    .await
    {
        futures::future::Either::Left((stopped, _)) => assert!(stopped),
        futures::future::Either::Right(_) => panic!("channel worker did not stop"),
    }
}

/// Waits for a test operation without allowing a missing response to hang.
pub async fn with_timeout<T>(driver: &DefaultDriver, future: impl Future<Output = T>) -> T {
    let mut timer = pal_async::timer::PolledTimer::new(driver);
    match futures::future::select(
        std::pin::pin!(future),
        std::pin::pin!(timer.sleep(Duration::from_secs(5))),
    )
    .await
    {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => panic!("channel operation did not complete"),
    }
}
