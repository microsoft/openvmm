// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Emulator for PL011 serial UART.
//!
//! This device does not fully implement the PL011 UART defined by ARM (e.g., it
//! is missing DMA support), and Linux interprets it as an SBSA-compatible UART
//! when it is enumerated by ACPI (even when we use ARM's PL011 ACPI CID). SBSA
//! only defines a subset of the UART registers, leaving the rest as vendor
//! specified.
//!
//! If you extend this emulator, do so only to make it closer to a real PL011;
//! if you want to add other vendor-specific behavior, do it in a separate
//! wrapping emulator.

#![forbid(unsafe_code)]

pub mod resolver;
mod spec;

use self::spec::ControlRegister;
use self::spec::DmaControlRegister;
use self::spec::FIFO_SIZE;
use self::spec::FifoLevelSelect;
use self::spec::FractionalBaudRateRegister;
use self::spec::InterruptFifoLevelSelectRegister;
use self::spec::InterruptRegister;
use self::spec::LineControlRegister;
use self::spec::REGISTERS_SIZE;
use self::spec::Register;
use self::spec::UARTPCELL_ID;
use self::spec::UARTPERIPH_ID;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

/// A PL011 serial port emulator.
#[derive(InspectMut)]
pub struct SerialPl011 {
    // Fixed configuration
    #[inspect(skip)]
    debug_name: String,
    #[inspect(skip)]
    mmio_region: (&'static str, RangeInclusive<u64>),
    /// Don't transmit until the guest sets RTS. This exists here for symmetry
    /// with the 16550 emulator, but it's not useful because this device is
    /// enumerated as an SBSA UART, which does not support the RTS bit (a full
    /// PL011 would).
    wait_for_rts: bool,

    // Runtime glue
    interrupt: LineInterrupt,
    #[inspect(mut)]
    io: Box<dyn SerialIo>,

    // Runtime book-keeping
    state: State,
    #[inspect(skip)]
    rx_waker: Option<Waker>,
    #[inspect(skip)]
    tx_waker: Option<Waker>,
    stats: SerialStats,
}

#[derive(Inspect, Default)]
struct SerialStats {
    rx_bytes: Counter,
    tx_bytes: Counter,
    rx_dropped: Counter,
    tx_dropped: Counter,
}

#[derive(Inspect)]
struct State {
    #[inspect(with = "VecDeque::len")]
    tx_buffer: VecDeque<u8>,
    #[inspect(with = "VecDeque::len")]
    rx_buffer: VecDeque<u8>,
    rx_overrun: bool,
    connected: bool,
    ilpr: u8,                               // UARTILPR
    ibrd: u16,                              // UARTIBRD
    fbrd: FractionalBaudRateRegister,       // UARTFBRD
    lcr: LineControlRegister,               // UARTLCR_H: u8,
    cr: ControlRegister,                    // UARTCR: u16,
    ifls: InterruptFifoLevelSelectRegister, // UARTIFLS: u16
    imsc: InterruptRegister,                // UARTIMSC
    ris: InterruptRegister, // UARTRIS: 16 holds currently asserted interrupts, only to be updated by UpdateInterrupts and writes to UARTCIR
    dmacr: DmaControlRegister, // UARTDMACR

    // Updating UARTIBRD or UARTFBRD requires a write to UARTLCR_H.
    // Thus, we need to store if we've seen a different value incase we ever see a UARTLCR_H write.
    new_ibrd: u16,
    new_fbrd: FractionalBaudRateRegister,
}

// A normal FIFO has only 16 bytes, but we get greater batching with these values.
const TX_BUFFER_MAX: usize = 256;
const RX_BUFFER_MAX: usize = 256;

/// An error returned by [`SerialPl011::new`].
#[derive(Debug, Error)]
pub enum ConfigurationError {
    /// The provided base address was not aligned to the register bank width.
    #[error("unaligned base address: {0}")]
    UnalignedBaseAddress(u64),
    /// The specified register with was invalid.
    #[error("invalid register width: {0}")]
    InvalidRegisterWidth(u8),
}

impl SerialPl011 {
    /// Returns a new emulator instance.
    ///
    /// `debug_name` is used to improve tracing statements. `base` is the base
    /// IO port and will be used for an IO region spanning 8 bytes.
    pub fn new(
        debug_name: String,
        base: u64,
        interrupt: LineInterrupt,
        io: Box<dyn SerialIo>,
    ) -> Result<Self, ConfigurationError> {
        if base & (REGISTERS_SIZE - 1) != 0 {
            return Err(ConfigurationError::UnalignedBaseAddress(base));
        }

        let mut this = Self {
            debug_name,
            mmio_region: ("registers", base..=base + (REGISTERS_SIZE - 1)),
            wait_for_rts: false,
            state: State::new(),
            interrupt,
            io,
            rx_waker: None,
            tx_waker: None,
            stats: Default::default(),
        };
        if this.io.is_connected() {
            this.state.connect();
        }
        this.sync();
        Ok(this)
    }

