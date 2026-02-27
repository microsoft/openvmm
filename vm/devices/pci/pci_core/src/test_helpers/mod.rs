// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various PCI behaviors.

use crate::msi::SignalMsi;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// A test-only interrupt controller that simply stashes incoming interrupt
/// requests in a FIFO queue. Implements [`SignalMsi`].
#[derive(Debug, Clone)]
pub struct TestPciInterruptController {
    inner: Arc<TestPciInterruptControllerInner>,
}

#[derive(Debug)]
struct TestPciInterruptControllerInner {
    // TODO: also support INTx interrupts
    msi_requests: Mutex<VecDeque<(u64, u32)>>, // (addr, data)
}

impl TestPciInterruptController {
    /// Return a new test PCI interrupt controller
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TestPciInterruptControllerInner {
                msi_requests: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Fetch the first (addr, data) MSI-X interrupt in the FIFO interrupt queue
    pub fn get_next_interrupt(&self) -> Option<(u64, u32)> {
        self.inner.msi_requests.lock().pop_front()
    }

    /// Returns an `Arc<dyn SignalMsi>` to this controller.
    pub fn signal_msi(&self) -> Arc<dyn SignalMsi> {
        self.inner.clone()
    }
}

impl SignalMsi for TestPciInterruptControllerInner {
    fn signal_msi(&self, rid: u32, address: u64, data: u32) {
        assert_eq!(rid, 0);
        self.msi_requests.lock().push_back((address, data));
    }
}
