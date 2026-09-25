// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Control-session broker worker mode.
//!
//! [`VirtioConsoleDevice::new_broker`] creates the control console. Its worker
//! runs the control-session broker between the guest queues and a
//! reconnectable host endpoint: it only attaches a host whose local peer
//! identity matches the configured one, bounds host authentication with a
//! timeout, and tracks the host endpoint's connection lifecycle in
//! [`HostTransportState`].

use crate::BUF_SIZE;
use crate::ConsoleWorker;
use crate::ConsoleWorkerState;
use crate::VirtioConsoleDevice;
use crate::WorkerError;
use crate::control_session_broker;
use crate::direct::ConsoleWorkerMode;
use crate::spec::VirtioConsoleConfig;
use guestmem::GuestMemory;
use inspect::Inspect;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use task_control::TaskControl;
use virtio_resources::console::control::VirtioControlConsoleBrokerConfig;
use vmcore::vm_task::VmTaskDriverSource;

impl VirtioConsoleDevice {
    /// Creates the control console with a reconnectable host endpoint and a
    /// VMM-resident session broker.
    pub fn new_broker(
        driver_source: &VmTaskDriverSource,
        host_io: Box<dyn SerialIo>,
        config: VirtioControlConsoleBrokerConfig,
    ) -> Self {
        let driver = driver_source.simple();
        let transport_state = initial_host_transport_state(&*host_io);
        let broker = control_session_broker::ControlSessionBroker::new(
            config.instance_id,
            config.capability,
        );
        let auth_timer = PolledTimer::new(&driver);
        let mut worker = TaskControl::new(ConsoleWorker {
            mode: ConsoleWorkerMode::Broker(Box::new(BrokerWorker {
                host_io,
                broker,
                config,
                transport_state,
                host_input: VecDeque::new(),
                auth_timer,
                auth_deadline: None,
            })),
        });
        worker.insert(
            &driver,
            "virtio-control-console",
            ConsoleWorkerState {
                receiveq: None,
                transmitq: None,
                mem: GuestMemory::empty(),
                partial_transmit: 0,
                staged_rx: VecDeque::new(),
                input_gated: false,
            },
        );
        Self {
            driver,
            config: VirtioConsoleConfig::default(),
            worker,
        }
    }
}

pub(crate) struct BrokerWorker {
    pub(crate) host_io: Box<dyn SerialIo>,
    pub(crate) broker: control_session_broker::ControlSessionBroker,
    pub(crate) config: VirtioControlConsoleBrokerConfig,
    pub(crate) transport_state: HostTransportState,
    pub(crate) host_input: VecDeque<u8>,
    auth_timer: PolledTimer,
    pub(crate) auth_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostTransportState {
    WaitingForDisconnect,
    WaitingForConnect,
    Connected,
    Disabled,
}

pub(crate) fn initial_host_transport_state(host_io: &dyn SerialIo) -> HostTransportState {
    if host_io.is_connected() {
        HostTransportState::Connected
    } else {
        HostTransportState::WaitingForConnect
    }
}

pub(crate) fn disconnect_host_transport(host_io: &mut dyn SerialIo) -> HostTransportState {
    if !host_io.is_connected() || host_io.disconnect_current().is_ok() {
        HostTransportState::WaitingForConnect
    } else {
        HostTransportState::WaitingForDisconnect
    }
}

impl Inspect for BrokerWorker {
    fn inspect(&self, req: inspect::Request<'_>) {
        let counters = self.broker.counters();
        req.respond()
            .field("mode", "broker")
            .field("broker_state", format!("{:?}", self.broker.state()))
            .field("epoch", self.broker.epoch())
            .field("host_transport", format!("{:?}", self.transport_state))
            .field("host_authenticated", self.broker.host_is_authenticated())
            .field(
                "guest_output_records",
                self.broker
                    .output_record_count(control_session_broker::OutputLegId::Guest),
            )
            .field(
                "guest_output_bytes",
                self.broker
                    .output_byte_count(control_session_broker::OutputLegId::Guest),
            )
            .field(
                "host_output_records",
                self.broker
                    .output_record_count(control_session_broker::OutputLegId::Host),
            )
            .field(
                "host_output_bytes",
                self.broker
                    .output_byte_count(control_session_broker::OutputLegId::Host),
            )
            .field(
                "guest_parser_bytes",
                self.broker.guest_parser_buffered_bytes(),
            )
            .field("guest_receive_window", self.broker.guest_receive_window())
            .field("guest_receive_credit", self.broker.guest_receive_credit())
            .field("host_input_bytes", self.host_input.len())
            .field("protocol_errors", counters.protocol_errors)
            .field("authentication_errors", counters.authentication_errors)
            .field("sequence_errors", counters.sequence_errors)
            .field("ack_errors", counters.ack_errors)
            .field("reset_errors", counters.reset_errors)
            .field("reconnect_errors", counters.reconnect_errors)
            .field("backpressure_errors", counters.backpressure_errors);
    }
}

impl BrokerWorker {
    /// Applies a guest-initiated device reset to the broker and host transport.
    pub(crate) fn reset_for_device(&mut self) {
        let preserve_unstarted_host = self.broker.state()
            == control_session_broker::BrokerState::AwaitGuestAttach
            && !self.broker.host_is_connected()
            && self.transport_state == HostTransportState::Connected;
        self.broker.reset_for_device();
        self.host_input.clear();
        self.auth_deadline = None;
        self.transport_state = if preserve_unstarted_host {
            if self.host_io.is_connected() {
                HostTransportState::Connected
            } else {
                HostTransportState::WaitingForDisconnect
            }
        } else {
            disconnect_host_transport(&mut *self.host_io)
        };
    }