    /// Synchronize interrupt and waker state with device state.
    fn sync(&mut self) {
        // Wake to poll if there are any bytes to write.
        if !self.state.tx_buffer.is_empty() {
            if let Some(waker) = self.tx_waker.take() {
                waker.wake();
            }
        }

        // Reduce wakeups by waking to poll if the rx buffer is at least half empty.
        if self.state.should_poll_rx(self.wait_for_rts)
            && self.state.rx_buffer.len() <= RX_BUFFER_MAX / 2
        {
            if let Some(waker) = self.rx_waker.take() {
                waker.wake();
            }
        }

        // Synchronize the receive timeout interrupt. In hardware, this would
        // only raise after 32 bits worth of clock have expired and there is
        // data in the RX FIFO. But that's too hard, so just treat the clock as
        // expiring constantly.
        //
        // This means the guest can't really clear this interrupt as long as
        // there is data in the FIFO.
        self.state.ris.set_rt(!self.state.rx_buffer.is_empty());

        // Synchronize the interrupt output.
        self.interrupt.set_level(self.state.pending_interrupt());
    }

    fn poll_tx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while !self.state.tx_buffer.is_empty() {
            if !self.state.connected {
                // The backend is disconnected, so drop everything in the FIFO.
                self.stats.tx_dropped.add(self.state.tx_buffer.len() as u64);
                if self.state.tx_buffer.len() > self.state.tx_fifo_trigger() {
                    self.state.ris.set_tx(true);
                }
                self.state.tx_buffer.clear();
                break;
            }
            let (buf, _) = self.state.tx_buffer.as_slices();
            let old_len = self.state.tx_buffer.len();
            match ready!(Pin::new(&mut self.io).poll_write(cx, buf)) {
                Ok(n) => {
                    assert_ne!(n, 0);
                    self.state.tx_buffer.drain(..n);
                    self.stats.tx_bytes.add(n as u64);
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    tracing::info!(
                        port = self.debug_name,
                        "serial output broken pipe, disconnecting"
                    );
                    self.state.disconnect();
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        port = self.debug_name,
                        len = buf.len(),
                        error = &err as &dyn std::error::Error,
                        "serial write failed, dropping data"
                    );
                    self.stats.tx_dropped.add(buf.len() as u64);
                    self.state.tx_buffer.drain(..buf.len());
                }
            }
            let tx_fifo_trigger = self.state.tx_fifo_trigger();
            if old_len > tx_fifo_trigger && self.state.tx_buffer.len() <= tx_fifo_trigger {
                self.state.ris.set_tx(true);
            }
        }
        // Wait for more bytes to write.
        self.tx_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_rx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut buf = [0; RX_BUFFER_MAX];
        loop {
            if !self.state.connected {
                // Wait for reconnect.
                if let Err(err) = ready!(self.io.poll_connect(cx)) {
                    tracing::info!(
                        port = self.debug_name,
                        error = &err as &dyn std::error::Error,
                        "serial backend failure"
                    );
                    break Poll::Ready(());
                }
                tracing::trace!(port = self.debug_name, "serial connected");
                self.state.connect();
            }
            if !self.state.should_poll_rx(self.wait_for_rts) {
                // Wait for buffer space to read into, or to leave loopback mode.
                self.rx_waker = Some(cx.waker().clone());
                if let Err(err) = ready!(self.io.poll_disconnect(cx)) {
                    tracing::info!(
                        port = self.debug_name,
                        error = &err as &dyn std::error::Error,
                        "serial backend failure"
                    );
                    break Poll::Ready(());
                }
                tracing::trace!(port = self.debug_name, "serial disconnected");
                self.state.disconnect();
                continue;
            }
            let avail_space = RX_BUFFER_MAX - self.state.rx_buffer.len();
            let buf = &mut buf[..avail_space];
            match ready!(Pin::new(&mut self.io).poll_read(cx, buf)) {
                Ok(0) => {
                    tracing::trace!(port = self.debug_name, "serial disconnected");
                    self.state.disconnect();
                }
                Ok(n) => {
                    let rx_fifo_trigger = self.state.rx_fifo_trigger();
                    if self.state.rx_buffer.len() < rx_fifo_trigger
                        && self.state.rx_buffer.len() + n >= rx_fifo_trigger
                    {
                        self.state.ris.set_rx(true);
                    }
                    self.state.rx_buffer.extend(&buf[..n]);
                    self.stats.rx_bytes.add(n as u64);
                }
                Err(err) => {
                    tracing::error!(
                        port = self.debug_name,
                        error = &err as &dyn std::error::Error,
                        "failed to read serial input, disconnecting"
                    );
                    self.state.disconnect();
                    break Poll::Ready(());
                }
            }
        }
    }

    fn register(&self, addr: u64) -> Result<Register, IoError> {
        // All registers are 32 bits wide, and the SBSA spec requires aligned access.
        if addr & 3 != 0 {
            return Err(IoError::UnalignedAccess);
        }
        Ok(Register((addr & (REGISTERS_SIZE - 1)) as u16))
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let register = match self.register(addr) {
            Err(e) => return IoResult::Err(e),
            Ok(r) => r,
        };

        data.fill(0);
        let val: u16 = match register {
            Register::UARTDR => self.state.read_dr().into(),
            Register::UARTRSR => 0, // Status flags we don't care about, return zeros.
            Register::UARTFR => self.state.read_fr(),
            Register::UARTILPR => self.state.ilpr as u16,
            Register::UARTIBRD => self.state.ibrd,
            Register::UARTFBRD => u8::from(self.state.fbrd) as u16,
            Register::UARTLCR_H => u8::from(self.state.lcr) as u16,
            Register::UARTCR => u16::from(self.state.cr),
            Register::UARTIFLS => u16::from(self.state.ifls),
            Register::UARTIMSC => u16::from(self.state.imsc),
            Register::UARTRIS => u16::from(self.state.ris),
            Register::UARTMIS => u16::from(self.state.ris) & u16::from(self.state.imsc),
            Register::UARTDMACR => u16::from(self.state.dmacr),
            Register::UARTPERIPHID0 => UARTPERIPH_ID[0],
            Register::UARTPERIPHID1 => UARTPERIPH_ID[1],
            Register::UARTPERIPHID2 => UARTPERIPH_ID[2],
            Register::UARTPERIPHID3 => UARTPERIPH_ID[3],
            Register::UARTPCELLID0 => UARTPCELL_ID[0],
            Register::UARTPCELLID1 => UARTPCELL_ID[1],
            Register::UARTPCELLID2 => UARTPCELL_ID[2],
            Register::UARTPCELLID3 => UARTPCELL_ID[3],
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        // The SBSA spec only requires the device to support 8-bit reads on some
        // registers and leaves it implementation defined on others. Allow 8-bit
        // reads on all registers for simplicity.
        data[0] = val.to_le_bytes()[0];
        if data.len() > 1 {
            data[1] = val.to_le_bytes()[1];
        }

        self.sync();
        IoResult::Ok
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let register = match self.register(addr) {
            Err(e) => return IoResult::Err(e),
            Ok(r) => r,
        };

        tracing::trace!(?register, ?data, "serial write");

        // Registers that allow 8-bit access.
        match register {
            Register::UARTDR => self.state.write_dr(&mut self.stats, data[0]),
            Register::UARTECR => {}
            Register::UARTILPR => self.state.ilpr = data[0],
            Register::UARTFBRD => self.state.write_fbrd(data[0]),
            Register::UARTLCR_H => self.state.write_lcrh(&mut self.stats, data[0]),
            _ => {
                // 16-bit registers.
                let Some(data) = data.get(..2) else {
                    return IoResult::Err(IoError::InvalidAccessSize);
                };
                let data16 = u16::from_le_bytes(data.try_into().unwrap());
                match register {
                    Register::UARTIBRD => self.state.new_ibrd = data16,
                    Register::UARTCR => self.state.write_cr(data16),
                    Register::UARTIFLS => self.state.write_ifls(data16),
                    Register::UARTIMSC => self.state.write_imsc(data16),
                    Register::UARTICR => self.state.write_icr(data16),
                    Register::UARTDMACR => self.state.write_dmacr(data16),
                    _ => return IoResult::Err(IoError::InvalidRegister),
                };
            }
        }
        self.sync();
        IoResult::Ok
    }
}

