// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a `Pollable` interface to vmswitch's DirectIO NIC
//! type. This provides a tap-like interface to vmswitch on Windows, allowing
//! Ethernet frames to be sent and received.

use super::kernel::c16;
use super::kernel::SwitchPortId;
use super::vmsif;
use futures::AsyncRead;
use guid::Guid;
use pal::windows::status_to_error;
use pal::windows::Overlapped;
use pal::windows::SendSyncRawHandle;
use pal_async::driver::Driver;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::ffi::c_void;
use std::io;
use std::io::ErrorKind;
use std::io::Write;
use std::os::windows::prelude::*;
use std::pin::Pin;
use std::ptr;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use winapi::shared::ntstatus;
use winapi::um::fileapi::ReadFile;
use winapi::um::fileapi::WriteFile;
use winapi::um::ioapiset::CancelIo;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::INFINITE;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const MINIMUM_FRAME_SIZE: usize = 60;
pub const FRAME_SIZE: usize = 1514;
const OUT_OP_COUNT: usize = 32;
const IN_OP_COUNT: usize = 2;
const IN_BUFFER_SIZE: usize = 32765;

pub struct DioNic {
    f: OwnedHandle,
    nic_name: String,
}

pub struct DioQueue {
    state: QueueState, // must be first so that it's dropped before the nic
    nic: DioNic,
}

struct QueueState {
    handle: SendSyncRawHandle,
    in_next_full: (usize, usize),
    in_next_pending: usize,
    in_buf: Box<[[u8; IN_BUFFER_SIZE]; IN_OP_COUNT]>,
    in_event: PolledWait<Event>,
    in_overlapped: Box<[Overlapped; IN_OP_COUNT]>,
    out_buf: Box<[[u8; FRAME_SIZE]; OUT_OP_COUNT]>,
    out_overlapped: Box<[Overlapped; OUT_OP_COUNT]>,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct DioNicPacketHeader {
    len: u32,
    next: u32,
}

impl DioNic {
    /// Creates a new direct IO NIC, not connected to any switch.
    pub fn new(
        vm_id: Guid,
        nic_name: &str,
        friendly_name: &str,
        mac_address: [u8; 6],
    ) -> io::Result<Self> {
        let full_nic_name = format!("{}--{}", vm_id, nic_name);
        let path = format!(r#"\\.\VmSwitch\{}"#, &full_nic_name);

        let handle = unsafe {
            let mut raw_handle = ptr::null_mut();
            vmsif::chk(vmsif::VmsIfNicCreateEmulated(
                &mut raw_handle,
                c16(path)?.as_ptr(),
            ))?;
            let handle = OwnedHandle::from_raw_handle(raw_handle);
            let vm_id_16 = c16(vm_id.to_string())?;
            vmsif::chk(vmsif::VmsIfNicMorphToEmulatedNic(
                handle.as_raw_handle(),
                c16(&full_nic_name)?.as_ptr(),
                c16(friendly_name)?.as_ptr(),
                c16(Guid::new_random().to_string())?.as_ptr(),
                vm_id_16.as_ptr(),
                vm_id_16.as_ptr(),
                &mac_address,
                true,
                0,
                0x100,
            ))?;

            handle
        };

        Ok(Self {
            f: handle,
            nic_name: full_nic_name,
        })
    }

    /// Connects the NIC to a port on the given switch.
    pub fn connect(&mut self, id: &SwitchPortId) -> io::Result<()> {
        let (switch16, port16) = id.c_ids();
        unsafe {
            vmsif::chk(vmsif::VmsIfNicConnect(
                self.f.as_raw_handle(),
                switch16.as_ptr(),
                port16.as_ptr(),
                c16(&self.nic_name)?.as_ptr(),
                Duration::from_secs(10).as_millis() as u32,
            ))?;

            Ok(())
        }
    }
}

impl DioQueue {
    pub fn new(driver: &(impl ?Sized + Driver), nic: DioNic) -> Self {
        // All read operations use the same event. This can cause spurious
        // wakeups (rarely, since reads should generally be completed by
        // vmswitch in order), but it cannot cause missed wakeups since we never
        // wait on the event and issue a new IO using the event concurrently.
        let in_event = PolledWait::new(driver, Event::new()).unwrap();
        let in_overlapped: Box<_> = (0..IN_OP_COUNT)
            .map(|_| {
                let mut o = Overlapped::new();
                o.set_event(in_event.get().as_handle().as_raw_handle());
                o
            })
            .collect();
        // Write operations do not use an event since we only need to wait for a
        // write to finish in `drop`, where spurious wakeups from completing
        // reads will not be a significant issue.
        let out_overlapped = Default::default();
        let handle = nic.f.as_raw_handle();
        let mut this = Self {
            nic,
            state: QueueState {
                handle: SendSyncRawHandle(handle),
                in_next_full: (0, 0),
                in_next_pending: 0,
                in_buf: Box::new([[0; IN_BUFFER_SIZE]; IN_OP_COUNT]),
                in_event,
                in_overlapped: in_overlapped.try_into().ok().unwrap(),
                out_buf: Box::new([[0; FRAME_SIZE]; OUT_OP_COUNT]),
                out_overlapped,
            },
        };
        for i in 0..IN_OP_COUNT {
            this.start_read(i)
        }
        this
    }

    pub fn into_inner(self) -> DioNic {
        let Self { state, nic } = self;
        // Ensure all IOs are cancelled.
        drop(state);
        nic
    }

