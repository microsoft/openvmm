// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test harness extensions for the direct-mode and save/restore tests.
//!
//! [`ControlledMockSerialIo`] wraps the mock serial backend and applies the
//! fault injection configured in [`MockControls`]. [`TestHarness`] gains policy
//! device variants backed by it and device replacement for restore tests. The
//! remaining helpers poll the executor.

use super::MockSerialHandle;
use super::MockSerialIo;
use super::TestHarness;
use super::new_mock_serial;
use crate::VirtioConsoleDevice;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use serial_core::SerialIo;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// Additional mock backend state, applied by [`ControlledMockSerialIo`].
#[derive(Default)]
pub(super) struct MockControls {
    read_error_then_disconnect: bool,
    disconnect_poll_count: usize,
}

/// The mock serial backend with [`MockControls`] applied before each call is
/// forwarded to [`MockSerialIo`].
struct ControlledMockSerialIo(MockSerialIo);

fn new_controlled_mock_serial() -> (ControlledMockSerialIo, MockSerialHandle) {
    let (io, handle) = new_mock_serial();
    (ControlledMockSerialIo(io), handle)
}

impl InspectMut for ControlledMockSerialIo {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.0.inspect_mut(req);
    }
}

impl SerialIo for ControlledMockSerialIo {
    fn is_connected(&self) -> bool {
        self.0.is_connected()
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.poll_connect(cx)
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        {
            let mut shared = self.0.shared.lock();
            shared.controls.disconnect_poll_count += 1;
            if shared.controls.read_error_then_disconnect {
                shared.controls.read_error_then_disconnect = false;
                shared.connected = false;
                return Poll::Ready(Ok(()));
            }
        }
        self.0.poll_disconnect(cx)
    }
}

impl AsyncRead for ControlledMockSerialIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        {
            let shared = self.0.shared.lock();
            if shared.connected && shared.controls.read_error_then_disconnect {
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }
        }
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for ControlledMockSerialIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}

impl MockSerialHandle {
    pub(super) fn tx_data(&self) -> Vec<u8> {
        self.shared.lock().tx_buf.clone()
    }

    pub(super) fn pending_rx_len(&self) -> usize {
        self.shared.lock().rx_buf.len()
    }

    pub(super) fn set_read_error_then_disconnect(&self) {
        let mut shared = self.shared.lock();
        shared.controls.read_error_then_disconnect = true;
        if let Some(waker) = shared.rx_waker.take() {
            waker.wake();
        }
    }

    pub(super) fn disconnect_poll_count(&self) -> usize {
        self.shared.lock().controls.disconnect_poll_count
    }
}

impl TestHarness {
    pub(super) fn new_with_policy(
        driver: &DefaultDriver,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    ) -> Self {
        Self::new_with_device(driver, |driver_source, io| {
            VirtioConsoleDevice::new_with_policy(driver_source, io, disconnect_policy)
        })
    }

    /// Builds the standard harness, then swaps in the device returned by
    /// `make_device` together with a fresh controlled mock backend.
    fn new_with_device(
        driver: &DefaultDriver,
        make_device: impl FnOnce(&VmTaskDriverSource, Box<dyn SerialIo>) -> VirtioConsoleDevice,
    ) -> Self {
        let mut harness = Self::new(driver);
        let (io, handle) = new_controlled_mock_serial();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        harness.device = make_device(&driver_source, Box::new(io));
        harness.handle = handle;
        harness
    }

    pub(super) fn replace_device(&mut self, disconnect_policy: VirtioConsoleDisconnectPolicy) {
        let (io, handle) = new_controlled_mock_serial();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(self.driver.clone()));
        self.device =
            VirtioConsoleDevice::new_with_policy(&driver_source, Box::new(io), disconnect_policy);
        self.handle = handle;
    }
}

pub(super) async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

pub(super) async fn yield_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        yield_now().await;
    }
    assert!(condition(), "condition did not become true");
}