impl ChangeDeviceState for SerialPl011 {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state = State::new();
        self.state.connect();
        self.sync();
    }
}

impl ChipsetDevice for SerialPl011 {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for SerialPl011 {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        let _ = self.poll_tx(cx);
        let _ = self.poll_rx(cx);
        self.sync();
    }
}

impl State {
    fn new() -> Self {
        // The initial state for this UART does not completely match the PL011
        // specification. This is because Linux loads its SBSA-compatible UART
        // driver instead of its PL011 driver, and the SBSA-compatible driver
        // expects the firmware to initialize the UART.
        //
        // We could look at enumerating this as a true PL011 instead, but
        // 1. It's unclear how to do this with ACPI (it's trivial with
        //    DeviceTree).
        // 2. There may be a compatibility concern with changing the
        //    enumeration.
        // 3. This is not really a full PL011 emulator anyway, since it does not
        //    support DMA.
        //
        // Instead, initialize the state as defined in the SBSA. Normally
        // firmware would do this, but we do it here.

        let cr = ControlRegister::new()
            .with_enabled(true)
            .with_rxe(true)
            .with_txe(true);

        let lcr = LineControlRegister::new().with_enable_fifos(true);

        Self {
            tx_buffer: VecDeque::new(),
            rx_buffer: VecDeque::new(),
            rx_overrun: false,
            connected: false,
            ilpr: 0,
            ibrd: 0,
            fbrd: FractionalBaudRateRegister::new(),
            lcr,
            cr,
            ifls: InterruptFifoLevelSelectRegister::new()
                .with_txiflsel(FifoLevelSelect::BYTES_16.0)
                .with_rxiflsel(FifoLevelSelect::BYTES_16.0),
            imsc: InterruptRegister::new(),
            ris: InterruptRegister::new(),
            dmacr: DmaControlRegister::new(),
            new_ibrd: 0,
            new_fbrd: FractionalBaudRateRegister::new(),
        }
    }

