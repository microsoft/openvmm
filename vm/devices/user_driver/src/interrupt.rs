// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt handling for user-mode device drivers.

use parking_lot::Mutex;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::Ordering::Release;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// A mapped device interrupt.
///
/// This interrupt can be cloned multiple times. Each clone will be separately
/// pollable. Initially, the clone is in the not-signaled state, even if the
/// original instance is signaled.
pub struct DeviceInterrupt {
    slot: Arc<DeviceInterruptSlot>,
    inner: Arc<DeviceInterruptInner>,
}

impl Clone for DeviceInterrupt {
    fn clone(&self) -> Self {
        self.inner.new_interrupt()
    }
}

impl Drop for DeviceInterrupt {
    fn drop(&mut self) {
        let mut slots = self.inner.slots.lock();
        let i = slots
            .iter()
            .position(|s| Arc::ptr_eq(s, &self.slot))
            .unwrap();
        slots.swap_remove(i);
        self.inner.slots_updated.store(true, Release);
    }
}

impl DeviceInterrupt {
    /// Polls the interrupt, returning `Poll::Ready` if the interrupt is
    /// signaled.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.slot.poll(cx)
    }

    /// Waits for the interrupt to be signaled.
    pub async fn wait(&mut self) {
        poll_fn(|cx| self.poll(cx)).await
    }
}

struct DeviceInterruptSlot {
    signaled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl DeviceInterruptSlot {
    fn new() -> Self {
        Self {
            signaled: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.signaled.load(Acquire) {
            self.signaled.store(false, Release);
            Poll::Ready(())
        } else {
            let _old_waker;
            let mut waker = self.waker.lock();
            // Check again under the lock.
            if self.signaled.load(Acquire) {
                self.signaled.store(false, Release);
                return Poll::Ready(());
            }
            if waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                _old_waker = waker.replace(cx.waker().clone());
            }
            Poll::Pending
        }
    }

    fn signal(&self) {
        self.signaled.store(true, Release);
        if let Some(waker) = self.waker.lock().take() {
            waker.wake();
        }
    }
}

struct DeviceInterruptInner {
    slots: Mutex<Vec<Arc<DeviceInterruptSlot>>>,
    slots_updated: AtomicBool,
}

impl DeviceInterruptInner {
    fn new_interrupt(self: &Arc<Self>) -> DeviceInterrupt {
        let slot = Arc::new(DeviceInterruptSlot::new());
        self.slots.lock().push(slot.clone());
        self.slots_updated.store(true, Release);
        DeviceInterrupt {
            slot,
            inner: self.clone(),
        }
    }
}

/// A source of device interrupts.
///
/// This is intended to be used by the device backends to signal the
/// [`DeviceInterrupt`] instances used by the drivers.
pub struct DeviceInterruptSource {
    slots: Vec<Arc<DeviceInterruptSlot>>,
    inner: Arc<DeviceInterruptInner>,
}

impl DeviceInterruptSource {
    /// Creates a new interrupt source.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DeviceInterruptInner {
                slots: Mutex::new(Vec::new()),
                slots_updated: false.into(),
            }),
            slots: Vec::new(),
        }
    }

    /// Creates a new interrupt target, each of which is notified when `signal`
    /// is called.
    pub fn new_target(&self) -> DeviceInterrupt {
        self.inner.new_interrupt()
    }

    /// Signals all interrupt targets.
    pub fn signal(&mut self) {
        if self.inner.slots_updated.load(Acquire) {
            let slots = self.inner.slots.lock();
            self.inner.slots_updated.store(false, Relaxed);
            self.slots.clone_from(&*slots);
        }
        for slot in &self.slots {
            slot.signal();
        }
    }

    /// Signals all interrupt targets without using the target cache. Use
    /// `signal` instead when you have a mutable reference.
    pub fn signal_uncached(&self) {
        for slot in &*self.inner.slots.lock() {
            slot.signal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceInterruptSource;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;

    #[async_test]
    async fn test_interrupt(driver: DefaultDriver) {
        let mut source = DeviceInterruptSource::new();
        let mut target = source.new_target();
        source.signal();
        target.wait().await;
        let mut target_clone = target.clone();
        let task = driver.spawn("test", async move { target_clone.wait().await });
        source.signal();
        task.await;
        target.wait().await;
    }
}
