// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-memory forwarding for remote chipset devices.
//!
//! [`GuestMemoryTask`] services requests in the parent process, while
//! [`GuestMemoryRemoteBuilder`] constructs the worker's guest-memory access.

use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use parking_lot::Mutex;
use std::future::poll_fn;
use std::ptr::NonNull;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::StopTask;

/// Requests sent to the parent process for guest memory access.
#[derive(MeshPayload)]
enum GuestMemoryRequest {
    /// Read from guest memory.
    Read { addr: u64, len: usize },
    /// Write to guest memory.
    Write { addr: u64, data: Vec<u8> },
    /// Fill to guest memory.
    Fill { addr: u64, val: u8, len: usize },
}

/// Responses from the parent process for guest memory access.
#[derive(MeshPayload)]
enum GuestMemoryResponse {
    /// Response to a read operation.
    Read(Result<Vec<u8>, RemoteError>),
    /// Response to a write operation.
    Write(Result<(), RemoteError>),
}

/// A proxy for guest memory access that forwards operations over a mesh channel.
struct GuestMemoryProxy {
    gm: GuestMemory,
    req_recv: mesh::Receiver<GuestMemoryRequest>,
    resp_send: mesh::Sender<GuestMemoryResponse>,
}

pub struct GuestMemoryTask {
    guest_memory: GuestMemoryProxy,
    encrypted_guest_memory: GuestMemoryProxy,
}

impl GuestMemoryTask {
    /// Returns the task, normal-memory builder, and encrypted-memory builder.
    pub fn new(
        guest_memory: GuestMemory,
        encrypted_guest_memory: GuestMemory,
    ) -> (Self, GuestMemoryRemoteBuilder, GuestMemoryRemoteBuilder) {
        let (guest_memory, guest_memory_remote) = GuestMemoryProxy::new(guest_memory);
        let (encrypted_guest_memory, encrypted_guest_memory_remote) =
            GuestMemoryProxy::new(encrypted_guest_memory);
        (
            Self {
                guest_memory,
                encrypted_guest_memory,
            },
            guest_memory_remote,
            encrypted_guest_memory_remote,
        )
    }
}

impl AsyncRun<()> for GuestMemoryTask {
    async fn run(&mut self, stop: &mut StopTask<'_>, _: &mut ()) -> Result<(), Cancelled> {
        stop.until_stopped(poll_fn(|cx| {
            self.guest_memory.poll(cx);
            self.encrypted_guest_memory.poll(cx);
            Poll::Pending
        }))
        .await
    }
}

#[derive(MeshPayload)]
pub struct GuestMemoryRemoteBuilder {
    inner: GuestMemoryRemoteInner,
}

struct GuestMemoryRemote {
    inner: Mutex<GuestMemoryRemoteInner>,
}

#[derive(MeshPayload)]
struct GuestMemoryRemoteInner {
    req_send: mesh::Sender<GuestMemoryRequest>,
    resp_recv: mesh::Receiver<GuestMemoryResponse>,
}

impl GuestMemoryProxy {
    /// Create a new guest memory proxy.
    fn new(gm: GuestMemory) -> (Self, GuestMemoryRemoteBuilder) {
        let (req_send, req_recv) = mesh::channel();
        let (resp_send, resp_recv) = mesh::channel();

        let remote = GuestMemoryRemoteBuilder {
            inner: GuestMemoryRemoteInner {
                req_send,
                resp_recv,
            },
        };
        let proxy = GuestMemoryProxy {
            gm,
            req_recv,
            resp_send,
        };

        (proxy, remote)
    }

    fn poll(&mut self, cx: &mut std::task::Context<'_>) {
        let Poll::Ready(Some(request)) = self.req_recv.poll_next_unpin(cx) else {
            return;
        };
        let response = match request {
            GuestMemoryRequest::Read { addr, len } => {
                let mut data = vec![0u8; len];
                let result = self
                    .gm
                    .read_at(addr, &mut data)
                    .map(|_| data)
                    .map_err(RemoteError::new);
                GuestMemoryResponse::Read(result)
            }
            GuestMemoryRequest::Write { addr, data } => {
                let result = self.gm.write_at(addr, &data).map_err(RemoteError::new);
                GuestMemoryResponse::Write(result)
            }
            GuestMemoryRequest::Fill { addr, val, len } => {
                let result = self.gm.fill_at(addr, val, len).map_err(RemoteError::new);
                GuestMemoryResponse::Write(result)
            }
        };
        self.resp_send.send(response);
        // Bound each poll so both memory services and task control can run.
        cx.waker().wake_by_ref();
    }
}

