// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the bochs / QEMU debugcon device.
//!
//! This is a zero-configuration, output-only serial device, which should only
//! be used for debugging (hence the name). It offers no flow control
//! mechanisms, or any method of reading data into the Guest.


pub mod resolver;

use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use futures::AsyncWrite;
use inspect::InspectMut;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use vmcore::device_state::ChangeDeviceState;

// the bound here is entirely arbitrary. we pick a relatively large number, just
// in case some guest decides to try and dump a _lot_ of data over the debugcon
// all at once.
const TX_BUFFER_MAX: usize = 1024 * 1024; // 1MB

/// A debugcon serial port emulator.
#[derive(InspectMut)]
pub struct SerialDebugcon {
    // Fixed configuration
    io_port: u16,
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),

    // Runtime glue
    #[inspect(mut)]
    io: Box<dyn SerialIo>,

    // Volatile state
    #[inspect(with = "VecDeque::len")]
    tx_buffer: VecDeque<u8>,
    #[inspect(skip)]
    tx_waker: Option<Waker>,
}

impl SerialDebugcon {
    /// Returns a new emulator instance.
    pub fn new(port: u16, io: Box<dyn SerialIo>) -> Self {
        Self {
            io_port: port,
            io_region: ("debugcon", port..=port),
            io,
            tx_buffer: VecDeque::new(),
            tx_waker: None,
        }
    }

    /// Synchronize interrupt and waker state with device state.
    fn sync(&mut self) {
        // Wake to poll if there are any bytes to write.
        if !self.tx_buffer.is_empty() {
            if let Some(waker) = self.tx_waker.take() {
                waker.wake();
            }
        }
    }

    fn poll_tx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while !self.tx_buffer.is_empty() {
            let (buf, _) = self.tx_buffer.as_slices();
            match ready!(Pin::new(&mut self.io).poll_write(cx, buf)) {
                Ok(n) => {
                    assert_ne!(n, 0);
                    self.tx_buffer.drain(..n);
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    tracing::info!("debugcon output broken pipe");
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        len = buf.len(),
                        error = &err as &dyn std::error::Error,
                        "debugcon write failed, dropping data"
                    );
                    self.tx_buffer.drain(..buf.len());
                }
            }
        }
        // Wait for more bytes to write.
        self.tx_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl ChangeDeviceState for SerialDebugcon {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.tx_buffer.clear();
    }
}

impl ChipsetDevice for SerialDebugcon {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for SerialDebugcon {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        let _ = self.poll_tx(cx);
        self.sync();
    }
}

impl PortIoIntercept for SerialDebugcon {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port != self.io_port {
            return IoResult::Err(IoError::InvalidRegister);
        }

        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        // this is a magic constant which the guest can use to detect the
        // presence of the debugcon device. Its value is fixed, and matches the
        // value set by the original debugcon implementation in bochs.
        data[0] = 0xe9;

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if io_port != self.io_port {
            return IoResult::Err(IoError::InvalidRegister);
        }

        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if self.tx_buffer.len() >= TX_BUFFER_MAX {
            tracing::debug!("debugcon buffer overrun, dropping output data");
            return IoResult::Ok;
        }

        self.tx_buffer.push_back(data[0]);

        // HACK: work around the fact that in openvmm, the console is in raw mode.
        //
        // FUTURE: this should be configurable, in case folks need 1:1 faithful
        // debugcon output.
        if data[0] == b'\n' {
            self.tx_buffer.push_back(b'\r')
        }

        self.sync();

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

mod save_restore {
    use crate::SerialDebugcon;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for SerialDebugcon {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let NoSavedState = state;
            Ok(())
        }
    }
}
