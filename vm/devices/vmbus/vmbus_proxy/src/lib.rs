// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]
// UNSAFETY: Calling vmbus proxy ioctls.
#![expect(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use futures::poll;
use guestmem::GuestMemory;
use mesh::MeshPayload;
use ntapi::ntioapi::NtOpenFile;
use pal::windows::chk_status;
use pal::windows::UnicodeString;
use pal_async::driver::Driver;
use pal_async::windows::overlapped::IoBuf;
use pal_async::windows::overlapped::IoBufMut;
use pal_async::windows::overlapped::OverlappedFile;
use pal_event::Event;
use std::ffi::c_void;
use std::mem::zeroed;
use std::os::windows::prelude::*;
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmbusioctl::VMBUS_CHANNEL_OFFER;
use vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use winapi::shared::ntdef::OBJECT_ATTRIBUTES;
use winapi::shared::winerror::ERROR_CANCELLED;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winnt::GENERIC_ALL;
use winapi::um::winnt::SYNCHRONIZE;
use zerocopy::IntoBytes;

pub mod vmbusioctl {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_snake_case,
        non_upper_case_globals,
        clippy::upper_case_acronyms
    )]

    use vmbus_core::protocol::UserDefinedData;
    use winapi::shared::guiddef::GUID;

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct VMBUS_CHANNEL_OFFER {
        pub InterfaceType: GUID,
        pub InterfaceInstance: GUID,
        pub InterruptLatencyIn100nsUnits: u64,
        pub ChannelFlags: u16,
        pub MmioMegabytes: u16,         // in bytes * 1024 * 1024
        pub MmioMegabytesOptional: u16, // mmio memory in addition to MmioMegabytes that is optional
        pub SubChannelIndex: u16,
        pub TargetVtl: u8,
        pub Reserved: [u8; 7],
        pub UserDefined: UserDefinedData,
    }

    pub const VMBUS_CHANNEL_ENUMERATE_DEVICE_INTERFACE: u16 = 1;
    pub const VMBUS_CHANNEL_NAMED_PIPE_MODE: u16 = 0x10;
    pub const VMBUS_CHANNEL_LOOPBACK_OFFER: u16 = 0x100;
    pub const VMBUS_CHANNEL_REQUEST_MONITORED_NOTIFICATION: u16 = 0x400;
    pub const VMBUS_CHANNEL_FORCE_NEW_CHANNEL: u16 = 0x1000;
    pub const VMBUS_CHANNEL_TLNPI_PROVIDER_OFFER: u16 = 0x2000;

    pub const VMBUS_PIPE_TYPE_BYTE: u32 = 0;
    pub const VMBUS_PIPE_TYPE_MESSAGE: u32 = 4;
    pub const VMBUS_PIPE_TYPE_RAW: u32 = 8;

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
        pub RingBufferGpadlHandle: u32,
        pub DownstreamRingBufferPageOffset: u32,
        pub NodeNumber: u16,
    }
}

