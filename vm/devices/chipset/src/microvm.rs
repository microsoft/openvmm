// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM chipset devices.

pub mod resolver;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_resources::microvm::MicrovmPortbDrain;
use chipset_resources::microvm::MicrovmPortbTimeAbi;
use chipset_resources::microvm_time::RestorePacketBase;
use chipset_resources::microvm_time::RestorePacketV4;
use chipset_resources::microvm_time::RestoreTimeRecord;
use chipset_resources::microvm_time::TimeSample;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use power_resources::PowerRequest;
use power_resources::PowerRequestClient;
use serial_core::SerialIo;
use serial_core::disconnected::Disconnected;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::SystemTime;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateRoot;

const DATA_PORT: u16 = 0xe9;
const STATUS_PORT: u16 = 0xea;
const TIME_WINDOW_PORT: u16 = chipset_resources::microvm_time::TIME_WINDOW_PORT;
const SHUTDOWN_PORT: u16 = 0x604;
const SNAPSHOT_PORT: u16 = 0x605;
const RESTORE_PACKET_SELECT: u8 = 0xa5;
const GENERATION_ID_SELECT: u8 = 0xa6;
const TIME_SAMPLE_SELECT: u8 = chipset_resources::microvm_time::TIME_SAMPLE_SELECT;
const GENERATION_ID_SIZE: usize = 16;
const STATUS_INPUT_AVAILABLE: u8 = 1 << 0;
const STATUS_RESTORE_PACKET_AVAILABLE: u8 = 1 << 1;
const STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE: u8 = 1 << 2;
const STATUS_RESTORE_MEMORY_TARGET_AVAILABLE: u8 = 1 << 3;
const STATUS_RESTORE_MEMORY_EXPANSION_AVAILABLE: u8 = 1 << 4;
const STATUS_GENERATION_ID_AVAILABLE: u8 = 1 << 5;
const STATUS_TIME_SAMPLE_AVAILABLE: u8 =
    chipset_resources::microvm_time::STATUS_TIME_SAMPLE_AVAILABLE;
const BUFFER_MAX: usize = 1024 * 1024;

/// The restore packet version 4 of a restored VM process.
enum RestorePacketState {
    /// No packet, or the packet was consumed.
    None,
    /// Waiting for the worker to seal the time fields.
    Pending(RestorePacketBase, mesh::OneshotReceiver<RestoreTimeRecord>),
    /// Sealed and not yet selected; host UTC is latched at the first
    /// selection.
    Sealed(RestorePacketBase, RestoreTimeRecord),
    /// Encoded into the restore-packet queue.
    Encoded,
}

/// NVX time ABI v1 state of the portb device.
struct PortbTimeAbi {
    generation: u32,
    utc_offset_ns: i128,
    sample_delay: std::time::Duration,
    test_hooks: bool,
    restore: RestorePacketState,
    /// Notified at the guest's first selection of the restore packet.
    packet_selected: Option<mesh::OneshotSender<()>>,
}

impl PortbTimeAbi {
    /// Returns host UTC in nanoseconds, after the test hooks' delay and
    /// offset.
    fn latch_utc_ns(&self) -> u64 {
        if !self.sample_delay.is_zero() {
            std::thread::sleep(self.sample_delay);
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_nanos() as i128);
        (now + self.utc_offset_ns).clamp(0, u64::MAX.into()) as u64
    }

    /// Returns whether a sealed packet waits for its first selection,
    /// receiving the sealed time fields if they arrived.
    fn poll_sealed(&mut self) -> bool {
        self.restore = match std::mem::replace(&mut self.restore, RestorePacketState::None) {
            RestorePacketState::Pending(base, mut time) => {
                match Pin::new(&mut time).poll(&mut Context::from_waker(Waker::noop())) {
                    Poll::Ready(Ok(record)) => RestorePacketState::Sealed(base, record),
                    Poll::Ready(Err(_)) => {
                        tracelimit::error_ratelimited!(
                            "microVM restore packet time fields were never sealed"
                        );
                        RestorePacketState::None
                    }
                    Poll::Pending => RestorePacketState::Pending(base, time),
                }
            }
            state => state,
        };
        matches!(self.restore, RestorePacketState::Sealed(..))
    }
}

/// Raw bidirectional microVM portb console.
#[derive(InspectMut)]
pub struct MicrovmPortb {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
    #[inspect(with = "VecDeque::len")]
    rx_buffer: VecDeque<u8>,
    #[inspect(with = "VecDeque::len")]
    tx_buffer: VecDeque<u8>,
    generation_id: [u8; GENERATION_ID_SIZE],
    generation_id_read_index: Option<usize>,
    /// The encoded restore packet, from its first selection until it is read.
    #[inspect(with = "VecDeque::len")]
    restore_packet: VecDeque<u8>,
    restore_packet_selected: bool,
    restore_processor_target_available: bool,
    restore_memory_target_available: bool,
    restore_memory_expansion_available: bool,
    input_gated: bool,
    #[inspect(skip)]
    time_abi: PortbTimeAbi,
    #[inspect(with = "VecDeque::len")]
    time_window: VecDeque<u8>,
    #[inspect(skip)]
    rx_waker: Option<Waker>,
    #[inspect(skip)]
    tx_waker: Option<Waker>,
    #[inspect(skip)]
    output_drain_requests: Option<mesh::Receiver<mesh::rpc::FailableRpc<MicrovmPortbDrain, ()>>>,
    #[inspect(skip)]
    output_drain: Option<(MicrovmPortbDrain, mesh::rpc::FailableRpc<(), ()>)>,
}

impl MicrovmPortb {
    /// Creates a portb console using `io` as its host endpoint, with the NVX
    /// time ABI v1 configuration `time_abi`: restore packet version 4 and the
    /// time-sample window at port `0xeb`.
    pub fn new(
        io: Box<dyn SerialIo>,
        generation_id: [u8; GENERATION_ID_SIZE],
        time_abi: MicrovmPortbTimeAbi,
    ) -> Self {
        let MicrovmPortbTimeAbi {
            generation,
            utc_offset_ms,
            sample_delay_us,
            test_hooks,
            restore,
        } = time_abi;
        let (processor_target, memory_target, memory_expansion) =
            restore.as_ref().map_or((false, false, false), |source| {
                (
                    source.base.online_vp_count != 0,
                    source.base.memory_target,
                    !source.base.ranges.is_empty(),
                )
            });
        let (restore, packet_selected) = match restore {
            Some(source) => (
                RestorePacketState::Pending(source.base, source.time),
                source.selected,
            ),
            None => (RestorePacketState::None, None),
        };
        Self {
            io_region: ("microvm-portb", DATA_PORT..=TIME_WINDOW_PORT),
            io,
            rx_buffer: VecDeque::new(),
            tx_buffer: VecDeque::new(),
            generation_id,
            generation_id_read_index: None,
            restore_packet: VecDeque::new(),
            restore_packet_selected: false,
            restore_processor_target_available: processor_target,
            restore_memory_target_available: memory_target,
            restore_memory_expansion_available: memory_expansion,
            input_gated: false,
            time_abi: PortbTimeAbi {
                generation,
                utc_offset_ns: i128::from(utc_offset_ms) * 1_000_000,
                sample_delay: std::time::Duration::from_micros(sample_delay_us.into()),
                test_hooks,
                restore,
                packet_selected,
            },
            time_window: VecDeque::new(),
            rx_waker: None,
            tx_waker: None,
            output_drain_requests: None,
            output_drain: None,
        }
    }