    /// Updates CR when the modem connects.
    fn connect(&mut self) {
        if !self.connected {
            self.connected = true;
            // CTS/DCD/DSR changed.
            self.ris.set_cts(true);
            self.ris.set_dcd(true);
            self.ris.set_dsr(true);
        }
    }

    /// Updates CR when the modem disconnects.
    fn disconnect(&mut self) {
        if self.connected {
            self.connected = false;
            // CTS/DCD/DSR changed.
            self.ris.set_cts(true);
            self.ris.set_dcd(true);
            self.ris.set_dsr(true);
        }
    }

    fn tx_fifo_trigger(&self) -> usize {
        if self.lcr.enable_fifos() {
            match FifoLevelSelect(self.ifls.txiflsel()) {
                FifoLevelSelect::BYTES_4 => 4,   // <= 1/8 full
                FifoLevelSelect::BYTES_8 => 8,   // <= 1/4 full
                FifoLevelSelect::BYTES_16 => 16, // <= 1/2 full
                FifoLevelSelect::BYTES_24 => 24, // <= 3/4 full
                FifoLevelSelect::BYTES_28 => 28, // <= 7/8 full
                _ => 16,                         // reserved
            }
        } else {
            0
        }
    }

    fn rx_fifo_trigger(&self) -> usize {
        if self.lcr.enable_fifos() {
            match FifoLevelSelect(self.ifls.rxiflsel()) {
                FifoLevelSelect::BYTES_4 => 4,   // <= 1/8 full
                FifoLevelSelect::BYTES_8 => 8,   // <= 1/4 full
                FifoLevelSelect::BYTES_16 => 16, // <= 1/2 full
                FifoLevelSelect::BYTES_24 => 24, // <= 3/4 full
                FifoLevelSelect::BYTES_28 => 28, // <= 7/8 full
                _ => 16,                         // reserved
            }
        } else {
            1
        }
    }

    fn fifo_size(&self) -> usize {
        if self.lcr.enable_fifos() {
            FIFO_SIZE
        } else {
            1
        }
    }

    /// Returns whether it is time to poll the backend device for more data.
    fn should_poll_rx(&self, wait_for_rts: bool) -> bool {
        // Only poll if not in loopback mode, since data comes from THR in that case.
        if self.cr.loopback() {
            return false;
        }

        // Only poll if the backend is connected.
        if !self.connected {
            return false;
        }

        // If requested, only poll if the OS is requesting data. Essentially
        // this means the backend device implements hardware flow control.
        //
        // Without this, any data buffered into the serial port will be lost
        // during boot when the FIFO is cleared.
        if wait_for_rts && (!self.cr.dtr() || !self.cr.rts()) {
            return false;
        }

        // Only poll if there is space in the buffer.
        self.rx_buffer.len() < RX_BUFFER_MAX
    }

