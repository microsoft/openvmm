// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ACPI generic event device for a control-method power button.
//!
//! The 32-bit MMIO status register exposes a pending bit (bit 0). Presses
//! coalesce until the guest writes one to that bit, deasserting the interrupt.

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::acpi_power_button::ACPI_POWER_BUTTON_MMIO_BASE;
use chipset_resources::acpi_power_button::AcpiPowerButtonDeviceHandle;
use chipset_resources::acpi_power_button::PowerButtonRpc;
use futures::StreamExt;
use inspect::InspectMut;
use std::convert::Infallible;
use std::ops::RangeInclusive;
use std::task::Context;
use std::task::Poll;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

#[derive(InspectMut)]
struct AcpiPowerButton {
    interrupt: LineInterrupt,
    pending: bool,
    #[inspect(skip)]
    mmio_region: (&'static str, RangeInclusive<u64>),
    #[inspect(skip)]
    recv: mesh::Receiver<PowerButtonRpc>,
}

impl AcpiPowerButton {
    fn new(interrupt: LineInterrupt, recv: mesh::Receiver<PowerButtonRpc>) -> Self {
        let base = u64::from(ACPI_POWER_BUTTON_MMIO_BASE);
        Self {
            interrupt,
            pending: false,
            mmio_region: ("status", base..=base + 3),
            recv,
        }
    }

    fn set_pending(&mut self, pending: bool) {
        self.pending = pending;
        self.interrupt.set_level(pending);
    }
}

impl ChipsetDevice for AcpiPowerButton {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl MmioIntercept for AcpiPowerButton {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        if address != u64::from(ACPI_POWER_BUTTON_MMIO_BASE) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        if data.len() != size_of::<u32>() {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        data.copy_from_slice(&u32::from(self.pending).to_le_bytes());
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        if address != u64::from(ACPI_POWER_BUTTON_MMIO_BASE) {
            return IoResult::Err(IoError::InvalidRegister);
        }
        let Ok(bytes) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        if u32::from_le_bytes(bytes) & 1 != 0 {
            self.set_pending(false);
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio_region)
    }
}

impl PollDevice for AcpiPowerButton {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        while let Poll::Ready(Some(PowerButtonRpc::Press(rpc))) = self.recv.poll_next_unpin(cx) {
            self.set_pending(true);
            rpc.complete(());
        }
    }
}

impl ChangeDeviceState for AcpiPowerButton {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.set_pending(false);
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.acpi_power_button")]
        pub struct SavedState {
            #[mesh(1)]
            pub pending: bool,
        }
    }

    impl SaveRestore for AcpiPowerButton {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                interrupt: _,
                pending,
                mmio_region: _,
                recv: _,
            } = *self;

            let saved_state = state::SavedState { pending };
            Ok(saved_state)
        }

        fn restore(&mut self, saved_state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { pending } = saved_state;
            self.set_pending(pending);
            Ok(())
        }
    }
}

/// Resolver for the ACPI power-button generic event device.
pub struct AcpiPowerButtonResolver;

