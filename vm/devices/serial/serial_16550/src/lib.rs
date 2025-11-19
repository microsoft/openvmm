// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Emulator for 16550 serial UART.

#![forbid(unsafe_code)]

pub mod resolver;
mod spec;

use self::spec::FIFO_SIZE;
use self::spec::FifoControlRegister;
use self::spec::FifoState;
use self::spec::InterruptEnableRegister;
use self::spec::InterruptIdentificationRegister;
use self::spec::InterruptSource;
use self::spec::LineControlRegister;
use self::spec::LineStatusRegister;
use self::spec::ModemControlRegister;
use self::spec::ModemStatusRegister;
use self::spec::Register;
use self::spec::RxFifoInterruptTrigger;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use serial_16550_resources::MmioOrIoPort;
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

/// A 16550 serial port emulator.
#[derive(InspectMut)]
pub struct Serial16550 {
    // Fixed configuration
    #[inspect(skip)]
    debug_name: String,
    #[inspect(skip)]
    io_region: Option<(&'static str, RangeInclusive<u16>)>,
    #[inspect(skip)]
    mmio_region: Option<(&'static str, RangeInclusive<u64>)>,
    register_width: u8,
    register_shift: u8,
    wait_for_rts: bool,

    // Runtime glue
    interrupt: LineInterrupt,
    #[inspect(mut)]
    io: Box<dyn SerialIo>,

    // Volatile state
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
    ier: InterruptEnableRegister,
    lcr: LineControlRegister,
    mcr: ModemControlRegister,
    msr: ModemStatusRegister,
    fcr: FifoControlRegister,
    #[inspect(hex)]
    dll: u8,
    #[inspect(hex)]
    dlm: u8,
    #[inspect(hex)]
    scratch: u8,
    thr_empty_acknowledged: bool,
    rx_overrun: bool,
    #[inspect(with = "VecDeque::len")]
    tx_buffer: VecDeque<u8>,
    #[inspect(with = "VecDeque::len")]
    rx_buffer: VecDeque<u8>,
}

// A normal FIFO has only 16 bytes, but we get greater batching with these values.
const TX_BUFFER_MAX: usize = 256;
const RX_BUFFER_MAX: usize = 256;

/// An error returned by [`Serial16550::new`].
#[derive(Debug, Error)]
pub enum ConfigurationError {
    /// The provided base address was not aligned to the register bank width.
    #[error("unaligned base address: {0}")]
    UnalignedBaseAddress(u64),
    /// The specified register with was invalid.
    #[error("invalid register width: {0}")]
    InvalidRegisterWidth(u8),
}

impl Serial16550 {
    /// Returns a new emulator instance.
    ///
    /// `debug_name` is used to improve tracing statements. `base` is the base
    /// IO port and will be used for an IO region spanning 8 bytes.
    pub fn new(
        debug_name: String,
        base: MmioOrIoPort,
        register_width: u8,
        interrupt: LineInterrupt,
        io: Box<dyn SerialIo>,
        wait_for_rts: bool,
    ) -> Result<Self, ConfigurationError> {
        let width = 8 * register_width as u64;
        let (base_addr, io_region, mmio_region) = match base {
            MmioOrIoPort::Mmio(base) => {
                if ![1, 2, 4, 8].contains(&register_width) {
                    return Err(ConfigurationError::InvalidRegisterWidth(register_width));
                }
                (base, None, Some(("registers", base..=base + (width - 1))))
            }
            MmioOrIoPort::IoPort(base) => {
                if register_width != 1 {
                    return Err(ConfigurationError::InvalidRegisterWidth(register_width));
                }
                (
                    base.into(),
                    Some(("registers", base..=base + (width - 1) as u16)),
                    None,
                )
            }
        };

        if base_addr & (width - 1) != 0 {
            return Err(ConfigurationError::UnalignedBaseAddress(base_addr));
        }

        let mut this = Self {
            debug_name,
            io_region,
            mmio_region,
            register_width,
            register_shift: register_width.trailing_zeros() as u8,
            wait_for_rts,
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

        // Reduce wakeups by waking to poll if the rx buffer is at least half
        // empty.
        if self.state.should_poll_rx(self.wait_for_rts)
            && self.state.rx_buffer.len() <= RX_BUFFER_MAX / 2
        {
            if let Some(waker) = self.rx_waker.take() {
                waker.wake();
            }
        }

        // On PCs, OUT2 is ANDed with the interrupt line.
        self.interrupt
            .set_level(self.state.pending_interrupt().is_some() && self.state.mcr.out2());
    }

    fn poll_tx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while !self.state.tx_buffer.is_empty() {
            if !self.state.msr.dcd() {
                // The backend is disconnected, so drop everything in the FIFO.
                self.stats.tx_dropped.add(self.state.tx_buffer.len() as u64);
                self.state.tx_buffer.clear();
                break;
            }
            let (buf, _) = self.state.tx_buffer.as_slices();
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
        }
        // Wait for more bytes to write.
        self.tx_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_rx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut buf = [0; RX_BUFFER_MAX];
        loop {
            if !self.state.msr.dcd() {
                // Wait for reconnect.
                if let Err(err) = ready!(self.io.poll_connect(cx)) {
                    tracing::info!(
                        port = self.debug_name,
                        error = &err as &dyn std::error::Error,
                        "serial backend failure"
                    );
                    break Poll::Ready(());
                }
                tracing::info!(port = self.debug_name, "serial connected");
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
                tracing::info!(port = self.debug_name, "serial disconnected");
                self.state.disconnect();
                continue;
            }
            let avail_space = RX_BUFFER_MAX - self.state.rx_buffer.len();
            let buf = &mut buf[..avail_space];
            match ready!(Pin::new(&mut self.io).poll_read(cx, buf)) {
                Ok(0) => {
                    tracing::info!(port = self.debug_name, "serial disconnected");
                    self.state.disconnect();
                }
                Ok(n) => {
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

    fn register(&self, addr: u64) -> Option<Register> {
        if addr as u8 & (self.register_width - 1) != 0 {
            return None;
        }
        Some(Register(
            ((addr & ((8 << self.register_shift) - 1)) >> self.register_shift) as u8,
        ))
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let Some(register) = self.register(addr) else {
            return IoResult::Err(IoError::UnalignedAccess);
        };
        let dlab = self.state.lcr.dlab();
        data.fill(0);
        data[0] = match register {
            Register::RHR if !dlab => self.state.read_rhr(),
            Register::DLL if dlab => self.state.dll,
            Register::IER if !dlab => self.state.ier.into(),
            Register::DLM if dlab => self.state.dlm,
            Register::ISR => self.state.read_isr(),
            Register::LCR => self.state.lcr.into(),
            Register::MCR => self.state.mcr.into(),
            Register::LSR => self.state.read_lsr(),
            Register::MSR => self.state.read_msr(),
            Register::SPR => self.state.scratch,
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        self.sync();
        IoResult::Ok
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let Some(register) = self.register(addr) else {
            return IoResult::Err(IoError::UnalignedAccess);
        };
        let data = data[0];
        let dlab = self.state.lcr.dlab();
        match register {
            Register::THR if !dlab => self.state.write_thr(&mut self.stats, data),
            Register::DLL if dlab => self.state.dll = data,
            Register::IER if !dlab => self.state.write_ier(data),
            Register::DLM if dlab => self.state.dlm = data,
            Register::FCR => self.state.write_fcr(&mut self.stats, data),
            Register::LCR => self.state.lcr = data.into(),
            Register::MCR => self.state.write_mcr(data),
            Register::RST => self.state.write_reset(),
            Register::MSR => {}
            Register::SPR => self.state.scratch = data,
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        self.sync();
        IoResult::Ok
    }
}

impl ChangeDeviceState for Serial16550 {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state = State::new();
        self.state.connect();
        self.sync();
    }
}

impl ChipsetDevice for Serial16550 {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        self.io_region.is_some().then_some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.mmio_region.is_some().then_some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for Serial16550 {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        let _ = self.poll_tx(cx);
        let _ = self.poll_rx(cx);
        self.sync();
    }
}

impl State {
    fn new() -> Self {
        Self {
            ier: InterruptEnableRegister::new(),
            lcr: LineControlRegister::new(),
            mcr: ModemControlRegister::new(),
            msr: ModemStatusRegister::new(),
            dll: 0,
            dlm: 0,
            scratch: 0xff,
            thr_empty_acknowledged: false,
            rx_overrun: false,
            tx_buffer: VecDeque::new(),
            rx_buffer: VecDeque::new(),
            fcr: FifoControlRegister::new(),
        }
    }

    /// Updates MSR when the modem connects.
    fn connect(&mut self) {
        self.update_msr(|this| {
            this.msr.set_dcd(true);
            this.msr.set_cts(true);
            this.msr.set_dsr(true);
            this.msr.set_ri(false);
        });
    }

    /// Updates MSR when the modem disconnects.
    fn disconnect(&mut self) {
        self.update_msr(|this| {
            this.msr.set_dcd(false);
            this.msr.set_cts(false);
            this.msr.set_dsr(false);
            this.msr.set_ri(false);
        })
    }

    fn rx_fifo_trigger(&self) -> usize {
        if self.fcr.enable_fifos() {
            match RxFifoInterruptTrigger(self.fcr.rx_fifo_int_trigger()) {
                RxFifoInterruptTrigger::BYTES_1 => 1,
                RxFifoInterruptTrigger::BYTES_4 => 4,
                RxFifoInterruptTrigger::BYTES_8 => 8,
                RxFifoInterruptTrigger::BYTES_14 => 14,
                _ => unreachable!(),
            }
        } else {
            1
        }
    }

    fn fifo_size(&self) -> usize {
        if self.fcr.enable_fifos() {
            FIFO_SIZE
        } else {
            1
        }
    }

    fn is_thr_empty(&self) -> bool {
        // THR is empty when our buffer is empty. Note that this can cause a
        // guest to stall if the backend is not emptying its buffers fast enough
        // (or even stalls indefinitely). This isn't usually a problem, e.g. for
        // logging or interactive use, but it might cause problems with some
        // serial protocols or operating systems that expect the FIFO to drain
        // at the configured baud rate.
        //
        // A key advantage of this approach is that it provides backpressure to
        // the guest, ensuring that the guest won't send faster than the backend
        // can receive. It also provides a way for a guest to ensure that all
        // data is flushed before halting the VM.
        //
        // There are other approaches, each with different downsides:
        //
        // 1. Always report THR empty. This is simple and eliminates any
        //    stalling, but it also eliminates the backpressure and flush
        //    advantages.
        //
        // 2. Report THR empty after some timeout, but don't drop data unless
        //    the guest fills up the FIFO. This probabilistically provides some
        //    degree of backpressure and flushing without stalling the guest
        //    indefinitely.
        //
        // 3. Do either of these, but also add support for auto-flow control, so
        //    that the guest can opt into only draining the (visible) FIFO when
        //    the receiver is ready. This is perfect--it provides backpressure
        //    and flushing, and the guest knows and understands it is happening
        //    so it won't misbehave--but it requires guest opt in, and Linux, at
        //    least, will only opt in if the tty is configured to use the
        //    feature (hardware flow control). Typical configurations will not
        //    do this.
        //
        // For now, follow the behavior of Hyper-V and just stall the guest.
        self.tx_buffer.is_empty()
    }

    /// Returns whether it is time to poll the backend device for more data.
    fn should_poll_rx(&self, wait_for_rts: bool) -> bool {
        // Only poll if not in loopback mode, since data comes from THR in that case.
        if self.mcr.loopback() {
            return false;
        }
        // Only poll if the backend is connected.
        if !self.msr.dcd() {
            return false;
        }
        // If requested, only poll if the OS is requesting data. Essentially
        // this means the backend device implements hardware flow control.
        //
        // Without this, any data buffered into the serial port will be lost
        // during boot when the FIFO is cleared.
        if wait_for_rts && (!self.mcr.dtr() || !self.mcr.rts()) {
            return false;
        }
        // Only poll if there is space in the buffer.
        self.rx_buffer.len() < RX_BUFFER_MAX
    }

    fn pending_interrupt(&self) -> Option<InterruptSource> {
        // Check each condition in priority order.

        if self.ier.receiver_line_status() && self.rx_overrun {
            return Some(InterruptSource::RECEIVER_LINE_STATUS);
        }

        // RHR interrupt.
        if self.ier.received_data_avail() && !self.rx_buffer.is_empty() {
            if self.rx_buffer.len() >= self.rx_fifo_trigger() {
                return Some(InterruptSource::RECEIVED_DATA_AVAIL);
            } else {
                // Real hardware would ensure that no character had arrived for
                // at least 4 characters worth of time before signaling this.
                // But that's less than 1ms, so just signal it immediately.
                return Some(InterruptSource::RECEIVE_TIMEOUT);
            }
        }

        // THR interrupt.
        if self.ier.thr_empty() && self.is_thr_empty() && !self.thr_empty_acknowledged {
            return Some(InterruptSource::THR_EMPTY);
        }

        // MSR interrupt.
        if self.ier.modem_status()
            && (self.msr.cts_change()
                || self.msr.dcd_change()
                || self.msr.ri_went_low()
                || self.msr.dcd_change())
        {
            return Some(InterruptSource::MODEM_STATUS);
        }

        None
    }

    fn read_msr(&mut self) -> u8 {
        let msr = self.effective_msr();
        self.msr.set_cts_change(false);
        self.msr.set_dsr_change(false);
        self.msr.set_ri_went_low(false);
        self.msr.set_dcd_change(false);
        msr.into()
    }

    // The effective value of the MSR.
    fn effective_msr(&self) -> ModemStatusRegister {
        if self.mcr.loopback() {
            // When in loopback mode, modem status is reflected from the modem
            // control register.
            self.msr
                .with_cts(self.mcr.dtr())
                .with_dsr(self.mcr.rts())
                .with_ri(self.mcr.out1())
                .with_dcd(self.mcr.out2())
        } else {
            self.msr
        }
    }

    /// Call `f`, which updates the effective MSR, then update MSR change bits.
    fn update_msr(&mut self, f: impl FnOnce(&mut Self)) {
        let old_msr = self.effective_msr();
        f(self);
        self.post_update_msr(old_msr);
    }

    /// Update MSR change bits after an effective MSR change.
    fn post_update_msr(&mut self, old_msr: ModemStatusRegister) {
        let new_msr = self.effective_msr();
        if old_msr.cts() != new_msr.cts() {
            self.msr.set_cts_change(true);
        }
        if old_msr.dsr() != new_msr.dsr() {
            self.msr.set_dsr_change(true);
        }
        if old_msr.ri() && !new_msr.ri() {
            self.msr.set_ri_went_low(true);
        }
        if old_msr.dcd() != new_msr.dcd() {
            self.msr.set_dcd_change(true);
        }
    }

    fn read_rhr(&mut self) -> u8 {
        self.rx_buffer.pop_front().unwrap_or(0)
    }

    fn read_isr(&mut self) -> u8 {
        let interrupt = self.pending_interrupt();
        if interrupt == Some(InterruptSource::THR_EMPTY) {
            self.thr_empty_acknowledged = true;
        }
        let fifo_state = if self.fcr.enable_fifos() {
            FifoState::ENABLED
        } else {
            FifoState::DISABLED
        };
        InterruptIdentificationRegister::new()
            .with_no_interrupt_pending(interrupt.is_none())
            .with_source(interrupt.map_or(0, |x| x.0))
            .with_fifo_state(fifo_state.0)
            .into()
    }

    fn read_lsr(&mut self) -> u8 {
        let lsr = LineStatusRegister::new()
            .with_rx_ready(!self.rx_buffer.is_empty())
            .with_overrun_error(self.rx_overrun)
            .with_parity_error(false)
            .with_framing_error(false)
            .with_break_signal_received(false)
            .with_thr_empty(self.is_thr_empty())
            .with_thr_and_tsr_empty(self.is_thr_empty())
            .with_fifo_data_error(false);
        self.rx_overrun = false;
        lsr.into()
    }

    fn write_thr(&mut self, stats: &mut SerialStats, data: u8) {
        self.thr_empty_acknowledged = false;
        if self.mcr.loopback() {
            // Truncate the FIFO. Since the FIFO size can be computed while in
            // loopback mode, and some guests will behave differently depending
            // on the computed size, emulate the size faithfully.
            let fifo_size = self.fifo_size();
            if self.rx_buffer.len() >= fifo_size {
                stats
                    .rx_dropped
                    .add((self.rx_buffer.len() - fifo_size) as u64);
                self.rx_buffer.truncate(fifo_size);
                self.rx_overrun = true;
            }
            self.rx_buffer.push_back(data);
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
        }
    }

    fn write_mcr(&mut self, data: u8) {
        let mcr = ModemControlRegister::from(data).with_reserved(0);
        tracing::trace!(?mcr, "mcr update");
        // mcr.loopback may have changed, which could cause an MSR update.
        self.update_msr(|this| this.mcr = mcr);
    }

    fn write_ier(&mut self, data: u8) {
        self.ier = InterruptEnableRegister::from(data).with_reserved(0);
        self.thr_empty_acknowledged = false;
    }

    fn write_fcr(&mut self, stats: &mut SerialStats, data: u8) {
        let fcr = FifoControlRegister::from(data).with_reserved(0);
        if fcr.enable_fifos() {
            if fcr.clear_rx_fifo() {
                tracing::trace!("clearing rx fifo");
                stats.rx_dropped.add(self.rx_buffer.len() as u64);
                self.rx_buffer.clear();
            }
            if fcr.clear_tx_fifo() {
                tracing::trace!("clearing tx fifo");
                stats.tx_dropped.add(self.tx_buffer.len() as u64);
                self.tx_buffer.clear();
            }
            self.fcr = fcr.with_clear_rx_fifo(false).with_clear_tx_fifo(false);
        } else {
            self.fcr = FifoControlRegister::new();
        }
    }

    /// Reset state due to write to reset register.
    fn write_reset(&mut self) {
        let Self {
            ier,
            lcr,
            mcr,
            msr,
            fcr,
            dll,
            dlm,
            scratch,
            thr_empty_acknowledged,
            rx_overrun,
            tx_buffer,
            rx_buffer,
        } = Self::new();

        // Reset this state.
        self.ier = ier;
        self.lcr = lcr;
        self.mcr = mcr;
        self.msr = msr;
        self.fcr = fcr;
        self.thr_empty_acknowledged = thr_empty_acknowledged;
        self.rx_overrun = rx_overrun;

        // Preserve this state.
        let _ = dll;
        let _ = dlm;
        let _ = scratch;
        let _ = tx_buffer;
        let _ = rx_buffer;
    }
}

impl PortIoIntercept for Serial16550 {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        self.read(io_port.into(), data)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        self.write(io_port.into(), data)
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        if let Some(region) = &self.io_region {
            std::slice::from_ref(region)
        } else {
            &[]
        }
    }
}

impl MmioIntercept for Serial16550 {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.read(addr, data)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.write(addr, data)
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        if let Some(region) = &self.mmio_region {
            std::slice::from_ref(region)
        } else {
            &[]
        }
    }
}

mod save_restore {
    use crate::Serial16550;
    use crate::State;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "serial.uart16550")]
        pub struct SavedState {
            #[mesh(1)]
            pub(super) ier: u8,
            #[mesh(2)]
            pub(super) lcr: u8,
            #[mesh(3)]
            pub(super) mcr: u8,
            #[mesh(4)]
            pub(super) msr: u8,
            #[mesh(5)]
            pub(super) fcr: u8,
            #[mesh(6)]
            pub(super) dll: u8,
            #[mesh(7)]
            pub(super) dlm: u8,
            #[mesh(8)]
            pub(super) scratch: u8,
            #[mesh(9)]
            pub(super) thr_empty_acknowledged: bool,
            #[mesh(10)]
            pub(super) rx_overrun: bool,
            #[mesh(11)]
            pub(super) tx_buffer: Vec<u8>,
            #[mesh(12)]
            pub(super) rx_buffer: Vec<u8>,
        }
    }

    impl SaveRestore for Serial16550 {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let State {
                ier,
                lcr,
                mcr,
                msr,
                fcr,
                dll,
                dlm,
                scratch,
                thr_empty_acknowledged,
                rx_overrun,
                tx_buffer,
                rx_buffer,
            } = &self.state;
            Ok(state::SavedState {
                ier: (*ier).into(),
                lcr: (*lcr).into(),
                mcr: (*mcr).into(),
                msr: (*msr).into(),
                fcr: (*fcr).into(),
                dll: *dll,
                dlm: *dlm,
                scratch: *scratch,
                thr_empty_acknowledged: *thr_empty_acknowledged,
                rx_overrun: *rx_overrun,
                tx_buffer: tx_buffer.clone().into(),
                rx_buffer: rx_buffer.clone().into(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                ier,
                lcr,
                mcr,
                msr,
                fcr,
                dll,
                dlm,
                scratch,
                thr_empty_acknowledged,
                rx_overrun,
                tx_buffer,
                rx_buffer,
            } = state;
            self.state = State {
                ier: ier.into(),
                lcr: lcr.into(),
                mcr: mcr.into(),
                msr: msr.into(),
                fcr: fcr.into(),
                dll,
                dlm,
                scratch,
                thr_empty_acknowledged,
                rx_overrun,
                tx_buffer: tx_buffer.into(),
                rx_buffer: rx_buffer.into(),
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
