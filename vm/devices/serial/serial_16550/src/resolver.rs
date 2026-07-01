// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for a serial 16550 UART chipset device.

use crate::Serial16550;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use serial_16550_resources::Serial16550DeviceHandle;
use serial_core::SerialIo;
use serial_core::resources::ResolveSerialBackendParams;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::vm_task::VmTaskDriver;

/// The resource resolver for [`Serial16550`].
pub struct Serial16550Resolver;

declare_static_async_resolver! {
    Serial16550Resolver,
    (ChipsetDeviceHandleKind, Serial16550DeviceHandle),
}

/// An error resolving a [`Serial16550DeviceHandle`].
#[expect(missing_docs)]
#[derive(Debug, Error)]
pub enum Resolve16550Error {
    #[error("failed to resolve io backend")]
    ResolveBackend(#[source] ResolveError),
    #[error("failed to configure serial device")]
    Configuration(#[source] super::ConfigurationError),
}

fn apply_debugger_mode(
    debugger_mode: bool,
    driver: VmTaskDriver,
    device_name: &str,
    io: Box<dyn SerialIo>,
) -> Box<dyn SerialIo> {
    if debugger_mode {
        Box::new(serial_core::debugger::DebuggerRelay::new(
            driver,
            device_name,
            io,
        ))
    } else {
        io
    }
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Serial16550DeviceHandle>
    for Serial16550Resolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Resolve16550Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Serial16550DeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let io = resolver
            .resolve(
                resource.io,
                ResolveSerialBackendParams {
                    driver: Box::new(input.task_driver_source.simple()),
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(Resolve16550Error::ResolveBackend)?;

        let interrupt = input
            .configure
            .new_line(IRQ_LINE_SET, "interrupt", resource.irq);

        let io = apply_debugger_mode(
            resource.debugger_mode,
            input.task_driver_source.simple(),
            input.device_name,
            io.0.into_io(),
        );

        let device = Serial16550::new(
            input.device_name.to_string(),
            resource.base,
            resource.register_width,
            interrupt,
            io,
            resource.wait_for_rts,
        )
        .map_err(Resolve16550Error::Configuration)?;

        Ok(device.into())
    }
}

#[cfg(test)]
mod tests {
    use super::apply_debugger_mode;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use serial_core::SerialIo;
    use std::io;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    #[derive(InspectMut)]
    struct PendingWriteSerial;

    impl SerialIo for PendingWriteSerial {
        fn is_connected(&self) -> bool {
            true
        }

        fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncRead for PendingWriteSerial {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for PendingWriteSerial {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[async_test]
    async fn debugger_mode_wraps_backend(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

        let mut passthrough = apply_debugger_mode(
            false,
            driver_source.simple(),
            "serial",
            Box::new(PendingWriteSerial),
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut passthrough).poll_write(&mut cx, b"x"),
            Poll::Pending
        ));

        let mut debugger = apply_debugger_mode(
            true,
            driver_source.simple(),
            "serial",
            Box::new(PendingWriteSerial),
        );
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            Pin::new(&mut debugger).poll_write(&mut cx, b"x"),
            Poll::Ready(Ok(1))
        ));
    }
}