declare_static_async_resolver! {
    AcpiPowerButtonResolver,
    (ChipsetDeviceHandleKind, AcpiPowerButtonDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, AcpiPowerButtonDeviceHandle>
    for AcpiPowerButtonResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Infallible;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: AcpiPowerButtonDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(AcpiPowerButton::new(
            input
                .configure
                .new_line(IRQ_LINE_SET, "power-button", resource.irq),
            resource.recv,
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::rpc::RpcSend;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vmcore::line_interrupt::LineSetTarget;
    use vmcore::save_restore::SaveRestore;

    #[derive(Default)]
    struct Interrupts(Mutex<Vec<(u32, bool)>>);

    impl LineSetTarget for Interrupts {
        fn set_irq(&self, vector: u32, high: bool) {
            self.0.lock().push((vector, high));
        }
    }

    #[test]
    fn presses_latch_until_guest_acknowledgement() {
        futures::executor::block_on(async {
            let interrupts = Arc::new(Interrupts::default());
            let (send, recv) = mesh::channel();
            let mut button = AcpiPowerButton::new(
                LineInterrupt::new_with_target("button", interrupts.clone(), 10),
                recv,
            );
            button.start();
            assert!(interrupts.0.lock().is_empty());

            let base = u64::from(ACPI_POWER_BUTTON_MMIO_BASE);
            assert_eq!(button.get_static_regions(), [("status", base..=base + 3)]);
            for _ in 0..2 {
                let mut response = std::pin::pin!(send.call(PowerButtonRpc::Press, ()));
                assert!(futures::poll!(&mut response).is_pending());
                futures::future::poll_fn(|cx| {
                    button.poll_device(cx);
                    Poll::Ready(())
                })
                .await;
                response.await.unwrap();
                assert_eq!(*interrupts.0.lock(), [(10, true)]);
                let mut status = [0; 4];
                button.mmio_read(base, &mut status).unwrap();
                assert_eq!(u32::from_le_bytes(status), 1);
                button.mmio_write(base, &0u32.to_le_bytes()).unwrap();
                button.mmio_write(base, &2u32.to_le_bytes()).unwrap();
                assert_eq!(*interrupts.0.lock(), [(10, true)]);
                button.mmio_write(base, &1u32.to_le_bytes()).unwrap();
                assert_eq!(*interrupts.0.lock(), [(10, true), (10, false)]);
                button.mmio_read(base, &mut status).unwrap();
                assert_eq!(u32::from_le_bytes(status), 0);
                interrupts.0.lock().clear();
            }
            button.stop().await;
            assert!(interrupts.0.lock().is_empty());
        });
    }

    #[test]
    fn pending_event_survives_stop_and_save_restore_and_reset_clears_it() {
        futures::executor::block_on(async {
            let interrupts = Arc::new(Interrupts::default());
            let mut button = AcpiPowerButton::new(
                LineInterrupt::new_with_target("button", interrupts.clone(), 10),
                mesh::channel().1,
            );
            button.set_pending(true);
            button.stop().await;
            let pending = button.save().unwrap();
            assert_eq!(*interrupts.0.lock(), [(10, true)]);
            button.start();
            assert_eq!(*interrupts.0.lock(), [(10, true)]);

            button.reset().await;
            assert!(!button.pending);
            let idle = button.save().unwrap();
            button.restore(pending).unwrap();
            assert!(button.pending);
            assert_eq!(*interrupts.0.lock(), [(10, true), (10, false), (10, true)]);
            button.restore(idle).unwrap();
            assert!(!button.pending);
            assert_eq!(interrupts.0.lock().last(), Some(&(10, false)));
        });
    }

    #[test]
    fn acknowledgement_preserves_other_sources_on_a_shared_line() {
        let interrupts = Arc::new(Interrupts::default());
        let interrupt = LineInterrupt::new_with_target("button", interrupts.clone(), 10);
        let other = interrupt.new_shared("other").unwrap();
        let mut button = AcpiPowerButton::new(interrupt, mesh::channel().1);
        other.set_level(true);
        button.set_pending(true);
        button.set_pending(true);
        button
            .mmio_write(u64::from(ACPI_POWER_BUTTON_MMIO_BASE), &1u32.to_le_bytes())
            .unwrap();
        assert!(!button.pending);
        assert!(interrupts.0.lock().iter().all(|&(_, high)| high));
        other.set_level(false);
        assert_eq!(interrupts.0.lock().last(), Some(&(10, false)));
    }

    #[test]
    fn invalid_mmio_accesses_do_not_clear_pending_event() {
        let mut button = AcpiPowerButton::new(LineInterrupt::detached(), mesh::channel().1);
        button.set_pending(true);
        let base = u64::from(ACPI_POWER_BUTTON_MMIO_BASE);
        for size in [0, 1, 2, 3, 5, 8] {
            let mut data = vec![0xff; size];
            assert!(matches!(
                button.mmio_read(base, &mut data),
                IoResult::Err(IoError::InvalidAccessSize)
            ));
            assert!(matches!(
                button.mmio_write(base, &data),
                IoResult::Err(IoError::InvalidAccessSize)
            ));
        }
        for address in [0, base - 1, base + 1, base + 4, u64::MAX] {
            assert!(matches!(
                button.mmio_read(address, &mut [0; 4]),
                IoResult::Err(IoError::InvalidRegister)
            ));
            assert!(matches!(
                button.mmio_write(address, &1u32.to_le_bytes()),
                IoResult::Err(IoError::InvalidRegister)
            ));
        }
        assert!(button.pending);
    }
}