    /// Handles selector `0xa5`: encodes the sealed packet with host UTC at
    /// its first selection, then selects it.
    fn select_restore_packet(&mut self) {
        let time_abi = &mut self.time_abi;
        if time_abi.poll_sealed() {
            let RestorePacketState::Sealed(base, time) =
                std::mem::replace(&mut time_abi.restore, RestorePacketState::Encoded)
            else {
                return;
            };
            if let Some(selected) = time_abi.packet_selected.take() {
                selected.send(());
            }
            let packet = RestorePacketV4 {
                base,
                time,
                utc_ns: time_abi.latch_utc_ns(),
            };
            match packet.encode() {
                Ok(bytes) => self.restore_packet = bytes.into(),
                Err(error) => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "microVM restore packet is invalid"
                    );
                    self.clear_restore_packet();
                    return;
                }
            }
        }
        if !self.restore_packet.is_empty() {
            self.generation_id_read_index = None;
            self.restore_packet_selected = true;
        }
    }

    /// Handles selector `0xa7`: latches a fresh time sample into the window.
    fn latch_time_sample(&mut self) {
        let sample = TimeSample {
            test_hooks: self.time_abi.test_hooks,
            generation: self.time_abi.generation,
            utc_ns: self.time_abi.latch_utc_ns(),
        };
        self.time_window = sample.encode().into();
    }

    fn clear_restore_packet(&mut self) {
        self.restore_packet.clear();
        self.restore_packet_selected = false;
        self.restore_processor_target_available = false;
        self.restore_memory_target_available = false;
        self.restore_memory_expansion_available = false;
        self.time_abi.restore = RestorePacketState::None;
    }

    /// Installs the output drain request channel.
    ///
    /// A drain request writes and flushes every byte the device has accepted.
    /// [`MicrovmPortbDrain::Close`] then closes the endpoint before process
    /// exit, while [`MicrovmPortbDrain::Flush`] keeps it open because a
    /// snapshot capture may still roll back and resume the guest.
    pub fn with_output_drain(
        mut self,
        requests: Option<mesh::Receiver<mesh::rpc::FailableRpc<MicrovmPortbDrain, ()>>>,
    ) -> Self {
        self.output_drain_requests = requests;
        self
    }

    /// Completes an active flush that has no connected endpoint to write to.
    ///
    /// The accepted bytes stay buffered, so a snapshot saves them for the
    /// restored VM, as it does for output accepted while no peer is connected.
    fn complete_flush_without_endpoint(&mut self, cx: &mut Context<'_>) {
        if matches!(self.output_drain, Some((MicrovmPortbDrain::Flush, _)))
            && let Some((_, request)) = self.output_drain.take()
        {
            tracing::debug!(
                buffered_bytes = self.tx_buffer.len(),
                "microVM portb endpoint is not connected; keeping buffered output"
            );
            request.complete(Ok(()));
            // Poll again to accept the next drain request.
            cx.waker().wake_by_ref();
        }
    }

    fn poll_rx(&mut self, cx: &mut Context<'_>) {
        let mut buffer = [0; 256];
        loop {
            if self.rx_buffer.len() == BUFFER_MAX {
                self.rx_waker = Some(cx.waker().clone());
                return;
            }

            let available = BUFFER_MAX - self.rx_buffer.len();
            let read_len = available.min(buffer.len());
            match Pin::new(&mut self.io).poll_read(cx, &mut buffer[..read_len]) {
                Poll::Ready(Ok(0)) | Poll::Pending => return,
                Poll::Ready(Ok(count)) => self.rx_buffer.extend(&buffer[..count]),
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "microVM portb input failed"
                    );
                    return;
                }
            }
        }
    }

    fn poll_tx(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.tx_buffer.is_empty() {
            let (buffer, _) = self.tx_buffer.as_slices();
            match Pin::new(&mut self.io).poll_write(cx, buffer) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(count)) => {
                    self.tx_buffer.drain(..count);
                }
                Poll::Ready(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        len = buffer.len(),
                        error = &error as &dyn std::error::Error,
                        "microVM portb output failed; dropping buffered bytes"
                    );
                    self.tx_buffer.clear();
                    self.tx_waker = Some(cx.waker().clone());
                    return Poll::Ready(Err(error));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        self.tx_waker = Some(cx.waker().clone());
        Poll::Ready(Ok(()))
    }

    fn wake_tx(&mut self) {
        if let Some(waker) = self.tx_waker.take() {
            waker.wake();
        }
    }

    fn wake_rx(&mut self) {
        if let Some(waker) = self.rx_waker.take() {
            waker.wake();
        }
    }
}

impl ChangeDeviceState for MicrovmPortb {
    fn start(&mut self) {}

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.input_gated = true;
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.input_gated = false;
        self.wake_rx();
        Ok(())
    }

    async fn stop(&mut self) {
        // Drain every byte the endpoint accepts immediately. A guest-requested
        // snapshot flushes the output before stopping the device. Any remaining
        // VMM-owned bytes are serialized and retried against the reconstructed
        // endpoint after restore.
        let _ = self.poll_tx(&mut Context::from_waker(Waker::noop()));
    }

    async fn reset(&mut self) {
        self.rx_buffer.clear();
        self.tx_buffer.clear();
        self.generation_id_read_index = None;
        self.input_gated = false;
        self.time_window.clear();
        self.clear_restore_packet();
    }
}

impl ChipsetDevice for MicrovmPortb {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for MicrovmPortb {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if self.output_drain.is_none()
            && let Some(requests) = &mut self.output_drain_requests
        {
            match requests.poll_recv(cx) {
                Poll::Ready(Ok(request)) => {
                    let (drain, request) = request.split();
                    tracing::debug!(
                        buffered_bytes = self.tx_buffer.len(),
                        ?drain,
                        "draining microVM portb output"
                    );
                    self.output_drain = Some((drain, request));
                }
                Poll::Ready(Err(_)) => self.output_drain_requests = None,
                Poll::Pending => {}
            }
        }
        if !self.io.is_connected() {
            match self.io.poll_connect(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "microVM portb backend connection failed"
                    );
                    self.complete_flush_without_endpoint(cx);
                    return;
                }
                Poll::Pending => {
                    self.complete_flush_without_endpoint(cx);
                    return;
                }
            }
        }
        if !self.input_gated {
            self.poll_rx(cx);
        }
        let output = self.poll_tx(cx);
        if self.output_drain.is_some() {
            let output = match output {
                Poll::Ready(Ok(())) => Pin::new(&mut self.io).poll_flush(cx),
                output => output,
            };
            if let Poll::Ready(result) = output
                && let Some((drain, request)) = self.output_drain.take()
            {
                match drain {
                    MicrovmPortbDrain::Flush => {
                        // Poll again to accept the next drain request.
                        cx.waker().wake_by_ref();
                    }
                    MicrovmPortbDrain::Close => {
                        // Closing the endpoint publishes EOF only after every
                        // accepted byte has reached it. The controller also
                        // waits for the relay.
                        self.io = Box::new(Disconnected);
                        self.output_drain_requests = None;
                    }
                }
                request.handle_failable_sync(|()| result);
            }
        }
    }
}