    fn pending_interrupt(&mut self) -> bool {
        u16::from(self.ris) & u16::from(self.imsc) != 0
    }

    fn read_dr(&mut self) -> u8 {
        if self.rx_buffer.is_empty() {
            return 0;
        }

        let rx = self.rx_buffer.pop_front().unwrap_or(0);
        if self.rx_buffer.len() < self.rx_fifo_trigger() {
            self.ris.set_rx(false);
        }
        rx
    }

    fn write_dr(&mut self, stats: &mut SerialStats, data: u8) {
        if self.cr.loopback() {
            // Loopback mode wires UARTTXD to UARTRXD, so just add a byte
            // to the fifo along with updating tx state.
            if self.cr.enabled() && self.cr.txe() {
                if self.cr.rxe() {
                    if self.rx_buffer.len() >= TX_BUFFER_MAX {
                        stats
                            .rx_dropped
                            .add((self.rx_buffer.len() - TX_BUFFER_MAX) as u64);
                        self.rx_buffer.truncate(TX_BUFFER_MAX);
                        self.rx_overrun = true;
                        self.ris.set_oe(true);
                    }

                    self.rx_buffer.push_back(data);
                    if self.rx_buffer.len() == self.rx_fifo_trigger() {
                        self.ris.set_rx(true);
                    }
                }
            }
        } else {
            if self.tx_buffer.len() >= TX_BUFFER_MAX {
                // The FIFO is full. Real hardware drops the newest byte in the
                // FIFO, not the oldest one.
                tracing::debug!("tx fifo overrun, dropping output data");
                stats
                    .tx_dropped
                    .add((self.tx_buffer.len() - (TX_BUFFER_MAX - 1)) as u64);
                self.tx_buffer.truncate(TX_BUFFER_MAX - 1);
            }
            self.tx_buffer.push_back(data);

            if self.tx_buffer.len() > self.tx_fifo_trigger() {
                self.ris.set_tx(false);
            }
        }
    }

    fn write_fbrd(&mut self, data: u8) {
        self.new_fbrd = FractionalBaudRateRegister::from(data).clear_reserved();
    }

    fn write_lcrh(&mut self, stats: &mut SerialStats, data: u8) {
        // This register should not be written to when the UART is enabled.
        if self.cr.enabled() {
            return;
        }

        if self.new_ibrd != self.ibrd || u8::from(self.new_fbrd) != u8::from(self.fbrd) {
            self.ibrd = self.new_ibrd;
            self.fbrd = self.new_fbrd;
        }

        let lcr = LineControlRegister::from(data);
        if self.lcr.enable_fifos() && !lcr.enable_fifos() {
            // Fifo went from enabled -> disabled, clear all fifos and update status regs.
            // Additionally, since this can only happen when the UART is disabled, there's no need to update interrupts.
            stats.rx_dropped.add(self.rx_buffer.len() as u64);
            self.rx_buffer.clear();

            stats.tx_dropped.add(self.tx_buffer.len() as u64);
            self.tx_buffer.clear();
        }

        self.lcr = lcr;
    }

    fn write_cr(&mut self, data: u16) {
        self.cr = ControlRegister::from(data).clear_reserved();
    }

    fn write_ifls(&mut self, data: u16) {
        self.ifls = InterruptFifoLevelSelectRegister::from(data).clear_reserved();
    }

    fn write_imsc(&mut self, data: u16) {
        self.imsc = InterruptRegister::from(data).clear_reserved();
    }

    fn write_icr(&mut self, data: u16) {
        self.ris = InterruptRegister::from(u16::from(self.ris) & !data);
    }

    fn write_dmacr(&mut self, data: u16) {
        self.dmacr = DmaControlRegister::from(data).clear_reserved()
    }

    fn read_fr(&self) -> u16 {
        let fifo_size = self.fifo_size();
        let fr = spec::FlagRegister::new()
            .with_cts(self.connected)
            .with_dcd(self.connected)
            .with_dsr(self.connected)
            .with_busy(!self.tx_buffer.is_empty())
            .with_rxfe(self.rx_buffer.is_empty())
            .with_txff(self.tx_buffer.len() >= fifo_size)
            .with_rxff(self.rx_buffer.len() >= fifo_size)
            .with_txfe(self.tx_buffer.is_empty());

        fr.into()
    }
}

impl MmioIntercept for SerialPl011 {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.read(addr, data)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.write(addr, data)
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio_region)
    }
}

