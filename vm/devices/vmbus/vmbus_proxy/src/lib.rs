// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(windows)]
// UNSAFETY: Calling vmbus proxy ioctls.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

use futures::poll;
use guestmem::GuestMemory;
use guid::Guid;
use mesh::CancelContext;
use mesh::MeshPayload;
use pal::windows::ObjectAttributes;
use pal::windows::UnicodeStringRef;
use pal_async::driver::Driver;
use pal_async::windows::overlapped::IoBuf;
use pal_async::windows::overlapped::IoBufMut;
use pal_async::windows::overlapped::OverlappedFile;
use pal_event::Event;
use std::mem::ManuallyDrop;
use std::mem::zeroed;
use std::num::NonZeroU32;
use std::os::windows::prelude::*;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbusioctl::VMBUS_CHANNEL_OFFER;
use vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use widestring::Utf16Str;
use widestring::utf16str;
use windows::Wdk::Storage::FileSystem::NtOpenFile;
use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
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
    file: ManuallyDrop<OverlappedFile>,
    // NOTE: This must come after `file` so that it is not released until `file`
    // is closed.
    guest_memory: Option<GuestMemory>,
    cancel: CancelContext,
}

impl Drop for VmbusProxy {
    fn drop(&mut self) {
        // SAFETY: VmbusProxy is being dropped so can no longer be used.
        let file = unsafe { ManuallyDrop::take(&mut self.file) };

        // Extract the inner file to dissociate the I/O completion port. This is required so the
        // file object can be reused in case of handle brokering.
        file.into_inner();
    }
}

#[derive(Debug)]
pub enum ProxyAction {
    Offer {
        id: u64,
        offer: VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
        outgoing_event: Option<Event>,
        device_order: Option<NonZeroU32>,
    },
    Revoke {
        id: u64,
    },
    InterruptPolicy {},
    TlConnectResult {
        result: HvsockConnectResult,
        vtl: u8,
    },
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
    pub fn new(driver: &dyn Driver, handle: ProxyHandle, ctx: CancelContext) -> Result<Self> {
        // SAFETY: While the handle is cloned from the vdev, VmbusProxy is the only place where
        // async IO is performed using this file object.
        let file = unsafe { OverlappedFile::new(driver, handle.0)? };
        Ok(Self {
            file: ManuallyDrop::new(file),
            guest_memory: None,
            cancel: ctx,
        })
    }