impl PortIoIntercept for MicrovmPortb {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.is_empty() {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        data.fill(0);
        match io_port {
            // Selected records are read 1, 2, or 4 bytes at a time; console
            // input stays one byte per read.
            DATA_PORT => {
                if self.generation_id_read_index.is_some() {
                    for byte in data.iter_mut() {
                        let Some(index) = self.generation_id_read_index else {
                            break;
                        };
                        *byte = self.generation_id[index];
                        self.generation_id_read_index =
                            (index + 1 < self.generation_id.len()).then_some(index + 1);
                    }
                } else if self.restore_packet_selected {
                    for byte in data.iter_mut() {
                        let Some(value) = self.restore_packet.pop_front() else {
                            break;
                        };
                        *byte = value;
                    }
                    if self.restore_packet.is_empty() {
                        self.restore_packet_selected = false;
                        self.restore_processor_target_available = false;
                        self.restore_memory_target_available = false;
                        self.restore_memory_expansion_available = false;
                    }
                } else if !self.input_gated {
                    data[0] = self.rx_buffer.pop_front().unwrap_or(0);
                    self.wake_rx();
                }
            }
            STATUS_PORT => {
                data[0] = if !self.input_gated
                    && self.generation_id_read_index.is_none()
                    && !self.restore_packet_selected
                    && !self.rx_buffer.is_empty()
                {
                    STATUS_INPUT_AVAILABLE
                } else {
                    0
                };
                let sealed_packet = self.time_abi.poll_sealed();
                if matches!(self.time_abi.restore, RestorePacketState::None)
                    && self.restore_packet.is_empty()
                {
                    // An unsealed packet was dropped: no targets.
                    self.restore_processor_target_available = false;
                    self.restore_memory_target_available = false;
                    self.restore_memory_expansion_available = false;
                }
                if !self.restore_packet.is_empty() || sealed_packet {
                    data[0] |= STATUS_RESTORE_PACKET_AVAILABLE;
                }
                if self.restore_processor_target_available {
                    data[0] |= STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE;
                }
                if self.restore_memory_target_available {
                    data[0] |= STATUS_RESTORE_MEMORY_TARGET_AVAILABLE;
                }
                if self.restore_memory_expansion_available {
                    data[0] |= STATUS_RESTORE_MEMORY_EXPANSION_AVAILABLE;
                }
                data[0] |= STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE;
            }
            TIME_WINDOW_PORT => {
                for byte in data.iter_mut() {
                    let Some(value) = self.time_window.pop_front() else {
                        break;
                    };
                    *byte = value;
                }
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        match io_port {
            DATA_PORT => {
                let available = BUFFER_MAX - self.tx_buffer.len();
                self.tx_buffer.extend(data.iter().copied().take(available));
                if data.len() > available {
                    tracelimit::warn_ratelimited!(
                        dropped = data.len() - available,
                        "microVM portb output buffer full; dropping newest bytes"
                    );
                }
                self.wake_tx();
            }
            STATUS_PORT => match data.first() {
                Some(&RESTORE_PACKET_SELECT) => self.select_restore_packet(),
                Some(&GENERATION_ID_SELECT) => {
                    self.restore_packet_selected = false;
                    self.generation_id_read_index = Some(0);
                }
                Some(&TIME_SAMPLE_SELECT) => self.latch_time_sample(),
                _ => {}
            },
            TIME_WINDOW_PORT => {}
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmPortb {
    type SavedState = MicrovmPortbSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(MicrovmPortbSavedState {
            rx_buffer: self.rx_buffer.iter().copied().collect(),
            tx_buffer: self.tx_buffer.iter().copied().collect(),
        })
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        if state.rx_buffer.len() > BUFFER_MAX || state.tx_buffer.len() > BUFFER_MAX {
            return Err(RestoreError::InvalidSavedState(
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!("microVM portb buffer exceeds {BUFFER_MAX} bytes"),
                )
                .into(),
            ));
        }
        self.rx_buffer = state.rx_buffer.into();
        self.tx_buffer = state.tx_buffer.into();
        self.generation_id_read_index = None;
        self.restore_packet_selected = false;
        self.time_window.clear();
        Ok(())
    }
}

/// Saved guest-visible bytes owned by the microVM portb device.
#[derive(mesh::payload::Protobuf, SavedStateRoot)]
#[mesh(package = "chipset.microvm_portb")]
pub struct MicrovmPortbSavedState {
    /// Host bytes accepted but not consumed by the guest.
    #[mesh(1)]
    pub rx_buffer: Vec<u8>,
    /// Guest bytes accepted but not acknowledged by the host endpoint.
    #[mesh(2)]
    pub tx_buffer: Vec<u8>,
}

/// microVM process-status shutdown port.
#[derive(InspectMut)]
pub struct MicrovmShutdown {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(skip)]
    power_request: PowerRequestClient,
}

impl MicrovmShutdown {
    /// Creates the shutdown device.
    pub fn new(power_request: PowerRequestClient) -> Self {
        Self {
            io_region: ("microvm-shutdown", SHUTDOWN_PORT..=SHUTDOWN_PORT),
            power_request,
        }
    }
}

impl ChangeDeviceState for MicrovmShutdown {
    fn start(&mut self) {}
    async fn stop(&mut self) {}
    async fn reset(&mut self) {}
}

impl ChipsetDevice for MicrovmShutdown {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PortIoIntercept for MicrovmShutdown {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port != SHUTDOWN_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        data.fill(0xff);
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if io_port != SHUTDOWN_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        self.power_request
            .power_request(PowerRequest::PowerOffWithStatus {
                code: data.first().copied().unwrap_or(0),
            });
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmShutdown {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

/// Snapshot-request port with at most one unacknowledged notification.
#[derive(InspectMut)]
pub struct MicrovmSnapshotRequest {
    #[inspect(skip)]
    io_region: (&'static str, RangeInclusive<u16>),
    #[inspect(skip)]
    notify: Option<mesh::Sender<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
    input_gate_timeout: std::time::Duration,
    #[inspect(skip)]
    pending: Option<PendingSnapshotWrite>,
    #[inspect(skip)]
    poll_waker: Option<Waker>,
}

struct PendingSnapshotWrite {
    release_write: mesh::OneshotReceiver<()>,
    deferred_write: Option<DeferredWrite>,
    write_completed: Option<mesh::OneshotSender<()>>,
    transaction_complete: mesh::rpc::PendingRpc<()>,
    write_released: bool,
}

impl MicrovmSnapshotRequest {
    /// Creates a snapshot-request device with an optional asynchronous notification target.
    pub fn new(
        notify: Option<mesh::Sender<chipset_resources::microvm::MicrovmSnapshotBoundaryRequest>>,
        input_gate_timeout: std::time::Duration,
    ) -> Self {
        Self {
            io_region: ("microvm-snapshot-request", SNAPSHOT_PORT..=SNAPSHOT_PORT),
            notify,
            input_gate_timeout,
            pending: None,
            poll_waker: None,
        }
    }

    fn poll_pending(&mut self) {
        use std::future::Future;

        let mut cx = Context::from_waker(self.poll_waker.as_ref().unwrap_or(Waker::noop()));
        let Some(pending) = &mut self.pending else {
            return;
        };
        if !pending.write_released
            && Pin::new(&mut pending.release_write)
                .poll(&mut cx)
                .is_ready()
        {
            pending.write_released = true;
            if let Some(deferred_write) = pending.deferred_write.take() {
                deferred_write.complete();
            }
            if let Some(write_completed) = pending.write_completed.take() {
                write_completed.send(());
            }
        }
        if Pin::new(&mut pending.transaction_complete)
            .poll(&mut cx)
            .is_ready()
        {
            self.pending = None;
        }
    }
}

impl ChangeDeviceState for MicrovmSnapshotRequest {
    fn start(&mut self) {}
    async fn stop(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.write_released)
        {
            return;
        }
        if let Some(mut pending) = self.pending.take() {
            if let Some(deferred_write) = pending.deferred_write.take() {
                deferred_write.complete_error(IoError::InvalidRegister);
            }
            if let Some(write_completed) = pending.write_completed.take() {
                write_completed.send(());
            }
        }
    }
    async fn reset(&mut self) {
        self.stop().await;
    }
}

impl ChipsetDevice for MicrovmSnapshotRequest {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for MicrovmSnapshotRequest {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.poll_waker = Some(cx.waker().clone());
        self.poll_pending();
    }
}

impl PortIoIntercept for MicrovmSnapshotRequest {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port != SNAPSHOT_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        data.fill(0xff);
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        use mesh::rpc::RpcSend;

        if io_port != SNAPSHOT_PORT {
            return IoResult::Err(IoError::InvalidRegister);
        }
        // A completed transaction can resume the vCPUs before the device's
        // poll task runs. Reap it before deciding this write is a duplicate.
        self.poll_pending();
        if self.pending.is_some() {
            tracelimit::warn_ratelimited!("coalescing duplicate microVM snapshot request");
            return IoResult::Ok;
        }
        if let Some(notify) = &self.notify {
            let scratch_policy = if data.first().copied().unwrap_or(0) == 0 {
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            } else {
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
            };
            let (deferred_write, token) = defer_write();
            let (release_write, release_recv) = mesh::oneshot();
            let (write_completed, write_completed_recv) = mesh::oneshot();
            let transaction_complete = notify.call(
                |transaction_complete| chipset_resources::microvm::MicrovmSnapshotBoundaryRequest {
                    scratch_policy,
                    release_write,
                    write_completed: write_completed_recv,
                    input_gate_timeout: self.input_gate_timeout,
                    transaction_complete,
                },
                (),
            );
            self.pending = Some(PendingSnapshotWrite {
                release_write: release_recv,
                deferred_write: Some(deferred_write),
                write_completed: Some(write_completed),
                transaction_complete,
                write_released: false,
            });
            if let Some(waker) = &self.poll_waker {
                waker.wake_by_ref();
            }
            return IoResult::Defer(token);
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        std::slice::from_ref(&self.io_region)
    }
}

impl SaveRestore for MicrovmSnapshotRequest {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncRead;
    use futures::AsyncWrite;
    use futures::FutureExt;
    use mesh::rpc::RpcSend;
    use parking_lot::Mutex;
    use serial_core::disconnected::Disconnected;
    use serial_core::serial_io::Connected;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use test_with_tracing::test;

    const TEST_GENERATION_ID: [u8; GENERATION_ID_SIZE] = [0x3c; GENERATION_ID_SIZE];

    fn test_time_abi(
        restore: Option<chipset_resources::microvm::MicrovmRestorePacketSource>,
    ) -> MicrovmPortbTimeAbi {
        MicrovmPortbTimeAbi {
            generation: 7,
            utc_offset_ms: 0,
            sample_delay_us: 0,
            test_hooks: true,
            restore,
        }
    }

    #[derive(Default)]
    struct OutputState {
        bytes: Vec<u8>,
        writable: bool,
        flushed: bool,
        closed: bool,
        error: Option<ErrorKind>,
    }

    struct BufferedOutput(Arc<Mutex<OutputState>>);

    impl Drop for BufferedOutput {
        fn drop(&mut self) {
            self.0.lock().closed = true;
        }
    }

    impl AsyncRead for BufferedOutput {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for BufferedOutput {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut state = self.0.lock();
            if let Some(error) = state.error {
                return Poll::Ready(Err(error.into()));
            }
            if !state.writable {
                return Poll::Pending;
            }
            let count = buffer.len().min(3);
            state.bytes.extend_from_slice(&buffer[..count]);
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.0.lock().flushed {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[test]
    fn portb_exit_drain_waits_for_pending_writes_and_flush_before_closing() {
        let state = Arc::new(Mutex::new(OutputState::default()));
        let (requests, receiver) = mesh::channel();
        let mut portb = MicrovmPortb::new(
            Box::new(Connected::new(BufferedOutput(state.clone()))),
            TEST_GENERATION_ID,
            test_time_abi(None),
        )
        .with_output_drain(Some(receiver));
        let payload = b"OPENVMM-SNAPSHOT-RESTORE-OK\n\0\xff";
        assert!(matches!(portb.io_write(DATA_PORT, payload), IoResult::Ok));
        let mut result =
            Box::pin(requests.call_failable(std::convert::identity, MicrovmPortbDrain::Close));
        let mut cx = Context::from_waker(Waker::noop());
        portb.poll_device(&mut cx);
        assert!(result.as_mut().now_or_never().is_none());
        assert!(!state.lock().closed);
        assert_eq!(portb.save().unwrap().tx_buffer, payload);

        state.lock().writable = true;
        portb.poll_device(&mut cx);
        assert!(result.as_mut().now_or_never().is_none());
        assert_eq!(state.lock().bytes, payload);
        assert!(!state.lock().closed);

        state.lock().flushed = true;
        portb.poll_device(&mut cx);
        result.now_or_never().unwrap().unwrap();
        assert!(state.lock().closed);
        assert!(portb.tx_buffer.is_empty());
    }

    #[test]
    fn portb_exit_drain_reports_output_failure() {
        let state = Arc::new(Mutex::new(OutputState {
            error: Some(ErrorKind::BrokenPipe),
            ..Default::default()
        }));
        let (requests, receiver) = mesh::channel();
        let mut portb = MicrovmPortb::new(
            Box::new(Connected::new(BufferedOutput(state.clone()))),
            TEST_GENERATION_ID,
            test_time_abi(None),
        )
        .with_output_drain(Some(receiver));
        assert!(matches!(portb.io_write(DATA_PORT, b"marker"), IoResult::Ok));
        let result = requests.call_failable(std::convert::identity, MicrovmPortbDrain::Close);
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        assert!(result.now_or_never().unwrap().is_err());
        assert!(state.lock().closed);
    }

    #[test]
    fn portb_snapshot_flush_delivers_output_and_keeps_endpoint_open() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        use std::task::Wake;

        struct FlushWake(AtomicBool);
        impl Wake for FlushWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let wake = Arc::new(FlushWake(AtomicBool::new(false)));
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let state = Arc::new(Mutex::new(OutputState::default()));
        let (requests, receiver) = mesh::channel();
        let mut portb = MicrovmPortb::new(
            Box::new(Connected::new(BufferedOutput(state.clone()))),
            TEST_GENERATION_ID,
            test_time_abi(None),
        )
        .with_output_drain(Some(receiver));
        // The guest writes a marker just before its snapshot request, while
        // the endpoint does not yet accept writes.
        let marker = b"OPENVMM-LINUX-MPTABLE-SNAPSHOT-READY";
        assert!(matches!(portb.io_write(DATA_PORT, marker), IoResult::Ok));
        let mut flush =
            Box::pin(requests.call_failable(std::convert::identity, MicrovmPortbDrain::Flush));
        portb.poll_device(&mut cx);
        assert!(flush.as_mut().now_or_never().is_none());

        state.lock().writable = true;
        portb.poll_device(&mut cx);
        assert!(flush.as_mut().now_or_never().is_none());
        assert_eq!(state.lock().bytes, marker);

        state.lock().flushed = true;
        wake.0.store(false, Ordering::Relaxed);
        portb.poll_device(&mut cx);
        flush.now_or_never().unwrap().unwrap();
        assert!(wake.0.load(Ordering::Relaxed));
        assert!(!state.lock().closed);

        // A capture that rolls back keeps delivering guest output.
        assert!(matches!(portb.io_write(DATA_PORT, b"+"), IoResult::Ok));
        portb.poll_device(&mut cx);
        assert_eq!(
            state.lock().bytes,
            [marker.as_slice(), b"+".as_slice()].concat()
        );

        // A committed capture saves no stale source output, and the source
        // endpoint holds the marker when the terminating process closes it.
        futures::executor::block_on(portb.stop());
        assert!(portb.save().unwrap().tx_buffer.is_empty());
        drop(portb);
        assert!(state.lock().closed);
    }

    #[test]
    fn portb_snapshot_flush_without_endpoint_keeps_output_for_restore() {
        let (requests, receiver) = mesh::channel();
        let mut portb = MicrovmPortb::new(
            Box::new(Disconnected),
            TEST_GENERATION_ID,
            test_time_abi(None),
        )
        .with_output_drain(Some(receiver));
        assert!(matches!(portb.io_write(DATA_PORT, b"marker"), IoResult::Ok));
        let flush = requests.call_failable(std::convert::identity, MicrovmPortbDrain::Flush);
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        flush.now_or_never().unwrap().unwrap();
        assert_eq!(portb.save().unwrap().tx_buffer, b"marker");
    }

    #[test]
    fn portb_snapshot_flush_failure_keeps_endpoint_for_exit_drain() {
        let state = Arc::new(Mutex::new(OutputState {
            error: Some(ErrorKind::BrokenPipe),
            ..Default::default()
        }));
        let (requests, receiver) = mesh::channel();
        let mut portb = MicrovmPortb::new(
            Box::new(Connected::new(BufferedOutput(state.clone()))),
            TEST_GENERATION_ID,
            test_time_abi(None),
        )
        .with_output_drain(Some(receiver));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(portb.io_write(DATA_PORT, b"marker"), IoResult::Ok));
        let flush = requests.call_failable(std::convert::identity, MicrovmPortbDrain::Flush);
        portb.poll_device(&mut cx);
        assert!(flush.now_or_never().unwrap().is_err());
        assert!(!state.lock().closed);
        assert_eq!(portb.save().unwrap().tx_buffer, b"marker");

        *state.lock() = OutputState {
            writable: true,
            flushed: true,
            ..Default::default()
        };
        let close = requests.call_failable(std::convert::identity, MicrovmPortbDrain::Close);
        portb.poll_device(&mut cx);
        close.now_or_never().unwrap().unwrap();
        assert_eq!(state.lock().bytes, b"marker");
        assert!(state.lock().closed);
    }

    #[test]
    fn portb_output_error_preserves_the_next_write_wakeup() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        use std::task::Wake;

        struct OutputWake(AtomicBool);
        impl Wake for OutputWake {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let wake = Arc::new(OutputWake(AtomicBool::new(false)));
        let waker = Waker::from(wake.clone());
        let mut cx = Context::from_waker(&waker);
        let state = Arc::new(Mutex::new(OutputState {
            error: Some(ErrorKind::Other),
            ..Default::default()
        }));
        let mut portb = MicrovmPortb::new(
            Box::new(Connected::new(BufferedOutput(state.clone()))),
            TEST_GENERATION_ID,
            test_time_abi(None),
        );
        assert!(matches!(portb.io_write(DATA_PORT, b"lost"), IoResult::Ok));
        portb.poll_device(&mut cx);
        state.lock().error = None;
        state.lock().writable = true;
        assert!(matches!(
            portb.io_write(DATA_PORT, b"next write"),
            IoResult::Ok
        ));
        assert!(wake.0.load(Ordering::Relaxed));
        portb.poll_device(&mut cx);
        assert_eq!(state.lock().bytes, b"next write");
    }

    struct ConnectWithByte {
        connected: bool,
        byte: Option<u8>,
    }

    impl InspectMut for ConnectWithByte {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl SerialIo for ConnectWithByte {
        fn is_connected(&self) -> bool {
            self.connected
        }

        fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.connected = true;
            Poll::Ready(Ok(()))
        }

        fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncRead for ConnectWithByte {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let Some(byte) = self.byte.take() else {
                return Poll::Pending;
            };
            data[0] = byte;
            Poll::Ready(Ok(1))
        }
    }

    impl AsyncWrite for ConnectWithByte {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn portb_preserves_wide_binary_output_and_zero_fills_reads() {
        let mut portb = time_abi_portb(None);
        assert!(matches!(
            portb.io_write(DATA_PORT, b"\0\xffA\x80"),
            IoResult::Ok
        ));
        assert_eq!(portb.tx_buffer, b"\0\xffA\x80");

        portb.rx_buffer.extend([0x5a, 0x6b]);
        let mut status = [0xff; 4];
        assert!(matches!(
            portb.io_read(STATUS_PORT, &mut status),
            IoResult::Ok
        ));
        assert_eq!(
            status,
            [
                STATUS_INPUT_AVAILABLE
                    | STATUS_GENERATION_ID_AVAILABLE
                    | STATUS_TIME_SAMPLE_AVAILABLE,
                0,
                0,
                0
            ]
        );

        let mut data = [0xff; 4];
        assert!(matches!(portb.io_read(DATA_PORT, &mut data), IoResult::Ok));
        assert_eq!(data, [0x5a, 0, 0, 0]);
    }

    #[test]
    fn pending_portb_bytes_survive_restore_and_the_restore_packet_is_private() {
        let mut portb = time_abi_portb(None);
        portb.rx_buffer.extend([1, 2, 3]);
        portb.tx_buffer.extend([4, 5, 6]);
        let state = portb.save().unwrap();

        let base = RestorePacketBase {
            online_vp_count: 0,
            memory_target: false,
            ack_required: false,
            generation: 8,
            ranges: Vec::new(),
            entropy: [0x7e; 64],
        };
        let (send, recv) = mesh::oneshot();
        let mut restored = MicrovmPortb::new(
            Box::new(Disconnected),
            [2; 16],
            test_time_abi(Some(
                chipset_resources::microvm::MicrovmRestorePacketSource {
                    base: base.clone(),
                    time: recv,
                    selected: None,
                },
            )),
        );
        restored.restore(state).unwrap();
        assert_eq!(restored.rx_buffer, [1, 2, 3]);
        assert_eq!(restored.tx_buffer, [4, 5, 6]);
        assert_eq!(restored.generation_id, [2; 16]);

        let mut data = [0];
        for expected in [1, 2, 3] {
            assert!(matches!(
                restored.io_read(DATA_PORT, &mut data),
                IoResult::Ok
            ));
            assert_eq!(data, [expected]);
        }
        assert!(matches!(
            restored.io_read(DATA_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(data, [0]);

        // The restored process serves its own packet, never saved state.
        send.send(RestoreTimeRecord {
            downtime_ns: 1_000,
            downtime_utc: false,
            rate_deviation: 0,
            test_hooks: true,
        });
        assert!(matches!(
            restored.io_write(STATUS_PORT, &[RESTORE_PACKET_SELECT]),
            IoResult::Ok
        ));
        let bytes = read_record(&mut restored, DATA_PORT, base.encoded_len());
        assert_eq!(RestorePacketV4::decode(&bytes).unwrap().base, base);
    }

    #[test]
    fn generation_id_is_repeatable_and_not_consumed() {
        let mut portb = time_abi_portb(None);
        let mut data = [0];

        for _ in 0..2 {
            assert!(matches!(
                portb.io_read(STATUS_PORT, &mut data),
                IoResult::Ok
            ));
            assert_eq!(
                data,
                [STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE]
            );
            assert!(matches!(
                portb.io_write(STATUS_PORT, &[GENERATION_ID_SELECT]),
                IoResult::Ok
            ));
            for expected in TEST_GENERATION_ID {
                assert!(matches!(portb.io_read(DATA_PORT, &mut data), IoResult::Ok));
                assert_eq!(data, [expected]);
            }
        }
    }

    #[test]
    fn restore_packet_v4_advertises_its_targets() {
        let range = chipset_resources::microvm_time::RestoreMemoryRange {
            gpa_start: 0x2000_0000,
            length: 0x2000_0000,
        };
        for (online_vp_count, memory_target, ranges, targets) in [
            (
                2,
                false,
                Vec::new(),
                STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE,
            ),
            (0, true, Vec::new(), STATUS_RESTORE_MEMORY_TARGET_AVAILABLE),
            (
                0,
                true,
                vec![range],
                STATUS_RESTORE_MEMORY_TARGET_AVAILABLE | STATUS_RESTORE_MEMORY_EXPANSION_AVAILABLE,
            ),
            (
                2,
                true,
                vec![range],
                STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE
                    | STATUS_RESTORE_MEMORY_TARGET_AVAILABLE
                    | STATUS_RESTORE_MEMORY_EXPANSION_AVAILABLE,
            ),
        ] {
            let base = RestorePacketBase {
                online_vp_count,
                memory_target,
                ack_required: true,
                generation: 7,
                ranges,
                entropy: [0x5a; 64],
            };
            let (send, recv) = mesh::oneshot();
            let mut portb = time_abi_portb(Some(
                chipset_resources::microvm::MicrovmRestorePacketSource {
                    base: base.clone(),
                    time: recv,
                    selected: None,
                },
            ));
            send.send(RestoreTimeRecord {
                downtime_ns: 0,
                downtime_utc: false,
                rate_deviation: 0,
                test_hooks: true,
            });
            assert_eq!(
                read_status(&mut portb),
                STATUS_RESTORE_PACKET_AVAILABLE
                    | targets
                    | STATUS_GENERATION_ID_AVAILABLE
                    | STATUS_TIME_SAMPLE_AVAILABLE
            );
            assert!(matches!(
                portb.io_write(STATUS_PORT, &[RESTORE_PACKET_SELECT]),
                IoResult::Ok
            ));
            let bytes = read_record(&mut portb, DATA_PORT, base.encoded_len());
            assert_eq!(RestorePacketV4::decode(&bytes).unwrap().base, base);
            // Reading the whole packet clears every target.
            assert_eq!(
                read_status(&mut portb),
                STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE
            );
        }
    }

    #[test]
    fn accepted_connection_is_polled_immediately() {
        let mut portb = MicrovmPortb::new(
            Box::new(ConnectWithByte {
                connected: false,
                byte: Some(0x5a),
            }),
            TEST_GENERATION_ID,
            test_time_abi(None),
        );
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(portb.rx_buffer, [0x5a]);
    }

    fn time_abi_portb(
        restore: Option<chipset_resources::microvm::MicrovmRestorePacketSource>,
    ) -> MicrovmPortb {
        MicrovmPortb::new(
            Box::new(Disconnected),
            TEST_GENERATION_ID,
            test_time_abi(restore),
        )
    }

    fn read_status(portb: &mut MicrovmPortb) -> u8 {
        let mut data = [0];
        assert!(matches!(
            portb.io_read(STATUS_PORT, &mut data),
            IoResult::Ok
        ));
        data[0]
    }

    fn read_record(portb: &mut MicrovmPortb, port: u16, len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        while bytes.len() < len {
            let mut data = [0; 4];
            assert!(matches!(portb.io_read(port, &mut data), IoResult::Ok));
            bytes.extend(data);
        }
        bytes.truncate(len);
        bytes
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    #[test]
    fn time_abi_portb_cold_boot_offers_time_samples() {
        let mut portb = time_abi_portb(None);
        assert_eq!(portb.get_static_regions()[0].1, 0xe9..=0xeb);
        assert_eq!(
            read_status(&mut portb),
            STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE
        );
        // No packet on a cold boot: selecting it reads console input.
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[RESTORE_PACKET_SELECT]),
            IoResult::Ok
        ));
        assert!(!portb.restore_packet_selected);

        // The window is empty until a sample is latched.
        assert_eq!(read_record(&mut portb, TIME_WINDOW_PORT, 4), [0; 4]);
        let before = now_ns();
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[TIME_SAMPLE_SELECT]),
            IoResult::Ok
        ));
        let after = now_ns();
        let sample = TimeSample::decode(&read_record(&mut portb, TIME_WINDOW_PORT, 16)).unwrap();
        assert_eq!((sample.generation, sample.test_hooks), (7, true));
        assert!((before..=after).contains(&sample.utc_ns));
        // Reads past the end are zero, and selecting a sample keeps the
        // generation-ID selection.
        assert_eq!(read_record(&mut portb, TIME_WINDOW_PORT, 4), [0; 4]);
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[GENERATION_ID_SELECT]),
            IoResult::Ok
        ));
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[TIME_SAMPLE_SELECT]),
            IoResult::Ok
        ));
        assert_eq!(read_record(&mut portb, DATA_PORT, 16), TEST_GENERATION_ID);
    }

    #[test]
    fn time_abi_portb_serves_restore_packet_v4() {
        let base = RestorePacketBase {
            online_vp_count: 2,
            memory_target: false,
            ack_required: true,
            generation: 7,
            ranges: Vec::new(),
            entropy: [0x3c; 64],
        };
        let time = RestoreTimeRecord {
            downtime_ns: 1_000_000,
            downtime_utc: true,
            rate_deviation: -5,
            test_hooks: true,
        };
        let (send, recv) = mesh::oneshot();
        let (selected_send, mut selected) = mesh::oneshot();
        let mut portb = time_abi_portb(Some(
            chipset_resources::microvm::MicrovmRestorePacketSource {
                base: base.clone(),
                time: recv,
                selected: Some(selected_send),
            },
        ));
        // Unsealed: only the targets are advertised.
        assert_eq!(
            read_status(&mut portb),
            STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE
                | STATUS_GENERATION_ID_AVAILABLE
                | STATUS_TIME_SAMPLE_AVAILABLE
        );
        send.send(time);
        assert_eq!(
            read_status(&mut portb),
            STATUS_RESTORE_PACKET_AVAILABLE
                | STATUS_RESTORE_PROCESSOR_TARGET_AVAILABLE
                | STATUS_GENERATION_ID_AVAILABLE
                | STATUS_TIME_SAMPLE_AVAILABLE
        );
        assert!(
            (&mut selected).now_or_never().is_none(),
            "selection is notified only at the first selection"
        );

        let before = now_ns();
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[RESTORE_PACKET_SELECT]),
            IoResult::Ok
        ));
        let after = now_ns();
        assert!(matches!(selected.now_or_never(), Some(Ok(()))));
        let bytes = read_record(&mut portb, DATA_PORT, base.encoded_len());
        let packet = RestorePacketV4::decode(&bytes).unwrap();
        assert_eq!((packet.base, packet.time), (base, time));
        assert!((before..=after).contains(&packet.utc_ns));

        // The packet is consumed once read.
        assert_eq!(
            read_status(&mut portb),
            STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE
        );
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[RESTORE_PACKET_SELECT]),
            IoResult::Ok
        ));
        assert!(!portb.restore_packet_selected);
    }

    #[test]
    fn time_abi_portb_drops_an_unsealed_packet() {
        let (send, recv) = mesh::oneshot::<RestoreTimeRecord>();
        let mut portb = time_abi_portb(Some(
            chipset_resources::microvm::MicrovmRestorePacketSource {
                base: RestorePacketBase {
                    online_vp_count: 2,
                    memory_target: false,
                    ack_required: false,
                    generation: 1,
                    ranges: Vec::new(),
                    entropy: [0; 64],
                },
                time: recv,
                selected: None,
            },
        ));
        drop(send);
        assert_eq!(
            read_status(&mut portb),
            STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE
        );
    }

    #[test]
    fn time_abi_portb_window_is_not_saved() {
        let mut portb = time_abi_portb(None);
        assert!(matches!(
            portb.io_write(STATUS_PORT, &[TIME_SAMPLE_SELECT]),
            IoResult::Ok
        ));
        let state = portb.save().unwrap();
        portb.restore(state).unwrap();
        assert_eq!(read_record(&mut portb, TIME_WINDOW_PORT, 4), [0; 4]);

        assert!(matches!(
            portb.io_write(STATUS_PORT, &[TIME_SAMPLE_SELECT]),
            IoResult::Ok
        ));
        futures::executor::block_on(portb.reset());
        assert_eq!(read_record(&mut portb, TIME_WINDOW_PORT, 4), [0; 4]);
    }

    #[test]
    fn portb_input_gate_blocks_rx_but_not_tx() {
        let mut portb = MicrovmPortb::new(
            Box::new(ConnectWithByte {
                connected: true,
                byte: Some(0x5a),
            }),
            TEST_GENERATION_ID,
            test_time_abi(None),
        );
        portb.rx_buffer.push_back(0x44);
        futures::executor::block_on(portb.quiesce_input()).unwrap();
        assert!(matches!(portb.io_write(DATA_PORT, b"output"), IoResult::Ok));
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        let mut data = [0xff];
        assert!(matches!(
            portb.io_read(STATUS_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(
            data,
            [STATUS_GENERATION_ID_AVAILABLE | STATUS_TIME_SAMPLE_AVAILABLE]
        );
        assert!(matches!(portb.io_read(DATA_PORT, &mut data), IoResult::Ok));
        assert_eq!(data, [0]);
        assert_eq!(portb.rx_buffer, [0x44]);
        assert!(portb.tx_buffer.is_empty());

        futures::executor::block_on(portb.resume_input()).unwrap();
        portb.poll_device(&mut Context::from_waker(Waker::noop()));
        assert_eq!(portb.rx_buffer, [0x44, 0x5a]);
        assert!(matches!(portb.io_read(DATA_PORT, &mut data), IoResult::Ok));
        assert_eq!(data, [0x44]);
    }

    #[test]
    fn lifecycle_ports_preserve_status_and_remain_nonblocking() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let mut shutdown =
            MicrovmShutdown::new((move |request| captured.lock().push(request)).into());
        assert!(matches!(
            shutdown.io_write(SHUTDOWN_PORT, &[37, 99]),
            IoResult::Ok
        ));
        assert_eq!(
            *requests.lock(),
            [PowerRequest::PowerOffWithStatus { code: 37 }]
        );
        requests.lock().clear();
        assert!(matches!(
            shutdown.io_write(SHUTDOWN_PORT, &[0x25]),
            IoResult::Ok
        ));
        assert_eq!(
            *requests.lock(),
            [PowerRequest::PowerOffWithStatus { code: 0x25 }]
        );

        let mut snapshot = MicrovmSnapshotRequest::new(None, std::time::Duration::from_secs(1));
        let mut data = [0; 4];
        assert!(matches!(
            snapshot.io_read(SNAPSHOT_PORT, &mut data),
            IoResult::Ok
        ));
        assert_eq!(data, [0xff; 4]);
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[1]),
            IoResult::Ok
        ));
    }

    #[test]
    fn snapshot_requests_are_coalesced_until_acknowledged() {
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));

        let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
            panic!("snapshot write was not deferred");
        };
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[2, 3, 4, 5]),
            IoResult::Ok
        ));
        let mut first = recv.try_recv().unwrap();
        assert_eq!(
            first.scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
        );
        assert!(recv.try_recv().is_err());
        assert!(
            deferred_write
                .poll_write(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        first.release_write.send(());
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));
        assert!(
            deferred_write
                .poll_write(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(
            Pin::new(&mut first.write_completed)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[0]),
            IoResult::Ok
        ));
        assert!(recv.try_recv().is_err());

        first.transaction_complete.complete(());
        snapshot.poll_device(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[]),
            IoResult::Defer(_)
        ));
        assert_eq!(
            recv.try_recv().unwrap().scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
        );
    }

    #[test]
    fn snapshot_request_completed_transaction_is_reusable_without_poll() {
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        let mut cx = Context::from_waker(Waker::noop());

        let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
            panic!("snapshot write was not deferred");
        };
        let mut first = recv.try_recv().unwrap();
        first.release_write.send(());
        snapshot.poll_device(&mut cx);
        assert!(matches!(
            deferred_write.poll_write(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            Pin::new(&mut first.write_completed).poll(&mut cx),
            Poll::Ready(Ok(()))
        ));

        first.transaction_complete.complete(());
        // The next write can acquire the device before its poll task runs.
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[0]),
            IoResult::Defer(_)
        ));
        assert_eq!(
            recv.try_recv().unwrap().scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
        );
    }

    #[test]
    fn snapshot_request_early_completion_releases_previous_write() {
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        let mut cx = Context::from_waker(Waker::noop());

        let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[0]) else {
            panic!("snapshot write was not deferred");
        };
        let mut first = recv.try_recv().unwrap();
        first.release_write.send(());
        first.transaction_complete.complete(());

        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[1]),
            IoResult::Defer(_)
        ));
        assert!(matches!(
            deferred_write.poll_write(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            Pin::new(&mut first.write_completed).poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(
            recv.try_recv().unwrap().scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
        );
    }

    #[test]
    fn snapshot_request_closed_transaction_is_reusable() {
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        let mut cx = Context::from_waker(Waker::noop());

        let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
            panic!("snapshot write was not deferred");
        };
        let mut first = recv.try_recv().unwrap();
        drop(first.transaction_complete);

        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[0]),
            IoResult::Defer(_)
        ));
        assert!(matches!(
            deferred_write.poll_write(&mut cx),
            Poll::Ready(Err(IoError::NoResponse))
        ));
        assert!(matches!(
            Pin::new(&mut first.write_completed).poll(&mut cx),
            Poll::Ready(Err(_))
        ));
        assert_eq!(
            recv.try_recv().unwrap().scratch_policy,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
        );
    }

    #[test]
    fn snapshot_request_duplicate_preserves_poll_waker() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        #[derive(Default)]
        struct WakeCount(AtomicUsize);

        impl futures::task::ArcWake for WakeCount {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let wakes = Arc::new(WakeCount::default());
        let waker = futures::task::waker(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&waker);
        let (send, mut recv) = mesh::channel();
        let mut snapshot =
            MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
        snapshot.poll_device(&mut cx);
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[0]),
            IoResult::Defer(_)
        ));
        let first = recv.try_recv().unwrap();
        snapshot.poll_device(&mut cx);
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[1]),
            IoResult::Ok
        ));
        wakes.0.store(0, Ordering::Relaxed);
        first.release_write.send(());
        assert_ne!(wakes.0.load(Ordering::Relaxed), 0);

        snapshot.poll_device(&mut cx);
        assert!(matches!(
            snapshot.io_write(SNAPSHOT_PORT, &[1]),
            IoResult::Ok
        ));
        wakes.0.store(0, Ordering::Relaxed);
        first.transaction_complete.complete(());
        assert_ne!(wakes.0.load(Ordering::Relaxed), 0);
        snapshot.poll_device(&mut cx);
        assert!(snapshot.pending.is_none());
    }

    #[test]
    fn snapshot_request_stop_and_reset_cancel_unreleased_write() {
        for reset in [false, true] {
            let (send, mut recv) = mesh::channel();
            let mut snapshot =
                MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
            let mut cx = Context::from_waker(Waker::noop());
            let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
                panic!("snapshot write was not deferred");
            };
            let mut first = recv.try_recv().unwrap();
            if reset {
                futures::executor::block_on(snapshot.reset());
            } else {
                futures::executor::block_on(snapshot.stop());
            }
            assert!(snapshot.pending.is_none());
            assert!(matches!(
                deferred_write.poll_write(&mut cx),
                Poll::Ready(Err(IoError::InvalidRegister))
            ));
            assert!(matches!(
                Pin::new(&mut first.write_completed).poll(&mut cx),
                Poll::Ready(Ok(()))
            ));
            assert!(matches!(
                snapshot.io_write(SNAPSHOT_PORT, &[0]),
                IoResult::Defer(_)
            ));
            assert_eq!(
                recv.try_recv().unwrap().scratch_policy,
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            );
        }
    }

    #[test]
    fn snapshot_request_stop_and_reset_preserve_released_transaction() {
        for reset in [false, true] {
            let (send, mut recv) = mesh::channel();
            let mut snapshot =
                MicrovmSnapshotRequest::new(Some(send), std::time::Duration::from_secs(1));
            let mut cx = Context::from_waker(Waker::noop());
            let IoResult::Defer(mut deferred_write) = snapshot.io_write(SNAPSHOT_PORT, &[1]) else {
                panic!("snapshot write was not deferred");
            };
            let first = recv.try_recv().unwrap();
            first.release_write.send(());
            snapshot.poll_device(&mut cx);
            assert!(matches!(
                deferred_write.poll_write(&mut cx),
                Poll::Ready(Ok(()))
            ));
            if reset {
                futures::executor::block_on(snapshot.reset());
            } else {
                futures::executor::block_on(snapshot.stop());
            }
            assert!(snapshot.pending.is_some());
            snapshot.start();
            assert!(matches!(
                snapshot.io_write(SNAPSHOT_PORT, &[0]),
                IoResult::Ok
            ));
            assert!(recv.try_recv().is_err());
            first.transaction_complete.complete(());
            assert!(matches!(
                snapshot.io_write(SNAPSHOT_PORT, &[0]),
                IoResult::Defer(_)
            ));
            assert_eq!(
                recv.try_recv().unwrap().scratch_policy,
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            );
        }
    }
}