    pub(crate) async fn run_loop(
        &mut self,
        state: &mut ConsoleWorkerState,
    ) -> Result<(), WorkerError> {
        if state.receiveq.is_none() && state.transmitq.is_none() {
            std::future::pending::<()>().await;
        }
        match poll_fn(|cx| self.poll_once(state, cx)).await {
            Ok(()) => Ok(()),
            Err(error) => {
                tracelimit::error_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "control-console worker faulted"
                );
                if let Err(detach_error) = self.detach_host() {
                    tracelimit::error_ratelimited!(
                        error = &detach_error as &dyn std::error::Error,
                        "control-console host detach failed after worker fault"
                    );
                }
                // Keep TaskControl restartable. Device reset clears the broker
                // and queue state before the worker is started again.
                std::future::pending().await
            }
        }
    }

    fn poll_once(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), WorkerError>> {
        let mut made_progress = false;

        match self.transport_state {
            HostTransportState::WaitingForDisconnect => match self.host_io.poll_disconnect(cx) {
                Poll::Ready(Ok(())) => {
                    self.detach_host()?;
                    self.transport_state = HostTransportState::WaitingForConnect;
                    made_progress = true;
                }
                Poll::Ready(Err(error)) => {
                    self.disable_host_transport(error)?;
                    made_progress = true;
                }
                Poll::Pending => {}
            },
            HostTransportState::WaitingForConnect => match self.host_io.poll_connect(cx) {
                Poll::Ready(Ok(())) => {
                    self.transport_state = HostTransportState::Connected;
                    self.begin_verified_host_attachment()?;
                    made_progress = true;
                }
                Poll::Ready(Err(error)) => {
                    self.disable_host_transport(error)?;
                    made_progress = true;
                }
                Poll::Pending => {}
            },
            HostTransportState::Connected => {
                if !self.broker.host_is_connected() {
                    self.begin_verified_host_attachment()?;
                    made_progress = true;
                }
                match self.host_io.poll_disconnect(cx) {
                    Poll::Ready(Ok(())) => {
                        self.detach_host()?;
                        made_progress = true;
                    }
                    Poll::Ready(Err(error)) => {
                        self.disable_host_transport(error)?;
                        made_progress = true;
                    }
                    Poll::Pending => {}
                }
            }
            HostTransportState::Disabled => {}
        }

        if self.transport_state == HostTransportState::Connected
            && self.broker.host_is_connected()
            && !self.broker.host_is_authenticated()
            && let Some(deadline) = self.auth_deadline
            && self.auth_timer.poll_until(cx, deadline).is_ready()
        {
            tracelimit::warn_ratelimited!("control-console host authentication timed out");
            self.detach_host()?;
            made_progress = true;
        }

        made_progress |= self.poll_guest_output(state, cx)?;
        made_progress |= self.poll_guest_input(state, cx)?;

        if self.transport_state == HostTransportState::Connected {
            made_progress |= self.poll_host_output(cx)?;
            if self.transport_state == HostTransportState::Connected && !state.input_gated {
                made_progress |= self.poll_host_input(cx)?;
            }
            if self.broker.host_is_authenticated() {
                self.auth_deadline = None;
            }
        }

        if made_progress {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    fn poll_guest_output(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Result<bool, WorkerError> {
        let Some(receiveq) = state.receiveq.as_mut() else {
            return Ok(false);
        };
        if !self
            .broker
            .begin_output(control_session_broker::OutputLegId::Guest)
        {
            return Ok(false);
        }

        let work = match receiveq.try_peek().map_err(WorkerError::Virtio)? {
            Some(work) => work,
            None => {
                return Ok(receiveq.poll_kick(cx).is_ready());
            }
        };
        let writeable_len = work
            .payload()
            .iter()
            .filter(|payload| payload.writeable)
            .map(|payload| payload.length as usize)
            .sum::<usize>();
        if writeable_len == 0 {
            let work = work.consume();
            receiveq.complete(work, 0);
            return Ok(true);
        }

        let mut bytes = [0; BUF_SIZE];
        let count = {
            let output = self
                .broker
                .peek_output(
                    control_session_broker::OutputLegId::Guest,
                    writeable_len.min(BUF_SIZE),
                )
                .ok_or(control_session_broker::BrokerError::InvalidOutputProgress)
                .map_err(WorkerError::Broker)?;
            bytes[..output.len()].copy_from_slice(output);
            output.len()
        };
        let work = work.consume();
        if let Err(error) = work.write(&state.mem, &bytes[..count]) {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "failed to write broker output to guest receive buffer"
            );
            receiveq.complete(work, 0);
        } else {
            self.broker
                .advance_output(control_session_broker::OutputLegId::Guest, count)
                .map_err(WorkerError::Broker)?;
            receiveq.complete(work, count as u32);
        }
        Ok(true)
    }

    fn poll_guest_input(
        &mut self,
        state: &mut ConsoleWorkerState,
        cx: &mut Context<'_>,
    ) -> Result<bool, WorkerError> {
        if self.broker.has_pending_guest_record() {
            let progress = self
                .broker
                .accept_guest_input(&[])
                .map_err(WorkerError::Broker)?;
            if progress.status != control_session_broker::InputStatus::Backpressured {
                return Ok(true);
            }
        }

        let Some(transmitq) = state.transmitq.as_mut() else {
            return Ok(false);
        };
        let work = match transmitq.try_peek().map_err(WorkerError::Virtio)? {
            Some(work) => work,
            None => {
                return Ok(transmitq.poll_kick(cx).is_ready());
            }
        };
        let readable_len = work.readable_length() as usize;
        if state.partial_transmit >= readable_len {
            state.partial_transmit = 0;
            let work = work.consume();
            transmitq.complete(work, 0);
            return Ok(true);
        }

        let mut bytes = [0; BUF_SIZE];
        let requested = (readable_len - state.partial_transmit).min(BUF_SIZE);
        let read = work
            .read_at_offset(
                state.partial_transmit as u64,
                &state.mem,
                &mut bytes[..requested],
            )
            .map_err(WorkerError::GuestMemory)?;
        if read == 0 {
            return Ok(false);
        }

        let mut offset = 0;
        while offset < read {
            let progress = self
                .broker
                .accept_guest_input(&bytes[offset..read])
                .map_err(WorkerError::Broker)?;
            offset += progress.consumed;
            state.partial_transmit += progress.consumed;
            if progress.status == control_session_broker::InputStatus::Backpressured
                || progress.consumed == 0
            {
                break;
            }
        }
        if state.partial_transmit == readable_len {
            state.partial_transmit = 0;
            let work = work.consume();
            transmitq.complete(work, 0);
        }
        Ok(offset != 0)
    }

    fn poll_host_output(&mut self, cx: &mut Context<'_>) -> Result<bool, WorkerError> {
        if !self
            .broker
            .begin_output(control_session_broker::OutputLegId::Host)
        {
            return Ok(false);
        }
        let mut bytes = [0; BUF_SIZE];
        let count = {
            let output = self
                .broker
                .peek_output(control_session_broker::OutputLegId::Host, BUF_SIZE)
                .ok_or(control_session_broker::BrokerError::InvalidOutputProgress)
                .map_err(WorkerError::Broker)?;
            bytes[..output.len()].copy_from_slice(output);
            output.len()
        };
        match Pin::new(&mut *self.host_io).poll_write(cx, &bytes[..count]) {
            Poll::Ready(Ok(0)) => {
                tracelimit::warn_ratelimited!(
                    broker_state = ?self.broker.state(),
                    "control-console host write returned EOF"
                );
                self.detach_host()?;
                Ok(true)
            }
            Poll::Ready(Ok(written)) => {
                self.broker
                    .advance_output(control_session_broker::OutputLegId::Host, written)
                    .map_err(WorkerError::Broker)?;
                Ok(true)
            }
            Poll::Ready(Err(error)) => {
                tracelimit::error_ratelimited!(
                    broker_state = ?self.broker.state(),
                    error = &error as &dyn std::error::Error,
                    "control-console host write failed"
                );
                self.detach_host()?;
                Ok(true)
            }
            Poll::Pending => Ok(false),
        }
    }

    fn poll_host_input(&mut self, cx: &mut Context<'_>) -> Result<bool, WorkerError> {
        if self.broker.has_pending_host_record() || !self.host_input.is_empty() {
            let progress = if self.broker.has_pending_host_record() {
                self.broker.accept_host_input(&[])
            } else {
                let input = self.host_input.make_contiguous();
                self.broker.accept_host_input(input)
            };
            match progress {
                Ok(progress) => {
                    self.host_input.drain(..progress.consumed);
                    return Ok(progress.consumed != 0
                        || progress.status != control_session_broker::InputStatus::Backpressured);
                }
                Err(error) => {
                    tracelimit::warn_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "control-console host protocol input rejected"
                    );
                    self.detach_host()?;
                    return Ok(true);
                }
            }
        }

        let mut bytes = [0; BUF_SIZE];
        match Pin::new(&mut *self.host_io).poll_read(cx, &mut bytes) {
            Poll::Ready(Ok(0)) => {
                tracelimit::warn_ratelimited!(
                    broker_state = ?self.broker.state(),
                    "control-console host read returned EOF"
                );
                self.detach_host()?;
                Ok(true)
            }
            Poll::Ready(Ok(read)) => {
                self.host_input.extend(&bytes[..read]);
                Ok(true)
            }
            Poll::Ready(Err(error)) => {
                tracelimit::error_ratelimited!(
                    broker_state = ?self.broker.state(),
                    error = &error as &dyn std::error::Error,
                    "control-console host read failed"
                );
                self.detach_host()?;
                Ok(true)
            }
            Poll::Pending => Ok(false),
        }
    }

    fn detach_host(&mut self) -> Result<(), WorkerError> {
        let broker_result = self.broker.host_disconnected().map_err(WorkerError::Broker);
        self.host_input.clear();
        self.auth_deadline = None;
        self.transport_state = disconnect_host_transport(&mut *self.host_io);
        broker_result
    }

    fn begin_verified_host_attachment(&mut self) -> Result<(), WorkerError> {
        let identity = self.host_io.local_peer_identity();
        let identity_status = match &identity {
            Ok(Some(identity)) if identity == &self.config.expected_peer_identity => "expected",
            Ok(Some(serial_core::LocalPeerIdentity::Unsupported)) => "unsupported",
            Ok(Some(_)) => "unexpected",
            Ok(None) => "missing",
            Err(_) => "error",
        };
        let identity_error_kind = identity.as_ref().err().map(std::io::Error::kind);
        if !matches!(
            identity,
            Ok(Some(ref identity)) if identity == &self.config.expected_peer_identity
        ) {
            tracelimit::warn_ratelimited!(
                broker_state = ?self.broker.state(),
                identity_status,
                identity_error_kind = ?identity_error_kind,
                "control-console host rejected because its local peer identity is unavailable or unexpected"
            );
            self.detach_host()?;
            return Ok(());
        }
        match self.broker.begin_host_attachment() {
            Ok(()) => {
                self.auth_deadline = Some(
                    Instant::now()
                        .saturating_add(Duration::from_millis(self.config.auth_timeout_ms)),
                );
            }
            Err(error) => {
                tracelimit::warn_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "control-console host attachment rejected"
                );
                self.auth_deadline = None;
                self.detach_host()?;
            }
        }
        Ok(())
    }

    fn disable_host_transport(&mut self, error: std::io::Error) -> Result<(), WorkerError> {
        tracelimit::error_ratelimited!(
            error = &error as &dyn std::error::Error,
            "control-console host transport disabled after a lifecycle error"
        );
        self.detach_host()?;
        self.transport_state = HostTransportState::Disabled;
        Ok(())
    }
}
