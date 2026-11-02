// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt-only ACPI generic event device for a control-method power button.

use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::poll_device::PollDevice;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::acpi_power_button::AcpiPowerButtonDeviceHandle;
use chipset_resources::acpi_power_button::PowerButtonRpc;
use futures::StreamExt;
use inspect::InspectMut;
use std::convert::Infallible;
use std::task::Context;
use std::task::Poll;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

#[derive(InspectMut)]
struct AcpiPowerButton {
    interrupt: LineInterrupt,
    #[inspect(skip)]
    recv: mesh::Receiver<PowerButtonRpc>,
}

impl ChipsetDevice for AcpiPowerButton {
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for AcpiPowerButton {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        while let Poll::Ready(Some(PowerButtonRpc::Press(rpc))) = self.recv.poll_next_unpin(cx) {
            self.interrupt.set_level(true);
            self.interrupt.set_level(false);
            rpc.complete(());
        }
    }
}

impl ChangeDeviceState for AcpiPowerButton {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl SaveRestore for AcpiPowerButton {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
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
        Ok(AcpiPowerButton {
            interrupt: input
                .configure
                .new_line(IRQ_LINE_SET, "power-button", resource.irq),
            recv: resource.recv,
        }
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

    #[derive(Default)]
    struct Interrupts(Mutex<Vec<(u32, bool)>>);

    impl LineSetTarget for Interrupts {
        fn set_irq(&self, vector: u32, high: bool) {
            self.0.lock().push((vector, high));
        }
    }

    #[test]
    fn presses_pulse_and_acknowledge_without_powering_off() {
        futures::executor::block_on(async {
            let interrupts = Arc::new(Interrupts::default());
            let (send, recv) = mesh::channel();
            let mut button = AcpiPowerButton {
                interrupt: LineInterrupt::new_with_target("button", interrupts.clone(), 10),
                recv,
            };
            button.start();
            button.reset().await;
            let state = button.save().unwrap();
            button.restore(state).unwrap();
            assert!(interrupts.0.lock().is_empty());

            for _ in 0..2 {
                let mut response = std::pin::pin!(send.call(PowerButtonRpc::Press, ()));
                assert!(futures::poll!(&mut response).is_pending());
                futures::future::poll_fn(|cx| {
                    button.poll_device(cx);
                    Poll::Ready(())
                })
                .await;
                response.await.unwrap();
                assert_eq!(*interrupts.0.lock(), [(10, true), (10, false)]);
                interrupts.0.lock().clear();
            }
            button.stop().await;
            assert!(interrupts.0.lock().is_empty());
        });
    }
}
