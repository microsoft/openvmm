// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test harness extensions for the direct-mode, broker, and save/restore tests.
//!
//! [`ControlledMockSerialIo`] wraps the mock serial backend and applies the
//! fault injection, write backpressure, and local peer identity configured in
//! [`MockControls`]. [`TestHarness`] gains policy and broker device variants
//! backed by it, device replacement for restore tests, and helpers that move
//! whole buffers through the guest queues. The remaining helpers poll the
//! executor and encode control-session records.

use super::MockSerialHandle;
use super::MockSerialIo;
use super::QUEUE_SIZE;
use super::TestHarness;
use super::new_mock_serial;
use crate::VirtioConsoleDevice;
use crate::control_session_protocol;
use crate::control_session_protocol::Record;
use crate::control_session_protocol::RecordType;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use serial_core::LocalPeerIdentity;
use serial_core::SerialIo;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use virtio_resources::console::control::VirtioControlConsoleBrokerConfig;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

pub(super) const BROKER_INSTANCE: [u8; 16] = [0x51; 16];
pub(super) const BROKER_CAPABILITY: [u8; 32] = [0xa7; 32];

/// Additional mock backend state, applied by [`ControlledMockSerialIo`].
pub(super) struct MockControls {
    write_blocked: bool,
    write_waker: Option<Waker>,
    read_error_then_disconnect: bool,
    connect_error: bool,
    disconnect_error: bool,
    disconnect_current_error: bool,
    disconnect_poll_count: usize,
    peer_identity: Option<LocalPeerIdentity>,
}

impl Default for MockControls {
    fn default() -> Self {
        Self {
            write_blocked: false,
            write_waker: None,
            read_error_then_disconnect: false,
            connect_error: false,
            disconnect_error: false,
            disconnect_current_error: false,
            disconnect_poll_count: 0,
            peer_identity: Some(LocalPeerIdentity::UnixUid(1000)),
        }
    }
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
        {
            let mut shared = self.0.shared.lock();
            if shared.controls.connect_error {
                shared.controls.connect_error = false;
                return Poll::Ready(Err(io::Error::other("injected connect error")));
            }
        }
        self.0.poll_connect(cx)
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        {
            let mut shared = self.0.shared.lock();
            shared.controls.disconnect_poll_count += 1;
            if shared.controls.disconnect_error {
                shared.controls.disconnect_error = false;
                return Poll::Ready(Err(io::Error::other("injected disconnect error")));
            }
            if shared.controls.read_error_then_disconnect {
                shared.controls.read_error_then_disconnect = false;
                shared.connected = false;
                return Poll::Ready(Ok(()));
            }
        }
        self.0.poll_disconnect(cx)
    }

    fn local_peer_identity(&self) -> io::Result<Option<LocalPeerIdentity>> {
        Ok(self.0.shared.lock().controls.peer_identity.clone())
    }

    fn disconnect_current(&mut self) -> io::Result<()> {
        let mut shared = self.0.shared.lock();
        if shared.controls.disconnect_current_error {
            return Err(io::Error::other("injected disconnect_current error"));
        }
        shared.connected = false;
        Ok(())
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
        {
            let mut shared = self.0.shared.lock();
            if shared.connected && shared.controls.write_blocked {
                shared.controls.write_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
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

    pub(super) fn set_write_blocked(&self, blocked: bool) {
        let mut shared = self.shared.lock();
        shared.controls.write_blocked = blocked;
        if !blocked {
            if let Some(waker) = shared.controls.write_waker.take() {
                waker.wake();
            }
        }
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

    pub(super) fn is_connected(&self) -> bool {
        self.shared.lock().connected
    }

    pub(super) fn set_disconnect_error(&self) {
        let mut shared = self.shared.lock();
        shared.controls.disconnect_error = true;
        if let Some(waker) = shared.disconnect_waker.take() {
            waker.wake();
        }
    }

    pub(super) fn set_disconnect_current_error(&self) {
        self.shared.lock().controls.disconnect_current_error = true;
    }

    pub(super) fn set_peer_identity(&self, identity: Option<LocalPeerIdentity>) {
        self.shared.lock().controls.peer_identity = identity;
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

    pub(super) fn new_broker(
        driver: &DefaultDriver,
        instance_id: [u8; 16],
        capability: [u8; 32],
    ) -> Self {
        Self::new_broker_with_timeout(driver, instance_id, capability, 5000)
    }

    pub(super) fn new_broker_with_timeout(
        driver: &DefaultDriver,
        instance_id: [u8; 16],
        capability: [u8; 32],
        auth_timeout_ms: u64,
    ) -> Self {
        Self::new_with_device(driver, |driver_source, io| {
            VirtioConsoleDevice::new_broker(
                driver_source,
                io,
                VirtioControlConsoleBrokerConfig {
                    instance_id,
                    capability,
                    expected_peer_identity: LocalPeerIdentity::UnixUid(1000),
                    auth_timeout_ms,
                },
            )
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

    pub(super) async fn send_guest_bytes(&mut self, desc_index: u16, bytes: &[u8]) {
        self.post_tx_and_signal(desc_index, bytes);
        let (used_id, used_len) = self.wait_for_tx_used().await;
        assert_eq!(used_id, desc_index);
        assert_eq!(used_len, 0);
    }

    pub(super) async fn receive_guest_bytes(&mut self, desc_index: u16, size: u32) -> Vec<u8> {
        let gpa = self.post_rx_buffer_and_signal(desc_index, size);
        let (used_id, used_len) = self.wait_for_rx_used().await;
        assert_eq!(used_id, desc_index);
        let mut bytes = vec![0; used_len as usize];
        self.mem.read_at(gpa, &mut bytes).unwrap();
        bytes
    }

    pub(super) async fn receive_guest_exact(
        &mut self,
        first_desc_index: u16,
        mut size: usize,
    ) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(size);
        let mut desc_index = first_desc_index;
        while size != 0 {
            let chunk = size.min(crate::BUF_SIZE);
            bytes.extend(self.receive_guest_bytes(desc_index, chunk as u32).await);
            size -= chunk;
            desc_index = (desc_index + 1) % QUEUE_SIZE;
        }
        bytes
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

pub(super) fn encode(record: &Record) -> Vec<u8> {
    control_session_protocol::encode(record).unwrap()
}

pub(super) fn decode(bytes: &[u8]) -> Record {
    control_session_protocol::decode_exact(bytes).unwrap()
}

pub(super) fn ack(instance_id: [u8; 16], epoch: u64) -> Record {
    Record::session(
        RecordType::Ack,
        instance_id,
        epoch,
        0,
        control_session_protocol::credit_payload(control_session_protocol::MIN_RECEIVE_CREDIT),
    )
}
