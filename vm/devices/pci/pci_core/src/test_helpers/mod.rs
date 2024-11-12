// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock types for unit-testing various PCI behaviors.

use crate::msi::MsiControl;
use crate::msi::MsiInterruptTarget;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// A test-only interrupt controller that simply stashes incoming interrupt
/// requests in a FIFO queue. Implements [`MsiInterruptTarget`].
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
}

impl MsiInterruptTarget for TestPciInterruptController {
    fn new_interrupt(&self) -> Box<dyn MsiControl> {
        let controller = self.inner.clone();
        Box::new(move |address, data| controller.msi_requests.lock().push_back((address, data)))
    }
}