impl GuestMemoryRemoteBuilder {
    /// Build the `GuestMemory` from the builder.
    pub fn build(self, name: &str) -> GuestMemory {
        GuestMemory::new(
            name,
            GuestMemoryRemote {
                inner: Mutex::new(self.inner),
            },
        )
    }
}

impl GuestMemoryRemote {
    fn handle_blocking(
        &self,
        addr: u64,
        request: GuestMemoryRequest,
    ) -> Result<GuestMemoryResponse, GuestMemoryBackingError> {
        let mut inner = self.inner.lock();
        inner.req_send.send(request);
        pal_async::local::block_on(inner.resp_recv.recv())
            .map_err(|e| GuestMemoryBackingError::other(addr, e))
    }
}

// SAFETY: This implementation forwards all operations to the parent process,
// which has the actual guest memory access. Since we don't have direct mapping
// access in the worker process, we return None for mapping() and implement
// all operations via fallback methods.
unsafe impl GuestMemoryAccess for GuestMemoryRemote {
    fn mapping(&self) -> Option<NonNull<u8>> {
        None
    }

    fn page_fault(
        &self,
        _address: u64,
        _len: usize,
        _write: bool,
        _bitmap_failure: bool,
    ) -> guestmem::PageFaultAction {
        guestmem::PageFaultAction::Fallback
    }