mod proxyioctl {
    #![allow(
        dead_code,
        non_snake_case,
        non_upper_case_globals,
        non_camel_case_types,
        clippy::upper_case_acronyms
    )]

    use super::vmbusioctl::VMBUS_CHANNEL_OFFER;
    use super::vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
    use winapi::um::winioctl::FILE_DEVICE_UNKNOWN;
    use winapi::um::winioctl::FILE_READ_ACCESS;
    use winapi::um::winioctl::FILE_WRITE_ACCESS;
    use winapi::um::winioctl::METHOD_BUFFERED;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    const fn CTL_CODE(DeviceType: u32, Function: u32, Method: u32, Access: u32) -> u32 {
        (DeviceType << 16) | (Access << 14) | (Function << 2) | Method
    }

    const fn VMBUS_PROXY_IOCTL(code: u32) -> u32 {
        CTL_CODE(
            FILE_DEVICE_UNKNOWN,
            code,
            METHOD_BUFFERED,
            FILE_READ_ACCESS | FILE_WRITE_ACCESS,
        )
    }

    pub const IOCTL_VMBUS_PROXY_SET_VM_NAME: u32 = VMBUS_PROXY_IOCTL(0x1);
    pub const IOCTL_VMBUS_PROXY_SET_TOPOLOGY: u32 = VMBUS_PROXY_IOCTL(0x2);
    pub const IOCTL_VMBUS_PROXY_SET_MEMORY: u32 = VMBUS_PROXY_IOCTL(0x3);
    pub const IOCTL_VMBUS_PROXY_NEXT_ACTION: u32 = VMBUS_PROXY_IOCTL(0x4);
    pub const IOCTL_VMBUS_PROXY_OPEN_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x5);
    pub const IOCTL_VMBUS_PROXY_CLOSE_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x6);
    pub const IOCTL_VMBUS_PROXY_CREATE_GPADL: u32 = VMBUS_PROXY_IOCTL(0x7);
    pub const IOCTL_VMBUS_PROXY_DELETE_GPADL: u32 = VMBUS_PROXY_IOCTL(0x8);
    pub const IOCTL_VMBUS_PROXY_RELEASE_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0x9);
    pub const IOCTL_VMBUS_PROXY_RUN_CHANNEL: u32 = VMBUS_PROXY_IOCTL(0xa);

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_SET_VM_NAME_INPUT {
        pub VmId: [u8; 16],
        pub NameLength: u16,
        pub NameOffset: u16,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_SET_TOPOLOGY_INPUT {
        pub NodeCount: u32,
        pub VpCount: u32,
        pub NodesOffset: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_SET_MEMORY_INPUT {
        pub BaseAddress: u64,
        pub Size: u64,
    }

    pub const VmbusProxyActionTypeOffer: u32 = 1;
    pub const VmbusProxyActionTypeRevoke: u32 = 2;
    pub const VmbusProxyActionTypeInterruptPolicy: u32 = 3;

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_NEXT_ACTION_OUTPUT {
        pub Type: u32,
        pub ChannelId: u64,
        pub u: VMBUS_PROXY_NEXT_ACTION_OUTPUT_union,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub union VMBUS_PROXY_NEXT_ACTION_OUTPUT_union {
        pub Offer: VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_Offer,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_NEXT_ACTION_OUTPUT_union_Offer {
        pub Offer: VMBUS_CHANNEL_OFFER,
        pub DeviceIncomingRingEvent: u64, // BUGBUG: HANDLE
        pub DeviceOutgoingRingEvent: u64, // BUGBUG: HANDLE
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_OPEN_CHANNEL_INPUT {
        pub ChannelId: u64,
        pub OpenParameters: VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS,
        pub VmmSignalEvent: u64, // BUGBUG: HANDLE
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_OPEN_CHANNEL_OUTPUT {
        pub Status: i32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_CLOSE_CHANNEL_INPUT {
        pub ChannelId: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout)]
    pub struct VMBUS_PROXY_CREATE_GPADL_INPUT {
        pub ChannelId: u64,
        pub GpadlId: u32,
        pub RangeCount: u32,
        pub RangeBufferOffset: u32,
        pub RangeBufferSize: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_DELETE_GPADL_INPUT {
        pub ChannelId: u64,
        pub GpadlId: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_RELEASE_CHANNEL_INPUT {
        pub ChannelId: u64,
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct VMBUS_PROXY_RUN_CHANNEL_INPUT {
        pub ChannelId: u64,
    }
}

pub type Error = std::io::Error;

/// A VM handle the VMBus proxy driver.
#[derive(Debug, MeshPayload)]
pub struct ProxyHandle(std::fs::File);

impl ProxyHandle {
    /// Creates a new VM handle.
    pub fn new() -> Result<Self, Error> {
        let mut pathu: UnicodeString = "\\Device\\VmbusProxy".try_into().expect("string fits");
        let mut oa = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: null_mut(),
            ObjectName: pathu.as_mut_ptr(),
            Attributes: 0,
            SecurityDescriptor: null_mut(),
            SecurityQualityOfService: null_mut(),
        };
        // SAFETY: calling API according to docs.
        unsafe {
            let mut iosb = zeroed();
            let mut handle = null_mut();
            chk_status(NtOpenFile(
                &mut handle,
                GENERIC_ALL | SYNCHRONIZE,
                &mut oa,
                &mut iosb,
                0,
                0,
            ))?;
            Ok(Self(std::fs::File::from_raw_handle(handle)))
        }
    }
}

impl From<OwnedHandle> for ProxyHandle {
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
    pub fn new(driver: &dyn Driver, handle: ProxyHandle) -> Result<Self, std::io::Error> {
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

    async unsafe fn ioctl<In, Out>(&self, code: u32, input: In, output: Out) -> std::io::Result<Out>
    where
        In: IoBufMut,
        Out: IoBufMut,
    {
        // Don't issue new IO if the cancel() method has been called.
        if self.cancelled.load(Ordering::Acquire) {
            tracing::trace!("ioctl cancelled before issued");
            return Err(std::io::Error::from_raw_os_error(ERROR_CANCELLED as i32));
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

    pub async fn set_memory(&mut self, guest_memory: &GuestMemory) -> Result<(), std::io::Error> {
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

    pub async fn next_action(&self) -> Result<ProxyAction, Error> {
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
    ) -> Result<(), Error> {
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
        chk_status(output.Status)?;
        Ok(())
    }

    pub async fn close(&self, id: u64) -> Result<(), Error> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_CLOSE_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_CLOSE_CHANNEL_INPUT { ChannelId: id }),
                (),
            )
            .await
        }
    }

    pub async fn release(&self, id: u64) -> Result<(), Error> {
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
    ) -> Result<(), Error> {
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

    pub async fn delete_gpadl(&self, id: u64, gpadl_id: u32) -> Result<(), Error> {
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

    pub fn run_channel(&self, id: u64) -> Result<(), Error> {
        unsafe {
            // This is a synchronous operation, so don't use the async IO infrastructure.
            let input = proxyioctl::VMBUS_PROXY_RUN_CHANNEL_INPUT { ChannelId: id };
            let mut bytes = 0;
            if DeviceIoControl(
                self.file.get().as_raw_handle(),
                proxyioctl::IOCTL_VMBUS_PROXY_RUN_CHANNEL,
                std::ptr::from_ref::<proxyioctl::VMBUS_PROXY_RUN_CHANNEL_INPUT>(&input)
                    as *mut c_void,
                size_of_val(&input) as u32,
                null_mut(),
                0,
                &mut bytes,
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
        };
        Ok(())
    }
}
