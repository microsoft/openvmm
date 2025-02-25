// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calling vmbus proxy ioctls.
#![expect(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use futures::poll;
use guestmem::GuestMemory;
use mesh::MeshPayload;
use pal::windows::ObjectAttributes;
use pal::windows::UnicodeStringRef;
use pal_async::driver::Driver;
use pal_async::windows::overlapped::IoBuf;
use pal_async::windows::overlapped::IoBufMut;
use pal_async::windows::overlapped::OverlappedFile;
use pal_event::Event;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmbusioctl::VMBUS_CHANNEL_OFFER;
use vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use widestring::utf16str;
use widestring::Utf16Str;
use windows::Wdk::Storage::FileSystem::NtOpenFile;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::NTSTATUS;
use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows::Win32::System::IO::DeviceIoControl;
use zerocopy::IntoBytes;

mod proxyioctl;
pub mod vmbusioctl;

pub type Error = windows::core::Error;
pub type Result<T> = windows::core::Result<T>;

/// A VM handle the VMBus proxy driver.
#[derive(Debug, MeshPayload)]
pub struct ProxyHandle(std::fs::File);

impl ProxyHandle {
    /// Creates a new VM handle.
    pub fn new() -> Result<Self> {
        const DEVICE_PATH: &Utf16Str = utf16str!("\\Device\\VmbusProxy");
        let pathu = UnicodeStringRef::try_from(DEVICE_PATH).expect("string fits");
        let mut oa = ObjectAttributes::new();
        oa.name(&pathu);
        // SAFETY: calling API according to docs.
        unsafe {
            let mut iosb = zeroed();
            let mut handle = HANDLE::default();
            NtOpenFile(
                &mut handle,
                (FILE_ALL_ACCESS | SYNCHRONIZE).0,
                oa.as_ref(),
                &mut iosb,
                0,
                0,
            )
            .ok()?;
            Ok(Self(std::fs::File::from_raw_handle(handle.0 as RawHandle)))
        }
    }
}

impl From<OwnedHandle> for ProxyHandle {
    /// Create a `ProxyHandle` from an existing VM handle.
    fn from(value: OwnedHandle) -> Self {
        Self(value.into())
    }
}

pub struct VmbusProxy {
    file: OverlappedFile,
    // NOTE: This must come after `file` so that it is not released until `file`
    // is closed.
    guest_memory: Option<GuestMemory>,
    cancelled: AtomicBool,
}

#[derive(Debug)]
pub enum ProxyAction {
    Offer {
        id: u64,
        offer: VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
        outgoing_event: Option<Event>,
    },
    Revoke {
        id: u64,
    },
    InterruptPolicy {},
}

struct StaticIoctlBuffer<T>(T);

// SAFETY: this is not generically safe, so callers must be careful to only use
// this newtype for values that can be safely passed to overlapped IO.
unsafe impl<T> IoBuf for StaticIoctlBuffer<T> {
    fn as_ptr(&self) -> *const u8 {
        std::ptr::from_ref::<Self>(self).cast()
    }

    fn len(&self) -> usize {
        size_of_val(self)
    }
}

// SAFETY: this is not generically safe, so callers must be careful to only use
// this newtype for values that can be safely passed to overlapped IO.
unsafe impl<T> IoBufMut for StaticIoctlBuffer<T> {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        std::ptr::from_mut::<Self>(self).cast()
    }
}

impl VmbusProxy {
    pub fn new(driver: &dyn Driver, handle: ProxyHandle) -> Result<Self> {
        Ok(Self {
            file: OverlappedFile::new(driver, handle.0)?,
            guest_memory: None,
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.file.cancel();
    }

    pub fn handle(&self) -> BorrowedHandle<'_> {
        self.file.get().as_handle()
    }

    async unsafe fn ioctl<In, Out>(&self, code: u32, input: In, output: Out) -> Result<Out>
    where
        In: IoBufMut,
        Out: IoBufMut,
    {
        // Don't issue new IO if the cancel() method has been called.
        if self.cancelled.load(Ordering::Acquire) {
            tracing::trace!("ioctl cancelled before issued");
            return Err(Error::from_hresult(ERROR_CANCELLED.into()));
        }

        // SAFETY: guaranteed by caller.
        let mut ioctl = unsafe { self.file.ioctl(code, input, output) };
        let (r, (_, output)) = match poll!(&mut ioctl) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                // Cancellation may have happened after the check above but before the IO was
                // issued, in which case it was not actually cancelled. Cancel it again now just in
                // case.
                if self.cancelled.load(Ordering::Acquire) {
                    tracing::trace!("ioctl cancelled during issue");
                    ioctl.cancel();
                }

                // Even when cancelled, we must wait to complete the IO so buffers aren't released
                // while still in use.
                ioctl.await
            }
        };

