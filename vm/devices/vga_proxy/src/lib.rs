// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A fake VGA device that proxies all PCI accesses to the emulated host device.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::InspectMut;
use std::future::Future;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::task::Waker;
use vmcore::device_state::ChangeDeviceState;

pub struct VgaProxyDevice {
    pci_cfg_proxy: Arc<dyn ProxyVgaPciCfgAccess>,
    pending_actions: Vec<DeferredAction>,
    waker: Option<Waker>,
    _host_port_handles: Vec<Box<dyn Send>>,
}

enum DeferredAction {
    Read(DeferredRead, Pin<Box<dyn Future<Output = u32> + Send>>),
    Write(DeferredWrite, Pin<Box<dyn Future<Output = ()> + Send>>),
}

static PORTS: &[(&str, RangeInclusive<u16>)] = &[
    ("s3", 0x4ae8..=0x4ae9),
    ("mda", 0x3b0..=0x3bf),
    ("vga", 0x3c0..=0x3cf),
    ("cga", 0x3d0..=0x3df),
];

impl VgaProxyDevice {
    pub fn new(
        pci_cfg_proxy: Arc<dyn ProxyVgaPciCfgAccess>,
        register: &dyn RegisterHostIoPortFastPath,
    ) -> Self {
        // Register the IO ports with the host so that the hypervisor sends the
        // IOs directly there.
        let host_port_handles = PORTS
            .iter()
            .map(|(_, range)| register.register(range.clone()))
            .collect();

        Self {
            pci_cfg_proxy,
            pending_actions: Vec::new(),
            waker: None,
            _host_port_handles: host_port_handles,
        }
    }
}

/// Trait for registering host IO ports.
pub trait RegisterHostIoPortFastPath {
    /// Registers ports in `range` to go directly to the host.
    ///
    /// When the return value is dropped, the ports will be unregistered.
    #[must_use]
    fn register(&self, range: RangeInclusive<u16>) -> Box<dyn Send>;
}

#[async_trait::async_trait]
pub trait ProxyVgaPciCfgAccess: Send + Sync {
    async fn vga_proxy_pci_read(&self, offset: u16) -> u32;
    async fn vga_proxy_pci_write(&self, offset: u16, value: u32);
}

impl ChangeDeviceState for VgaProxyDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        // No state is currently stored here, so no work is needed for reset.
        // The host side device will reset its own state for us.
    }
}

impl ChipsetDevice for VgaProxyDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PciConfigSpace for VgaProxyDevice {
    fn pci_cfg_read(&mut self, offset: u16, _value: &mut u32) -> IoResult {
        tracing::trace!(?offset, "VGA proxy read");
        let (read, token) = defer_read();

        let fut = {
            let proxy = self.pci_cfg_proxy.clone();
            async move { proxy.vga_proxy_pci_read(offset).await }
        };

        self.pending_actions
            .push(DeferredAction::Read(read, Box::pin(fut)));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        IoResult::Defer(token)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        tracing::trace!(?offset, ?value, "VGA proxy write");
        let (write, token) = defer_write();

        let fut = {
            let proxy = self.pci_cfg_proxy.clone();
            async move { proxy.vga_proxy_pci_write(offset, value).await }
        };

        self.pending_actions
            .push(DeferredAction::Write(write, Box::pin(fut)));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        IoResult::Defer(token)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 8, 0)) // to match legacy Hyper-V behavior
    }
}

impl PortIoIntercept for VgaProxyDevice {
    fn io_read(&mut self, io_port: u16, _data: &mut [u8]) -> IoResult {
        // This access extends beyond the registered IO port and was trapped
        // here instead of going to the host. Fail it.
        tracelimit::warn_ratelimited!(io_port, "invalid straddling vga write");
        IoResult::Err(IoError::InvalidAccessSize)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // This access extends beyond the registered IO port and was trapped
        // here instead of going to the host. Fail it.
        tracelimit::warn_ratelimited!(io_port, ?data, "invalid straddling vga write");
        IoResult::Err(IoError::InvalidAccessSize)
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        PORTS
    }
}

impl PollDevice for VgaProxyDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.waker = Some(cx.waker().clone());
        self.pending_actions = std::mem::take(&mut self.pending_actions)
            .into_iter()
            .filter_map(|action| match action {
                DeferredAction::Read(dr, mut fut) => {
                    if let Poll::Ready(value) = fut.as_mut().poll(cx) {
                        tracing::trace!(value, "VGA proxy read complete");
                        dr.complete(&value.to_ne_bytes());
                        None
                    } else {
                        Some(DeferredAction::Read(dr, fut))
                    }
                }
                DeferredAction::Write(dw, mut fut) => {
                    if let Poll::Ready(()) = fut.as_mut().poll(cx) {
                        tracing::trace!("VGA proxy write complete");
                        dw.complete();
                        None
                    } else {
                        Some(DeferredAction::Write(dw, fut))
                    }
                }
            })
            .collect();
    }
}

impl InspectMut for VgaProxyDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    // No state is stored on the proxy device, and so we don't need to do
    // anything here to enable save/restore.
    //
    // The host side device will save and restore its own state for us.

    impl SaveRestore for VgaProxyDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
