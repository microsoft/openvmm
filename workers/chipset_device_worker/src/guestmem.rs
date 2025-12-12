// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest memory proxy for remote chipset devices.
//!
//! This module provides a [`GuestMemoryProxy`] that implements [`GuestMemoryAccess`]
//! by forwarding memory operations over a mesh channel to the parent process.

use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use mesh::MeshPayload;
use mesh::RecvError;
use mesh::error::RemoteError;
use parking_lot::Mutex;
use std::ptr::NonNull;
use std::task::Poll;

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
pub struct GuestMemoryProxy {
    gm: GuestMemory,
    req_recv: mesh::Receiver<GuestMemoryRequest>,
    resp_send: mesh::Sender<GuestMemoryResponse>,
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
    pub fn new(gm: GuestMemory) -> (Self, GuestMemoryRemoteBuilder) {
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

    pub fn poll(&mut self, cx: &mut std::task::Context<'_>) {
        while let Poll::Ready(request) = self.req_recv.poll_recv(cx) {
            let response = match request {
                Ok(GuestMemoryRequest::Read { addr, len }) => {
                    let mut data = vec![0u8; len];
                    let result = self
                        .gm
                        .read_at(addr, &mut data)
                        .map(|_| data)
                        .map_err(RemoteError::new);
                    GuestMemoryResponse::Read(result)
                }
                Ok(GuestMemoryRequest::Write { addr, data }) => {
                    let result = self.gm.write_at(addr, &data).map_err(RemoteError::new);
                    GuestMemoryResponse::Write(result)
                }
                Ok(GuestMemoryRequest::Fill { addr, val, len }) => {
                    let result = self.gm.fill_at(addr, val, len).map_err(RemoteError::new);
                    GuestMemoryResponse::Write(result)
                }
                Err(RecvError::Closed) => {
                    // The remote device may just drop guest memory if it
                    // doesn't need it. We can just stop processing requests.
                    break;
                }
                Err(RecvError::Error(e)) => {
                    // TODO: Handle errors?
                    panic!("guest memory proxy channel error: {:?}", e);
                }
            };
            self.resp_send.send(response);
        }
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
    fn handle_blocking(&self, request: GuestMemoryRequest) -> GuestMemoryResponse {
        let mut inner = self.inner.lock();
        inner.req_send.send(request);
        let resp = pal_async::local::block_on(inner.resp_recv.recv());
        // TODO: Handle errors?
        resp.unwrap()
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
        // TODO: Is this the right value?
        u64::MAX
    }

    unsafe fn read_fallback(
        &self,
        addr: u64,
        dest: *mut u8,
        len: usize,
    ) -> Result<(), GuestMemoryBackingError> {
        let GuestMemoryResponse::Read(data) =
            self.handle_blocking(GuestMemoryRequest::Read { addr, len })
        else {
            unreachable!()
        };
        let data = data.map_err(|e| GuestMemoryBackingError::other(addr, e))?;
        debug_assert_eq!(data.len(), len);
        // SAFETY: caller guarantees dest is valid for write.
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
        // SAFETY: caller guarantees src is valid for read.
        let data = unsafe { std::slice::from_raw_parts(src, len) }.to_vec();
        let GuestMemoryResponse::Write(result) =
            self.handle_blocking(GuestMemoryRequest::Write { addr, data })
        else {
            unreachable!()
        };
        result.map_err(|e| GuestMemoryBackingError::other(addr, e))
    }

    fn fill_fallback(&self, addr: u64, val: u8, len: usize) -> Result<(), GuestMemoryBackingError> {
        let GuestMemoryResponse::Write(result) =
            self.handle_blocking(GuestMemoryRequest::Fill { addr, val, len })
        else {
            unreachable!()
        };
        result.map_err(|e| GuestMemoryBackingError::other(addr, e))
    }
}