mod save_restore {
    use crate::SerialPl011;
    use crate::State;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "serial.PL011")]
        pub struct SavedState {
            #[mesh(1)]
            pub(super) tx_buffer: Vec<u8>,
            #[mesh(2)]
            pub(super) rx_buffer: Vec<u8>,
            #[mesh(3)]
            pub(super) rx_overrun: bool,
            #[mesh(4)]
            pub(super) connected: bool,
            #[mesh(5)]
            pub(super) ilpr: u8,
            #[mesh(6)]
            pub(super) ibrd: u16,
            #[mesh(7)]
            pub(super) fbrd: u8,
            #[mesh(8)]
            pub(super) lcr: u8,
            #[mesh(9)]
            pub(super) cr: u16,
            #[mesh(10)]
            pub(super) ifls: u16,
            #[mesh(11)]
            pub(super) imsc: u16,
            #[mesh(12)]
            pub(super) ris: u16,
            #[mesh(13)]
            pub(super) dmacr: u16,
            #[mesh(14)]
            pub(super) new_ibrd: u16,
            #[mesh(15)]
            pub(super) new_fbrd: u8,
        }
    }

    impl SaveRestore for SerialPl011 {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let State {
                ref tx_buffer,
                ref rx_buffer,
                rx_overrun,
                connected,
                ilpr,
                ibrd,
                fbrd,
                lcr,
                cr,
                ifls,
                imsc,
                ris,
                dmacr,
                new_ibrd,
                new_fbrd,
            } = self.state;
            Ok(state::SavedState {
                tx_buffer: tx_buffer.clone().into(),
                rx_buffer: rx_buffer.clone().into(),
                rx_overrun,
                connected,
                ilpr,
                ibrd,
                fbrd: fbrd.into(),
                lcr: lcr.into(),
                cr: cr.into(),
                ifls: ifls.into(),
                imsc: imsc.into(),
                ris: ris.into(),
                dmacr: dmacr.into(),
                new_ibrd,
                new_fbrd: new_fbrd.into(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                tx_buffer,
                rx_buffer,
                rx_overrun,
                connected,
                ilpr,
                ibrd,
                fbrd,
                lcr,
                cr,
                ifls,
                imsc,
                ris,
                dmacr,
                new_ibrd,
                new_fbrd,
            } = state;
            self.state = State {
                tx_buffer: tx_buffer.into(),
                rx_buffer: rx_buffer.into(),
                rx_overrun,
                connected,
                ilpr,
                ibrd,
                fbrd: fbrd.into(),
                lcr: lcr.into(),
                cr: cr.into(),
                ifls: ifls.into(),
                imsc: imsc.into(),
                ris: ris.into(),
                dmacr: dmacr.into(),
                new_ibrd,
                new_fbrd: new_fbrd.into(),
            };
            if self.io.is_connected() {
                self.state.connect();
            } else {
                self.state.disconnect();
            }
            self.sync();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::io::IoError;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use inspect::InspectMut;
    use pal_async::async_test;
    use serial_core::SerialIo;
    use std::future::poll_fn;
    use std::io;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use vmcore::line_interrupt::LineInterrupt;

    const PL011_SERIAL0_BASE: u64 = 0xEFFEC000;

    const UARTCR_TXE: u16 = 0x0100;
    const UARTCR_RXE: u16 = 0x0200;
    const UARTCR_UARTEN: u16 = 0x0001;

    const UARTLCR_H_FIFO_ENABLE: u16 = 0x0010;
    const UARTLCR_H_8BITS: u16 = 0x0060;

    // This is a "loopback" kind of io, where a write to the serial port will appear in the read queue
    #[derive(InspectMut)]
    pub struct SerialIoMock {
        data: Vec<u8>,
    }

    impl SerialIo for SerialIoMock {
        fn is_connected(&self) -> bool {
            true
        }

        fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for SerialIoMock {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            if self.data.is_empty() {
                return Poll::Ready(Err(ErrorKind::ConnectionAborted.into()));
            }
            let n = buf.len().min(self.data.len());
            for (s, d) in self.data.drain(..n).zip(buf) {
                *d = s;
            }
            Poll::Ready(Ok(n))
        }
    }

    impl AsyncWrite for SerialIoMock {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let buf = &buf[..buf.len().min(FIFO_SIZE)];
            self.data.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl SerialIoMock {
        pub fn new() -> Self {
            Self { data: Vec::new() }
        }
    }

    #[test]
    fn test_read() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        let mut data = vec![0; 1];
        serial.mmio_read(0, &mut data).unwrap();

        let mut data = vec![0; 2];
        serial.mmio_read(0, &mut data).unwrap();

        let mut data = vec![0; 4];
        serial.mmio_read(0, &mut data).unwrap();

        assert!(matches!(
            serial.mmio_read(1, &mut data),
            IoResult::Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            serial.mmio_read(2, &mut data),
            IoResult::Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            serial.mmio_read(3, &mut data),
            IoResult::Err(IoError::UnalignedAccess)
        ));

        serial
            .mmio_read(Register::UARTDR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTRSR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTECR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTFR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTILPR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTIBRD.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTFBRD.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTLCR_H.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTCR.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTIFLS.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTIMSC.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTRIS.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTMIS.0 as u64, &mut data)
            .unwrap();
        assert!(matches!(
            serial.mmio_read(Register::UARTICR.0 as u64, &mut data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        serial
            .mmio_read(Register::UARTDMACR.0 as u64, &mut data)
            .unwrap();

        serial
            .mmio_read(Register::UARTPERIPHID0.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPERIPHID1.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPERIPHID2.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPERIPHID3.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPCELLID0.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPCELLID1.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPCELLID2.0 as u64, &mut data)
            .unwrap();
        serial
            .mmio_read(Register::UARTPCELLID3.0 as u64, &mut data)
            .unwrap();
    }

    #[test]
    fn test_write() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        let data = vec![0; 1];
        assert!(matches!(
            serial.mmio_write(Register::UARTIBRD.0.into(), &data),
            IoResult::Err(IoError::InvalidAccessSize)
        ));

        let data = vec![0; 2];
        serial.mmio_write(0, &data).unwrap();

        let data = vec![0; 3];
        serial.mmio_write(0, &data).unwrap();

        let data = vec![0; 4];
        serial.mmio_write(0, &data).unwrap();

        let data = vec![0; 5];
        serial.mmio_write(0, &data).unwrap();

        assert!(matches!(
            serial.mmio_write(1, &data),
            IoResult::Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            serial.mmio_write(2, &data),
            IoResult::Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            serial.mmio_write(3, &data),
            IoResult::Err(IoError::UnalignedAccess)
        ));

        serial.mmio_write(Register::UARTDR.0 as u64, &data).unwrap();
        serial
            .mmio_write(Register::UARTRSR.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTECR.0 as u64, &data)
            .unwrap();
        assert!(matches!(
            serial.mmio_write(Register::UARTFR.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        serial
            .mmio_write(Register::UARTILPR.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTIBRD.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTFBRD.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTLCR_H.0 as u64, &data)
            .unwrap();
        serial.mmio_write(Register::UARTCR.0 as u64, &data).unwrap();
        serial
            .mmio_write(Register::UARTIFLS.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTIMSC.0 as u64, &data)
            .unwrap();
        assert!(matches!(
            serial.mmio_write(Register::UARTRIS.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTMIS.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        serial
            .mmio_write(Register::UARTICR.0 as u64, &data)
            .unwrap();
        serial
            .mmio_write(Register::UARTDMACR.0 as u64, &data)
            .unwrap();

        assert!(matches!(
            serial.mmio_write(Register::UARTPERIPHID0.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPERIPHID1.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPERIPHID2.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPERIPHID3.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPCELLID0.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPCELLID1.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPCELLID2.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
        assert!(matches!(
            serial.mmio_write(Register::UARTPCELLID3.0 as u64, &data),
            IoResult::Err(IoError::InvalidRegister)
        ));
    }

    fn read(serial: &mut SerialPl011, r: Register) -> u16 {
        let mut data = vec![0; 2];
        serial.mmio_read(r.0 as u64, &mut data).unwrap();
        u16::from_ne_bytes(data[..2].try_into().unwrap())
    }

    fn write(serial: &mut SerialPl011, r: Register, val: u16) {
        let mut data = vec![0; 2];
        data[..2].copy_from_slice(&val.to_ne_bytes());
        serial.mmio_write(r.0 as u64, &data).unwrap();
    }

    #[test]
    fn test_init() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        assert_eq!(read(&mut serial, Register::UARTPERIPHID0), UARTPERIPH_ID[0]);
        assert_eq!(read(&mut serial, Register::UARTPERIPHID1), UARTPERIPH_ID[1]);
        assert_eq!(read(&mut serial, Register::UARTPERIPHID2), UARTPERIPH_ID[2]);
        assert_eq!(read(&mut serial, Register::UARTPERIPHID3), UARTPERIPH_ID[3]);
        assert_eq!(read(&mut serial, Register::UARTPCELLID0), UARTPCELL_ID[0]);
        assert_eq!(read(&mut serial, Register::UARTPCELLID1), UARTPCELL_ID[1]);
        assert_eq!(read(&mut serial, Register::UARTPCELLID2), UARTPCELL_ID[2]);
        assert_eq!(read(&mut serial, Register::UARTPCELLID3), UARTPCELL_ID[3]);

        // Mask interrupts
        write(&mut serial, Register::UARTIMSC, 0);
        // Disable interrupts (lower 11 bits)
        write(&mut serial, Register::UARTICR, 0x7ff);
        // Disable DMA on Rx and Tx
        write(&mut serial, Register::UARTDMACR, 0x0);

        // Leave Rx and Tx enabled to drain FIFOs, wait a bit,
        // and then disable Rx, Tx, and UART.
        write(&mut serial, Register::UARTCR, UARTCR_RXE | UARTCR_TXE);
        read(&mut serial, Register::UARTCR);
        read(&mut serial, Register::UARTCR);
        write(&mut serial, Register::UARTCR, 0x0000);

        // Set integer and fractinal parts of the baud rate,
        // hardcoded for now
        write(&mut serial, Register::UARTFBRD, 0x0004);
        write(&mut serial, Register::UARTIBRD, 0x0027);
        // The UARTLCR_H, UARTIBRD, and UARTFBRD registers form the single 30-bit
        // wide UARTLCR Register that is updated on a single write strobe generated by a
        // UARTLCR_H write
        write(
            &mut serial,
            Register::UARTLCR_H,
            UARTLCR_H_FIFO_ENABLE | UARTLCR_H_8BITS,
        );

        // Enable Tx and Rx, wait a bit, and then enable UART
        write(&mut serial, Register::UARTCR, UARTCR_RXE | UARTCR_TXE);
        read(&mut serial, Register::UARTCR);
        read(&mut serial, Register::UARTCR);
        write(
            &mut serial,
            Register::UARTCR,
            UARTCR_RXE | UARTCR_TXE | UARTCR_UARTEN,
        );
    }

    #[async_test]
    async fn test_writeread_data() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        write(&mut serial, Register::UARTCR, 0x400 | 0x800); // UARTCR_DTR | UARTCR_RTS

        for n in 1..FIFO_SIZE as u16 {
            write(&mut serial, Register::UARTDR, n);
        }

        poll_fn(|cx| {
            serial.poll_device(cx);
            std::task::Poll::Ready(())
        })
        .await;

        for n in FIFO_SIZE as u16..1 {
            assert_eq!(read(&mut serial, Register::UARTDR), n);
        }
    }

    #[test]
    fn test_write_ifls() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        write(&mut serial, Register::UARTIFLS, 0b000000);
        assert_eq!(u16::from(serial.state.ifls), 0b000000);

        write(&mut serial, Register::UARTIFLS, 0b001001);
        assert_eq!(u16::from(serial.state.ifls), 0b001001);

        write(&mut serial, Register::UARTIFLS, 0b100100);
        assert_eq!(u16::from(serial.state.ifls), 0b100100);

        write(&mut serial, Register::UARTIFLS, 0b11001001);
        assert_eq!(u16::from(serial.state.ifls), 0b001001); // Drop extra bits
    }

    #[test]
    fn test_write_icr() {
        let serial_io = SerialIoMock::new();
        let mut serial = SerialPl011::new(
            "com1".to_string(),
            PL011_SERIAL0_BASE,
            LineInterrupt::detached(),
            Box::new(serial_io),
        )
        .unwrap();

        serial.state.ris = InterruptRegister::from(0b11111111111).clear_reserved();
        write(&mut serial, Register::UARTICR, 0b00000000000);
        assert_eq!(u16::from(serial.state.ris), 0b11110111111);

        serial.state.ris = InterruptRegister::from(0b11111111111).clear_reserved();
        write(&mut serial, Register::UARTICR, 0b100000000000); // extra bit
        assert_eq!(u16::from(serial.state.ris), 0b11110111111);

        serial.state.ris = InterruptRegister::from(0b11111111111).clear_reserved();
        write(&mut serial, Register::UARTICR, 0b11111111111);
        assert_eq!(u16::from(serial.state.ris), 0b00000000000);

        serial.state.ris = InterruptRegister::from(0b11111111111).clear_reserved();
        write(&mut serial, Register::UARTICR, 0b111111111111); // extra bit
        assert_eq!(u16::from(serial.state.ris), 0b00000000000);

        serial.state.ris = InterruptRegister::from(0b11111111111).clear_reserved();
        write(&mut serial, Register::UARTICR, 0b01111011110);
        assert_eq!(u16::from(serial.state.ris), 0b10000100001);
    }
}