    /// Checks if there are incoming packets ready to be processed. Fails with
    /// `ErrorKind::WouldBlock` if there are no packets ready.
    fn process_in(&mut self) -> io::Result<()> {
        if self.state.in_next_full.0 != self.state.in_next_pending {
            Ok(())
        } else {
            match self.state.in_overlapped[self.state.in_next_pending].io_status() {
                Some((ntstatus::STATUS_SUCCESS, _)) => {
                    self.state.in_next_pending = (self.state.in_next_pending + 1) % IN_OP_COUNT;
                    Ok(())
                }
                None => Err(ErrorKind::WouldBlock.into()),
                Some((status, _)) => Err(status_to_error(status)),
            }
        }
    }

    /// Initiates a read to vmswitch.
    fn start_read(&mut self, buf_index: usize) {
        unsafe {
            let buf = &mut self.state.in_buf[buf_index];
            ReadFile(
                self.nic.f.as_raw_handle(),
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
                ptr::null_mut(),
                self.state.in_overlapped[buf_index].as_ptr(),
            );
        }
    }

    pub fn poll_read_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.state.in_next_full.0 != self.state.in_next_pending
                || self.state.in_overlapped[self.state.in_next_pending]
                    .io_status()
                    .is_some()
            {
                break Poll::Ready(());
            }
            std::task::ready!(self.state.in_event.poll_wait(cx))
                .expect("wait on handle cannot fail");
        }
    }

    pub fn read_with<F, R>(&mut self, f: F) -> io::Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        self.process_in()?;

        let (buf_index, offset) = self.state.in_next_full;
        let buf = &self.state.in_buf[buf_index][offset..];
        let (header, data) = DioNicPacketHeader::read_from_prefix(buf).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
        let len = header.len as usize;
        let r = f(&data[..len]);
        if header.next != 0 {
            self.state.in_next_full = (buf_index, offset + header.next as usize);
        } else {
            // This batch of packets is done, so the buffer is available again.
            // Start the next read operation.
            self.start_read(buf_index);
            self.state.in_next_full = ((buf_index + 1) % IN_OP_COUNT, 0);
        }
        Ok(r)
    }

    pub fn write_with<F, R>(&mut self, mut len: usize, f: F) -> Option<R>
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        for (i, o) in self.state.out_overlapped.iter_mut().enumerate() {
            if let Some((status, _)) = o.io_status() {
                // This overlapped is available for reuse.
                if status != ntstatus::STATUS_SUCCESS {
                    tracing::warn!(
                        error = &status_to_error(status) as &dyn std::error::Error,
                        "packet write failure"
                    );
                }
                let buf = &mut self.state.out_buf[i];
                let r = f(&mut buf[..len]);
                // Zero pad short frames out to the minimum.
                if len < MINIMUM_FRAME_SIZE {
                    for b in &mut buf[len..MINIMUM_FRAME_SIZE] {
                        *b = 0;
                    }
                    len = MINIMUM_FRAME_SIZE;
                }
                unsafe {
                    WriteFile(
                        self.nic.f.as_raw_handle(),
                        buf.as_ptr().cast::<c_void>(),
                        len as u32,
                        ptr::null_mut(),
                        o.as_ptr(),
                    );
                }
                return Some(r);
            }
        }
        tracing::warn!("dropped packet");
        None
    }
}

impl AsyncRead for DioQueue {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        loop {
            std::task::ready!(this.poll_read_ready(cx));
            let res = this.read_with(|data| {
                buf[..data.len()].copy_from_slice(data);
                data.len()
            });
            match res {
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                r => break Poll::Ready(r),
            }
        }
    }
}

impl Write for DioQueue {
    fn write(&mut self, packet: &[u8]) -> io::Result<usize> {
        self.write_with(packet.len(), |buf| buf.copy_from_slice(packet));
        Ok(packet.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for QueueState {
    fn drop(&mut self) {
        // Cancel and wait on any outstanding IO to release the overlapped
        // structures and buffers.
        unsafe {
            CancelIo(self.handle.0);
        }
        for o in self.in_overlapped.iter() {
            while o.io_status().is_none() {
                // BUGBUG: it's possible that the event signal will be lost
                // since it's associated with an IO driver...
                self.in_event.get().wait();
            }
        }
        for o in self.out_overlapped.iter() {
            while o.io_status().is_none() {
                unsafe {
                    // Writes are started without an event but will signal the
                    // file object on completion.
                    WaitForSingleObject(self.handle.0, INFINITE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DioNic;
    use super::DioQueue;
    use super::FRAME_SIZE;
    use crate::kernel::SwitchPort;
    use crate::kernel::SwitchPortId;
    use futures::AsyncReadExt;
    use futures::FutureExt;
    use guid::Guid;
    use pal_async::async_test;
    use pal_async::driver::Driver;
    use pal_async::DefaultDriver;

    const MAC_ADDRESS: [u8; 6] = [0x00, 0x15, 0x5D, 0x18, 0x99, 0x25];

    fn connected_nic(driver: &impl Driver) -> (DioQueue, SwitchPort) {
        let vm_id = Guid::new_random();
        let mut e = DioNic::new(vm_id, "nic", "my nic", MAC_ADDRESS).unwrap();
        // Connect to the Default Switch by well-known GUID.
        let id = SwitchPortId {
            switch: "C08CB7B8-9B3C-408E-8E30-5E16A3AEB444".parse().unwrap(),
            port: Guid::new_random(),
        };
        let port = SwitchPort::new(&id).unwrap();
        e.connect(&id).unwrap();
        let queue = DioQueue::new(driver, e);
        (queue, port)
    }

    #[async_test]
    #[ignore] // Requires vmswitch and admin privileges
    async fn test_default_switch(driver: DefaultDriver) {
        let (mut e, _port) = connected_nic(&driver);
        let mut packet = [0; FRAME_SIZE];
        assert!(e.read(&mut packet).now_or_never().is_none());
    }
}