    fn max_address(&self) -> u64 {
        u64::MAX
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let GuestMemoryResponse::Read(data) =
            self.handle_blocking(addr, GuestMemoryRequest::Read { addr, len })?
        else {
            unreachable!()
        };
        let data = data.map_err(|e| GuestMemoryBackingError::other(addr, e))?;
        assert_eq!(data.len(), len);
        // SAFETY: Caller guarantees dest is valid for write.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), dest, len);
        }
        Ok(())
    }

    unsafe fn write_fallback(
        &self,
        addr: u64,
        src: *const u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let mut data = vec![0u8; len];
        // SAFETY: Caller guarantees src is valid for read.
        unsafe { std::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), len) };
        let GuestMemoryResponse::Write(result) =
            self.handle_blocking(addr, GuestMemoryRequest::Write { addr, data })?
        else {
            unreachable!()
        };
        result.map_err(|e| GuestMemoryBackingError::other(addr, e))
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        let GuestMemoryResponse::Write(result) =
            self.handle_blocking(addr, GuestMemoryRequest::Fill { addr, val, len })?
        else {
            unreachable!()
        };
        result.map_err(|e| GuestMemoryBackingError::other(addr, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::ArcWake;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::task::Context;
    use task_control::TaskControl;
    use test_with_tracing::test;

    async fn run_remote(
        driver: &DefaultDriver,
        guest_memory: GuestMemory,
        encrypted_guest_memory: GuestMemory,
        run: impl FnOnce(GuestMemory, GuestMemory) + Send + 'static,
    ) {
        let (task, guest_memory, encrypted_guest_memory) =
            GuestMemoryTask::new(guest_memory, encrypted_guest_memory);
        let mut control = TaskControl::new(task);
        control.insert(driver, "test-guest-memory", ());
        control.start();
        let (done, done_recv) = mesh::oneshot();
        let thread = std::thread::spawn(move || {
            run(
                guest_memory.build("remote"),
                encrypted_guest_memory.build("remote-encrypted"),
            );
            done.send(());
        });
        done_recv.await.unwrap();
        thread.join().unwrap();
        control.stop().await;
    }

    #[async_test]
    async fn memory_spaces_are_independent(driver: DefaultDriver) {
        let guest_memory = GuestMemory::allocate(4096);
        let encrypted_guest_memory = GuestMemory::allocate(4096);
        guest_memory.fill_at(0, 0x11, 8).unwrap();
        encrypted_guest_memory.fill_at(0, 0x22, 8).unwrap();
        run_remote(
            &driver,
            guest_memory.clone(),
            encrypted_guest_memory.clone(),
            |normal, encrypted| {
                let mut data = [0; 8];
                normal.read_at(0, &mut data).unwrap();
                assert_eq!(data, [0x11; 8]);
                encrypted.read_at(0, &mut data).unwrap();
                assert_eq!(data, [0x22; 8]);
                normal.write_at(0, &[1, 2, 3, 4]).unwrap();
                normal.fill_at(4, 0x33, 4).unwrap();
                encrypted.write_at(0, &[5, 6, 7, 8]).unwrap();
                encrypted.fill_at(4, 0x44, 4).unwrap();
            },
        )
        .await;
        let mut data = [0; 8];
        guest_memory.read_at(0, &mut data).unwrap();
        assert_eq!(data, [1, 2, 3, 4, 0x33, 0x33, 0x33, 0x33]);
        encrypted_guest_memory.read_at(0, &mut data).unwrap();
        assert_eq!(data, [5, 6, 7, 8, 0x44, 0x44, 0x44, 0x44]);
    }

    #[async_test]
    async fn memory_errors_do_not_stop_service(driver: DefaultDriver) {
        run_remote(
            &driver,
            GuestMemory::allocate(4096),
            GuestMemory::allocate(4096),
            |normal, encrypted| {
                for memory in [&normal, &encrypted] {
                    let mut data = [0; 4];
                    assert!(memory.read_at(4096, &mut data).is_err());
                    assert!(memory.write_at(4096, &[1; 4]).is_err());
                    assert!(memory.fill_at(4096, 0xff, 4).is_err());
                    memory.read_at(0, &mut data).unwrap();
                    assert_eq!(data, [0; 4]);
                    memory.write_at(0, &[1, 2, 3, 4]).unwrap();
                    memory.read_at(0, &mut data).unwrap();
                    assert_eq!(data, [1, 2, 3, 4]);
                    memory.fill_at(0, 0x55, 4).unwrap();
                    memory.read_at(0, &mut data).unwrap();
                    assert_eq!(data, [0x55; 4]);
                }
            },
        )
        .await;
    }

    #[async_test]
    async fn closing_one_memory_space_keeps_the_other_running(driver: DefaultDriver) {
        for close_normal in [false, true] {
            run_remote(
                &driver,
                GuestMemory::allocate(4096),
                GuestMemory::allocate(4096),
                move |normal, encrypted| {
                    let memory = if close_normal {
                        drop(normal);
                        encrypted
                    } else {
                        drop(encrypted);
                        normal
                    };
                    for value in [0x11, 0x22, 0x33] {
                        memory.fill_at(0, value, 4).unwrap();
                        let mut data = [0; 4];
                        memory.read_at(0, &mut data).unwrap();
                        assert_eq!(data, [value; 4]);
                    }
                },
            )
            .await;
        }
    }

    struct WakeFlag(AtomicBool);

    impl ArcWake for WakeFlag {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn polling_is_bounded_and_reschedules_pending_requests() {
        let memory = GuestMemory::allocate(4096);
        let (mut proxy, mut remote) = GuestMemoryProxy::new(memory.clone());
        for value in [0x11, 0x22] {
            remote.inner.req_send.send(GuestMemoryRequest::Fill {
                addr: 0,
                val: value,
                len: 4,
            });
        }
        let wake = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = futures::task::waker(wake.clone());
        let mut cx = Context::from_waker(&waker);
        for value in [0x11, 0x22] {
            wake.0.store(false, Ordering::SeqCst);
            proxy.poll(&mut cx);
            let GuestMemoryResponse::Write(result) = remote.inner.resp_recv.try_recv().unwrap()
            else {
                panic!("expected a write response");
            };
            result.unwrap();
            assert!(remote.inner.resp_recv.try_recv().is_err());
            let mut data = [0; 4];
            memory.read_at(0, &mut data).unwrap();
            assert_eq!(data, [value; 4]);
            assert!(wake.0.load(Ordering::SeqCst));
        }
        wake.0.store(false, Ordering::SeqCst);
        proxy.poll(&mut cx);
        assert!(!wake.0.load(Ordering::SeqCst));
        drop(remote);
        wake.0.store(false, Ordering::SeqCst);
        proxy.poll(&mut cx);
        assert!(!wake.0.load(Ordering::SeqCst));
    }
}