    pub fn handle(&self) -> BorrowedHandle<'_> {
        self.file.get().as_handle()
    }

    async unsafe fn ioctl<In, Out>(&self, code: u32, input: In, output: Out) -> Result<Out>
    where
        In: IoBufMut,
        Out: IoBufMut,
    {
        // SAFETY: guaranteed by caller.
        let (r, (_, output)) = unsafe { self.file.ioctl(code, input, output).await };
        let size = r?;
        assert_eq!(size, output.len(), "ioctl returned unexpected size");
        Ok(output)
    }

    async unsafe fn ioctl_cancellable<In, Out>(
        &self,
        code: u32,
        input: In,
        output: Out,
    ) -> Result<Out>
    where
        In: IoBufMut,
        Out: IoBufMut,
    {
        // Don't issue new IO if the cancel context has already been cancelled.
        let mut cancel = self.cancel.clone();
        if cancel.is_cancelled() {
            tracing::trace!("ioctl cancelled before issued");
            return Err(ERROR_OPERATION_ABORTED.into());
        }

        // SAFETY: guaranteed by caller.
        let mut ioctl = unsafe { self.file.ioctl(code, input, output) };
        let (r, (_, output)) = match poll!(&mut ioctl) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                match cancel.until_cancelled(&mut ioctl).await {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::trace!("ioctl cancelled after issued");
                        ioctl.cancel();
                        // Even when cancelled, we must wait to complete the IO so buffers aren't released
                        // while still in use.
                        ioctl.await
                    }
                }
            }
        };

        let size = r?;
        assert_eq!(size, output.len(), "ioctl returned unexpected size");
        Ok(output)
    }

    pub async fn set_memory(&mut self, guest_memory: &GuestMemory) -> Result<()> {
        assert!(self.guest_memory.is_none());
        let (base, len) = guest_memory.full_mapping().ok_or_else(|| {
            std::io::Error::other("vmbusproxy not supported without mapped memory")
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
            self.ioctl_cancellable(
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
                    id: output.ProxyId,
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
                    device_order: NonZeroU32::new(output.u.Offer.DeviceOrder),
                })
            },
            proxyioctl::VmbusProxyActionTypeRevoke => {
                Ok(ProxyAction::Revoke { id: output.ProxyId })
            }
            proxyioctl::VmbusProxyActionTypeInterruptPolicy => Ok(ProxyAction::InterruptPolicy {}),
            proxyioctl::VmbusProxyActionTypeTlConnectResult => unsafe {
                Ok(ProxyAction::TlConnectResult {
                    result: HvsockConnectResult {
                        endpoint_id: output.u.TlConnectResult.EndpointId.into(),
                        service_id: output.u.TlConnectResult.ServiceId.into(),
                        success: output.u.TlConnectResult.Status.is_ok(),
                    },
                    vtl: output.u.TlConnectResult.Vtl,
                })
            },
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
                    ProxyId: id,
                    Padding: 0,
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
    pub async fn set_interrupt(&self, id: u64, event: &Event) -> Result<()> {
        unsafe {
            let handle = event.as_handle().as_raw_handle() as usize as u64;
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_RESTORE_SET_INTERRUPT,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_SET_INTERRUPT_INPUT {
                    ProxyId: id,
                    VmmSignalEvent: handle,
                }),
                (),
            )
            .await?
        };
        Ok(())
    }

    pub async fn restore(
        &self,
        interface_type: Guid,
        interface_instance: Guid,
        subchannel_index: u16,
        target_vtl: u8,
        open_params: Option<VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS>,
        gpadls: impl Iterator<Item = Gpadl<'_>>,
    ) -> Result<u64> {
        let mut buffer = Vec::new();
        let mut header = proxyioctl::VMBUS_PROXY_RESTORE_CHANNEL_INPUT {
            InterfaceType: interface_type,
            InterfaceInstance: interface_instance,
            SubchannelIndex: subchannel_index,
            TargetVtl: target_vtl,
            GpadlCount: 0,
            OpenParameters: open_params.unwrap_or_default(),
            Open: open_params.is_some().into(),
        };

        // Leave space for the header.
        const HEADER_LEN: usize = size_of::<proxyioctl::VMBUS_PROXY_RESTORE_CHANNEL_INPUT>();
        buffer.resize(HEADER_LEN, 0);

        // Add GPADLs to the buffer and count them.
        for gpadl in gpadls {
            header.GpadlCount += 1;
            Self::add_gpadl(
                &mut buffer,
                0, // Not used for restoring.
                gpadl.gpadl_id,
                gpadl.range_count,
                gpadl.range_buffer.as_bytes(),
            );
        }

        // Copy the header now that the GPADL count is known.
        buffer[..HEADER_LEN].copy_from_slice(header.as_bytes());
        Ok(unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_RESTORE_CHANNEL,
                buffer,
                StaticIoctlBuffer(zeroed::<proxyioctl::VMBUS_PROXY_RESTORE_CHANNEL_OUTPUT>()),
            )
            .await?
            .0
            .ProxyId
        })
    }

    pub async fn revoke_unclaimed_channels(&self) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_REVOKE_UNCLAIMED_CHANNELS,
                (),
                (),
            )
            .await
        }
    }

    pub async fn close(&self, id: u64) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_CLOSE_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_CLOSE_CHANNEL_INPUT { ProxyId: id }),
                (),
            )
            .await
        }
    }

    pub async fn release(&self, id: u64) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_RELEASE_CHANNEL,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_RELEASE_CHANNEL_INPUT { ProxyId: id }),
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
        Self::add_gpadl(&mut buf, id, gpadl_id, range_count, range_buf);
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
                    ProxyId: id,
                    GpadlId: gpadl_id,
                    Padding: 0,
                }),
                (),
            )
            .await
        }
    }

    pub async fn tl_connect_request(&self, request: &HvsockConnectRequest, vtl: u8) -> Result<()> {
        unsafe {
            self.ioctl(
                proxyioctl::IOCTL_VMBUS_PROXY_TL_CONNECT_REQUEST,
                StaticIoctlBuffer(proxyioctl::VMBUS_PROXY_TL_CONNECT_REQUEST_INPUT {
                    EndpoindId: request.endpoint_id.into(),
                    ServiceId: request.service_id.into(),
                    SiloId: request.silo_id.into(),
                    Flags: proxyioctl::VMBUS_PROXY_TL_CONNECT_REQUEST_FLAGS::new()
                        .with_hosted_silo_unaware(request.hosted_silo_unaware),
                    Vtl: vtl,
                    Padding: [0; 3],
                }),
                (),
            )
            .await
        }
    }

    pub fn run_channel(&self, id: u64) -> Result<()> {
        unsafe {
            // This is a synchronous operation, so don't use the async IO infrastructure.
            let input = proxyioctl::VMBUS_PROXY_RUN_CHANNEL_INPUT { ProxyId: id };
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

    /// Adds GPADL ioctl data to a buffer.
    fn add_gpadl(
        buffer: &mut Vec<u8>,
        id: u64,
        gpadl_id: u32,
        range_count: u32,
        range_buffer: &[u8],
    ) {
        let header = proxyioctl::VMBUS_PROXY_CREATE_GPADL_INPUT {
            ProxyId: id,
            GpadlId: gpadl_id,
            RangeCount: range_count,
            RangeBufferOffset: size_of::<proxyioctl::VMBUS_PROXY_CREATE_GPADL_INPUT>() as u32,
            RangeBufferSize: range_buffer.len() as u32,
        };
        buffer.extend_from_slice(header.as_bytes());
        buffer.extend_from_slice(range_buffer);
    }
}

/// Represents data to be restored for a GPADL.
pub struct Gpadl<'a> {
    pub gpadl_id: u32,
    pub range_count: u32,
    pub range_buffer: &'a [u64],
}