        r?;
        Ok(output)
    }

    pub async fn set_memory(&mut self, guest_memory: &GuestMemory) -> Result<()> {
        assert!(self.guest_memory.is_none());
        let (base, len) = guest_memory.full_mapping().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "vmbusproxy not supported without mapped memory",
            )
        })?;
        self.guest_memory = Some(guest_memory.clone());
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_SET_MEMORY,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_SET_MEMORY_INPUT {
                    BaseAddress: base as usize as u64,
                    Size: len as u64,
                }),
                (),
            )
            .await
        }
    }

    pub async fn next_action(&self) -> Result<ProxyAction> {
        let output = unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_NEXT_ACTION,
                (),
                StaticIoctlBuffer(zeroed::<proxyioctl::VMBUS_PROXY_NEXT_ACTION_OUTPUT>()),
            )
            .await?
            .0
        };
        match output.Type {
            proxyioctl::VmbusProxyActionTypeOffer => unsafe {
                Ok(ProxyAction::Offer {
                    id: output.ChannelId,
                    offer: output.u.Offer.Offer,
                    incoming_event: OwnedHandle::from_raw_handle(
                        output.u.Offer.DeviceIncomingRingEvent as usize as RawHandle,
                    )
                    .into(),
                    outgoing_event: if output.u.Offer.DeviceOutgoingRingEvent != 0 {
                        Some(
                            OwnedHandle::from_raw_handle(
                                output.u.Offer.DeviceOutgoingRingEvent as usize as RawHandle,
                            )
                            .into(),
                        )
                    } else {
                        None
                    },
                })
            },
            proxyioctl::VmbusProxyActionTypeRevoke => Ok(ProxyAction::Revoke {
                id: output.ChannelId,
            }),
            proxyioctl::VmbusProxyActionTypeInterruptPolicy => Ok(ProxyAction::InterruptPolicy {}),
            n => panic!("unexpected action: {}", n),
        }
    }

    pub async fn open(
        &self,
        id: u64,
        params: &VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS,
        event: &Event,
    ) -> Result<()> {
        let output = unsafe {
            let handle = event.as_handle().as_raw_handle() as usize as u64;
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_OPEN_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_OPEN_CHANNEL_INPUT {
                    ChannelId: id,
                    OpenParameters: *params,
                    VmmSignalEvent: handle,
                }),
                StaticIoctlBuffer(zeroed::<proxyioctl::VMBUS_PROXY_OPEN_CHANNEL_OUTPUT>()),
            )
            .await?
            .0
        };
        NTSTATUS(output.Status).ok()
    }

    pub async fn close(&self, id: u64) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_CLOSE_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_CLOSE_CHANNEL_INPUT { ChannelId: id }),
                (),
            )
            .await
        }
    }

    pub async fn release(&self, id: u64) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_RELEASE_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_RELEASE_CHANNEL_INPUT { ChannelId: id }),
                (),
            )
            .await
        }
    }

    pub async fn create_gpadl(
        &self,
        id: u64,
        gpadl_id: u32,
        range_count: u32,
        range_buf: &[u8],
    ) -> Result<()> {
        let mut buf = Vec::new();
        let header = proxyioctl::VMBUS_PROXY_CREATE_GPADL_INPUT {
            ChannelId: id,
            GpadlId: gpadl_id,
            RangeCount: range_count,
            RangeBufferOffset: size_of::<proxyioctl::VMBUS_PROXY_CREATE_GPADL_INPUT>() as u32,
            RangeBufferSize: range_buf.len() as u32,
        };
        buf.extend_from_slice(header.as_bytes());
        buf.extend_from_slice(range_buf);
        unsafe {
            self.ioctl(proxyioctl::IOCTL_VMBUS_PROXY_CREATE_GPADL, buf, ())
                .await
        }
    }

    pub async fn delete_gpadl(&self, id: u64, gpadl_id: u32) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_DELETE_GPADL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_DELETE_GPADL_INPUT {
                    ChannelId: id,
                    GpadlId: gpadl_id,
                }),
                (),
            )
            .await
        }
    }

    pub fn run_channel(&self, id: u64) -> Result<()> {
        unsafe {
            // This is a synchronous operation, so don't use the async IO infrastructure.
            let input = proxyioctl::VMBUS_PROXY_RUN_CHANNEL_INPUT { ChannelId: id };
            let mut bytes = 0;
            DeviceIoControl(
                HANDLE(self.file.get().as_raw_handle()),
                proxyioctl::IOCTL_VMBUS_PROXY_RUN_CHANNEL,
                Some(std::ptr::from_ref(&input).cast()),
                size_of_val(&input) as u32,
                None,
                0,
                Some(&mut bytes),
                None,
            )?;
        };
        Ok(())
    }
}
