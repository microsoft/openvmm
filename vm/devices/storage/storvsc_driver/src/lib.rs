// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Storvsc driver for use as a disk backend.

#[cfg(feature = "test")]
pub mod test_helpers;

#[cfg(not(feature = "test"))]
mod test_helpers;

use crate::save_restore::StorvscDriverSavedState;
use futures::FutureExt;
use futures::lock::Mutex;
use futures_concurrency::future::Race;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use mesh_channel::Receiver;
use mesh_channel::RecvError;
use mesh_channel::Sender;
use slab::Slab;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use tracing_helpers::ErrorValueExt;
use user_driver::DmaClient;
use user_driver::memory::MemoryBlock;
use vmbus_async::queue;
use vmbus_async::queue::CompletionPacket;
use vmbus_async::queue::DataPacket;
use vmbus_async::queue::ExternalDataError;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::PacketRef;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::PAGE_SIZE;
use vmbus_ring::RingMem;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Storvsc to provide a backend for SCSI devices over VMBus.
#[derive(Inspect)]
pub struct StorvscDriver<T: Send + Sync + RingMem> {
    #[inspect(skip)] // TODO: See how to inspect this
    storvsc: Mutex<TaskControl<StorvscState, Storvsc<T>>>,
    #[inspect(skip)]
    new_request_sender: Option<Sender<StorvscRequest>>,
    #[inspect(skip)]
    dma_client: Arc<dyn DmaClient>,
}

/// Storvsc backend for SCSI devices.
struct Storvsc<T: Send + Sync + RingMem> {
    pub(crate) inner: StorvscInner,
    version: storvsp_protocol::ProtocolVersion,
    queue: Queue<T>,
    pub(crate) num_sub_channels: Option<u16>,
    has_negotiated: bool,
}

struct StorvscInner {
    new_request_receiver: Receiver<StorvscRequest>,
    transactions: Slab<PendingOperation>,
}

struct StorvscRequest {
    request: storvsp_protocol::ScsiRequest,
    buf_gpa: u64,
    byte_len: usize,
    completion_sender: Sender<StorvscCompletion>,
}

/// Indicates the reason a storvsc operation was completed.
#[derive(Clone)]
pub enum StorvscCompleteReason {
    /// Completion received.
    CompletionReceived,
    /// Cancelled due to shutdown.
    Shutdown,
    /// Cancelled due to save/restore.
    SaveRestore,
}

/// Result of a Storvsc operation. If None, then operation was cancelled.
pub struct StorvscCompletion {
    reason: StorvscCompleteReason,
    completion: Option<storvsp_protocol::ScsiRequest>,
}

struct PendingOperation {
    sender: Sender<StorvscCompletion>,
}

impl PendingOperation {
    fn new(sender: Sender<StorvscCompletion>) -> Self {
        Self { sender }
    }

    fn complete(&mut self, result: storvsp_protocol::ScsiRequest) {
        self.sender.send(StorvscCompletion {
            reason: StorvscCompleteReason::CompletionReceived,
            completion: Some(result),
        })
    }

    fn cancel(&mut self, reason: StorvscCompleteReason) {
        // Sending completion with an empty result indicates cancellation or other error.
        self.sender.send(StorvscCompletion {
            reason,
            completion: None,
        });
    }
}

/// Errors resulting from storvsc.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct StorvscError(StorvscErrorInner);

/// The kind of storvsc error as visible from components sending requests.
#[derive(Debug)]
#[non_exhaustive]
pub enum StorvscErrorKind {
    /// Error waiting for completion of operation.
    CompletionError,
    /// Pending operation cancelled.
    Cancelled,
    /// Pending operation cancelled, but can be retried.
    CancelledRetry,
    /// Another error kind not covered by the above.
    Other,
}

impl StorvscError {
    /// Returns the kind of storvsc error that occurred.
    pub fn kind(&self) -> StorvscErrorKind {
        match self.0 {
            StorvscErrorInner::CompletionError(_) => StorvscErrorKind::CompletionError,
            StorvscErrorInner::Cancelled => StorvscErrorKind::Cancelled,
            StorvscErrorInner::CancelledRetry => StorvscErrorKind::CancelledRetry,
            _ => StorvscErrorKind::Other,
        }
    }
}

/// Inner errors from storvsc.
#[derive(Debug, Error)]
pub(crate) enum StorvscErrorInner {
    /// Packet error.
    #[error("packet error")]
    PacketError(#[source] PacketError),
    /// Queue error.
    #[error("queue error")]
    Queue(#[source] queue::Error),
    /// Queue out of space.
    #[error("queue should have enough space but no longer does")]
    NotEnoughSpace,
    /// Unsupported protocol version.
    #[error("requested protocol version unsupported by storvsp")]
    UnsupportedProtocolVersion,
    /// Unexpected protocol data or operation.
    #[error("unexpected protocol data or operation")]
    UnexpectedOperation,
    /// Error while decoding received packet.
    #[error("error decoding received packet")]
    DecodeError,
    /// Error sending request to storvsc driver.
    #[error("error sending request to storvsc")]
    RequestError,
    /// Error waiting for completion of operation.
    #[error("error waiting for completion of operation")]
    CompletionError(#[source] RecvError),
    /// Operation cancelled.
    #[error("pending operation cancelled")]
    Cancelled,
    /// Operation cancelled, but can be retried.
    #[error("pending operation cancelled, but can be retried")]
    CancelledRetry,
    /// Storvsc driver not fully initialized.
    #[error("driver not initialized")]
    Uninitialized,
}

/// Errors with packet parsing between storvsc and storvsp.
#[derive(Debug, Error)]
pub(crate) enum PacketError {
    /// Not transactional.
    #[error("Not transactional")]
    NotTransactional,
    /// Unenxpected transaction.
    #[error("Unexpected transaction {0:?}")]
    UnexpectedTransaction(u64),
    /// Unexpected status.
    #[error("Unexpected status {0:?}")]
    UnexpectedStatus(storvsp_protocol::NtStatus),
    /// Unrecognzied operation.
    #[error("Unrecognized operation {0:?}")]
    UnrecognizedOperation(storvsp_protocol::Operation),
    /// Invalid packet type.
    #[error("Invalid packet type")]
    InvalidPacketType,
    /// Invalid data transfer length.
    #[error("Invalid data transfer length")]
    InvalidDataTransferLength,
    /// Access error.
    #[error("Access error")]
    Access(#[source] AccessError),
    /// Range error.
    #[error("Range error")]
    Range(#[source] ExternalDataError),
}

impl<T: 'static + Send + Sync + RingMem> StorvscDriver<T> {
    /// Create a new driver instance connected to storvsp over VMBus.
    pub fn new(dma_client: Arc<dyn DmaClient>) -> Self {
        Self {
            storvsc: Mutex::new(TaskControl::new(StorvscState)),
            new_request_sender: None,
            dma_client,
        }
    }

    /// Start Storvsc.
    pub async fn run(
        &mut self,
        driver_source: &VmTaskDriverSource,
        channel: RawAsyncChannel<T>,
        version: storvsp_protocol::ProtocolVersion,
        target_vp: u32,
    ) -> Result<(), StorvscError> {
        let driver = driver_source
            .builder()
            .target_vp(target_vp)
            .run_on_target(true)
            .build("storvsc");
        let (new_request_sender, new_request_receiver) = mesh_channel::channel::<StorvscRequest>();
        let mut storvsc = Storvsc::new(channel, version, new_request_receiver)?;
        storvsc.negotiate().await.unwrap();
        self.new_request_sender = Some(new_request_sender);

        {
            let mut s = self.storvsc.lock().await;
            s.insert(&driver, "storvsc", storvsc);
            s.start();
        }
        Ok(())
    }

    /// Stop Storvsc.
    pub async fn stop(&self) {
        let mut s = self.storvsc.lock().await;
        s.stop().await;
        s.remove();
    }

    /// Saves the current state during servicing.
    pub async fn save(&self) -> Result<StorvscDriverSavedState, StorvscError> {
        let mut s = self.storvsc.lock().await;
        if s.stop().await {
            let state = s.state_mut().unwrap();

            // Cancel pending operations with save/restore reason.
            for mut transaction in state.inner.transactions.drain() {
                transaction.cancel(StorvscCompleteReason::SaveRestore);
            }

            Ok(StorvscDriverSavedState {
                version: state.version.major_minor,
                num_sub_channels: state.num_sub_channels,
                has_negotiated: state.has_negotiated,
            })
        } else {
            // Task was not running, so not state to save
            Ok(StorvscDriverSavedState {
                version: 0,
                num_sub_channels: None,
                has_negotiated: false,
            })
        }
    }

    /// Restore the state during servicing.
    pub async fn restore(
        state: &StorvscDriverSavedState,
        driver_source: &VmTaskDriverSource,
        channel: RawAsyncChannel<T>,
        target_vp: u32,
        dma_client: Arc<dyn DmaClient>,
    ) -> Result<Self, StorvscError> {
        let driver = driver_source
            .builder()
            .target_vp(target_vp)
            .run_on_target(true)
            .build("storvsc");
        let (new_request_sender, new_request_receiver) = mesh_channel::channel::<StorvscRequest>();
        let storvsc = Storvsc::new(
            channel,
            storvsp_protocol::ProtocolVersion {
                major_minor: state.version,
                reserved: 0,
            },
            new_request_receiver,
        )?;
        let storvsc_driver = Self {
            storvsc: Mutex::new(TaskControl::new(StorvscState)),
            new_request_sender: Some(new_request_sender),
            dma_client,
        };

        {
            let mut s = storvsc_driver.storvsc.lock().await;
            s.insert(&driver, "storvsc", storvsc);
            s.start();
        }

        Ok(storvsc_driver)
    }

    /// Send a SCSI request to storvsp over VMBus.
    pub async fn send_request(
        &self,
        request: &storvsp_protocol::ScsiRequest,
        buf_gpa: u64,
        byte_len: usize,
    ) -> Result<storvsp_protocol::ScsiRequest, StorvscError> {
        let (sender, mut receiver) = mesh_channel::channel::<StorvscCompletion>();
        let storvsc_request = StorvscRequest {
            request: *request,
            buf_gpa,
            byte_len,
            completion_sender: sender,
        };
        match &self.new_request_sender {
            Some(request_sender) => {
                request_sender.send(storvsc_request);
                Ok(())
            }
            None => Err(StorvscError(StorvscErrorInner::Uninitialized)),
        }?;

        let resp = receiver
            .recv()
            .await
            .map_err(|err| StorvscError(StorvscErrorInner::CompletionError(err)))?;

        match resp.reason {
            StorvscCompleteReason::CompletionReceived => match resp.completion {
                Some(completion) => Ok(completion),
                None => Err(StorvscError(StorvscErrorInner::Cancelled)),
            },
            StorvscCompleteReason::Shutdown => Err(StorvscError(StorvscErrorInner::Cancelled)),
            StorvscCompleteReason::SaveRestore => {
                Err(StorvscError(StorvscErrorInner::CancelledRetry))
            }
        }
    }

    /// Allocates a DMA buffer for use by clients to this driver.
    pub fn allocate_dma_buffer(&self, size: usize) -> Result<MemoryBlock, anyhow::Error> {
        self.dma_client.allocate_dma_buffer(size)
    }
}

struct StorvscState;

impl<T: 'static + Send + Sync + RingMem> AsyncRun<Storvsc<T>> for StorvscState {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        worker: &mut Storvsc<T>,
    ) -> Result<(), task_control::Cancelled> {
        let fut = async {
            if !worker.has_negotiated {
                worker.negotiate().await?;
            }
            worker.process_main().await
        };

        match stop.until_stopped(fut).await? {
            Ok(_) => {}
            Err(err) => tracing::error!(error = err.as_error(), "storvsc run error"),
        }
        Ok(())
    }
}

impl<T: 'static + Send + Sync + RingMem> InspectTask<Storvsc<T>> for StorvscState {
    fn inspect(&self, req: inspect::Request<'_>, worker: Option<&Storvsc<T>>) {
        if let Some(worker) = worker {
            let mut resp = req.respond();
            resp.field("has_negotiated", worker.has_negotiated);
        }
    }
}

impl<T: 'static + Send + Sync + RingMem> Storvsc<T> {
    pub(crate) fn new(
        channel: RawAsyncChannel<T>,
        version: storvsp_protocol::ProtocolVersion,
        new_request_receiver: Receiver<StorvscRequest>,
    ) -> Result<Self, StorvscError> {
        let queue =
            Queue::new(channel).map_err(|err| StorvscError(StorvscErrorInner::Queue(err)))?;

        Ok(Self {
            inner: StorvscInner {
                new_request_receiver,
                transactions: Slab::new(),
            },
            version,
            queue,
            num_sub_channels: None,
            has_negotiated: false,
        })
    }
}

impl<T: Send + Sync + RingMem> Storvsc<T> {
    async fn negotiate(&mut self) -> Result<(), StorvscError> {
        // Negotiate protocol with storvsp instance on the other end of VMBus
        // Step 1: BEGIN_INITIALIZATION
        self.inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::BEGIN_INITIALIZATION,
                1,
                &(),
            )
            .await?;

        // Step 2: QUERY_PROTOCOL_VERSION - request latest version
        self.inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION,
                2,
                &self.version,
            )
            .await
            .map_err(|err| match err {
                StorvscError(StorvscErrorInner::PacketError(PacketError::UnexpectedStatus(
                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                ))) => StorvscError(StorvscErrorInner::UnsupportedProtocolVersion),
                _ => err,
            })?;

        // Step 3: QUERY_PROPERTIES
        let properties_packet = self
            .inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::QUERY_PROPERTIES,
                3,
                &(),
            )
            .await?;
        let _properties = storvsp_protocol::ChannelProperties::ref_from_prefix(
            &properties_packet.data[0..properties_packet.data_size],
        )
        .map_err(|_err| StorvscError(StorvscErrorInner::UnexpectedOperation))?
        .0
        .to_owned();

        // Skip subchannels because unsupported at the moment

        // Step 4: END_INITIALIZATION
        self.inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::END_INITIALIZATION,
                4,
                &(),
            )
            .await?;

        self.has_negotiated = true;

        tracing::info!(
            version = self.version.major_minor,
            num_sub_channels = self.num_sub_channels,
            "Negotiated protocol"
        );

        Ok(())
    }

    /// Main loop to poll for and handle new operations and incoming completions for operations
    async fn process_main(&mut self) -> Result<(), StorvscError> {
        match self.inner.process_main(&mut self.queue).await {
            Ok(_) => Ok(()),
            Err(StorvscError(StorvscErrorInner::Queue(err2))) => {
                if err2.is_closed_error() {
                    // This is expected, cancel any pending completions
                    self.inner
                        .cancel_pending_completions(StorvscCompleteReason::Shutdown)
                        .await;
                    Ok(())
                } else {
                    Err(StorvscError(StorvscErrorInner::Queue(err2)))
                }
            }
            Err(err) => Err(err),
        }
    }
}

impl StorvscInner {
    async fn process_main<M: RingMem>(&mut self, queue: &mut Queue<M>) -> Result<(), StorvscError> {
        loop {
            enum Event<'a, M: RingMem> {
                NewRequestReceived(Result<StorvscRequest, RecvError>),
                VmbusPacketReceived(Result<PacketRef<'a, M>, queue::Error>),
            }
            let (mut reader, mut writer) = queue.split();
            match (
                self.new_request_receiver
                    .recv()
                    .map(Event::NewRequestReceived),
                reader.read().map(Event::VmbusPacketReceived),
            )
                .race()
                .await
            {
                Event::NewRequestReceived(result) => match result {
                    Ok(request) => {
                        match self.send_request(
                            &request.request,
                            request.buf_gpa,
                            request.byte_len,
                            &mut writer,
                            request.completion_sender,
                        ) {
                            Ok(()) => Ok(()),
                            Err(err) => {
                                tracing::error!(
                                    "Unable to send new request to VMBus, err={:?}",
                                    err
                                );
                                Err(err)
                            }
                        }
                    }
                    Err(err) => {
                        tracing::error!("Unable to receive new request, err={:?}", err);
                        Err(StorvscError(StorvscErrorInner::RequestError))
                    }
                },
                Event::VmbusPacketReceived(result) => match result {
                    Ok(packet_ref) => self.handle_packet(packet_ref.as_ref()),
                    Err(err) => {
                        tracing::error!("Error receiving VMBus packet, err={:?}", err);
                        Err(StorvscError(StorvscErrorInner::Queue(err)))
                    }
                },
            }?;
        }
    }

    fn send_request<M: RingMem>(
        &mut self,
        request: &storvsp_protocol::ScsiRequest,
        buf_gpa: u64,
        byte_len: usize,
        writer: &mut queue::WriteHalf<'_, M>,
        completion_sender: Sender<StorvscCompletion>,
    ) -> Result<(), StorvscError> {
        // Create pending transaction record
        let transaction_id = self
            .transactions
            .insert(PendingOperation::new(completion_sender));

        self.send_gpa_direct_packet(
            writer,
            storvsp_protocol::Operation::EXECUTE_SRB,
            storvsp_protocol::NtStatus::SUCCESS,
            transaction_id as u64,
            request,
            buf_gpa,
            byte_len,
        )
    }

    async fn cancel_pending_completions(&mut self, reason: StorvscCompleteReason) {
        for transaction in self.transactions.iter_mut() {
            transaction.1.cancel(reason.clone());
        }
        self.transactions.clear();
    }

    fn handle_packet<M: RingMem>(
        &mut self,
        packet: &IncomingPacket<'_, M>,
    ) -> Result<(), StorvscError> {
        let packet = parse_packet(packet)?;
        match packet {
            Packet::Data(data) => {
                match data.operation {
                    storvsp_protocol::Operation::ENUMERATE_BUS => {
                        // Nothing to do here, and no completion is required.
                        // This may be needed in the future for hot add.
                        Ok(())
                    }
                    _ => Err(StorvscError(StorvscErrorInner::UnexpectedOperation)),
                }
            }
            Packet::Completion(completion) => {
                // Parse ScsiRequest (contains response) from bytes
                let result =
                    storvsp_protocol::ScsiRequest::ref_from_bytes(completion.data.as_slice())
                        .map_err(|_err| StorvscError(StorvscErrorInner::DecodeError))?
                        .to_owned();

                // Match completion against pending transactions
                match self
                    .transactions
                    .get_mut(completion.transaction_id as usize)
                {
                    Some(t) => Ok(t),
                    None => Err(StorvscError(StorvscErrorInner::PacketError(
                        PacketError::UnexpectedTransaction(completion.transaction_id),
                    ))),
                }?
                .complete(result);

                Ok(())
            }
        }
    }

    /// Awaits the next incoming packet. Increments the count of outstanding packets when returning `Ok(Packet)`.
    async fn next_packet<'a, M: RingMem>(
        &mut self,
        reader: &'a mut queue::ReadHalf<'a, M>,
    ) -> Result<Packet, StorvscError> {
        let packet = reader
            .read()
            .await
            .map_err(|err| StorvscError(StorvscErrorInner::Queue(err)))?;
        parse_packet(&packet)
    }

    /// Send a non-GPA Direct packet over VMBus.
    fn send_packet<M: RingMem, P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        transaction_id: u64,
        payload: &P,
    ) -> Result<(), StorvscError> {
        let payload_bytes = payload.as_bytes();
        self.send_vmbus_packet(
            &mut writer.batched(),
            OutgoingPacketType::InBandWithCompletion,
            transaction_id,
            operation,
            status,
            payload_bytes,
        )?;
        Ok(())
    }

    /// Send a GPA Direct packet over VMBus.
    fn send_gpa_direct_packet<M: RingMem, P: IntoBytes + Immutable + KnownLayout>(
        &mut self,
        writer: &mut queue::WriteHalf<'_, M>,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        transaction_id: u64,
        payload: &P,
        gpa_start: u64,
        byte_len: usize,
    ) -> Result<(), StorvscError> {
        let payload_bytes = payload.as_bytes();
        let start_page: u64 = gpa_start / PAGE_SIZE as u64;
        let end_page: u64 = (gpa_start + (byte_len + PAGE_SIZE - 1) as u64) / PAGE_SIZE as u64;
        let gpas: Vec<u64> = (start_page..end_page).collect();
        let pages =
            PagedRange::new(gpa_start as usize % PAGE_SIZE, byte_len, gpas.as_slice()).unwrap();
        self.send_vmbus_packet(
            &mut writer.batched(),
            OutgoingPacketType::GpaDirect(&[pages]),
            transaction_id,
            operation,
            status,
            payload_bytes,
        )?;
        Ok(())
    }

    /// Send a VMBus packet.
    fn send_vmbus_packet<M: RingMem>(
        &mut self,
        writer: &mut queue::WriteBatch<'_, M>,
        packet_type: OutgoingPacketType<'_>,
        transaction_id: u64,
        operation: storvsp_protocol::Operation,
        status: storvsp_protocol::NtStatus,
        payload: &[u8],
    ) -> Result<(), StorvscError> {
        let header = storvsp_protocol::Packet {
            operation,
            flags: 0,
            status,
        };

        // storvsp limits the size of the completion packet to the size of the request packet,
        // so we need to pad the payload to the maximum size to ensure we get a complete response.
        let padding = [0; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
        let padding_bytes = if size_of_val(payload) < storvsp_protocol::SCSI_REQUEST_LEN_MAX {
            &padding[..storvsp_protocol::SCSI_REQUEST_LEN_MAX - size_of_val(payload)]
        } else {
            &[][..]
        };
        assert_eq!(
            size_of_val(payload) + padding_bytes.len(),
            storvsp_protocol::SCSI_REQUEST_LEN_MAX
        );
        writer
            .try_write(&OutgoingPacket {
                transaction_id,
                packet_type,
                payload: &[header.as_bytes(), payload, padding_bytes],
            })
            .map_err(|err| match err {
                queue::TryWriteError::Full(_) => StorvscError(StorvscErrorInner::NotEnoughSpace),
                queue::TryWriteError::Queue(err) => StorvscError(StorvscErrorInner::Queue(err)),
            })
    }

    async fn send_packet_and_expect_completion<
        M: RingMem,
        P: IntoBytes + Immutable + KnownLayout,
    >(
        &mut self,
        queue: &mut Queue<M>,
        operation: storvsp_protocol::Operation,
        transaction_id: u64,
        payload: &P,
    ) -> Result<StorvscCompletionPacket, StorvscError> {
        let (mut reader, mut writer) = queue.split();
        self.send_packet(
            &mut writer,
            operation,
            storvsp_protocol::NtStatus::SUCCESS,
            transaction_id,
            payload,
        )?;
        // Wait for completion
        let completion = match self.next_packet(&mut reader).await? {
            Packet::Completion(packet) => Ok(packet),
            Packet::Data(_) => Err(StorvscError(StorvscErrorInner::PacketError(
                PacketError::InvalidPacketType,
            ))),
        }?;

        // Expect matching transaction ID
        if completion.transaction_id != transaction_id {
            return Err(StorvscError(StorvscErrorInner::PacketError(
                PacketError::UnexpectedTransaction(completion.transaction_id),
            )));
        }

        // Expect success
        if completion.status != storvsp_protocol::NtStatus::SUCCESS {
            return Err(StorvscError(StorvscErrorInner::PacketError(
                PacketError::UnexpectedStatus(completion.status),
            )));
        }

        Ok(completion)
    }
}

enum Packet {
    Completion(StorvscCompletionPacket),
    Data(StorvscDataPacket),
}

#[derive(Debug)]
struct StorvscCompletionPacket {
    transaction_id: u64,
    status: storvsp_protocol::NtStatus,
    data_size: usize,
    data: [u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX],
}

#[derive(Debug)]
#[expect(dead_code)]
struct StorvscDataPacket {
    transaction_id: Option<u64>,
    request_size: usize,
    operation: storvsp_protocol::Operation,
    flags: u32,
    status: storvsp_protocol::NtStatus,
    data: [u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX],
}

fn parse_packet<T: RingMem>(packet: &IncomingPacket<'_, T>) -> Result<Packet, StorvscError> {
    match packet {
        IncomingPacket::Completion(completion) => parse_completion(completion)
            .map_err(|err| StorvscError(StorvscErrorInner::PacketError(err))),
        IncomingPacket::Data(data) => {
            parse_data(data).map_err(|err| StorvscError(StorvscErrorInner::PacketError(err)))
        }
    }
}

fn parse_completion<T: RingMem>(packet: &CompletionPacket<'_, T>) -> Result<Packet, PacketError> {
    let mut reader = packet.reader();
    let header: storvsp_protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
    if header.operation != storvsp_protocol::Operation::COMPLETE_IO {
        return Err(PacketError::NotTransactional);
    }
    let data_size = reader.len();
    let mut data = [0_u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
    let data_temp: Vec<u8> = reader.read_n(data_size).map_err(PacketError::Access)?;
    data[..data_size].clone_from_slice(data_temp.as_slice());
    Ok(Packet::Completion(StorvscCompletionPacket {
        transaction_id: packet.transaction_id(),
        status: header.status,
        data_size,
        data,
    }))
}

fn parse_data<T: RingMem>(packet: &DataPacket<'_, T>) -> Result<Packet, PacketError> {
    let transaction_id = packet.transaction_id();

    let mut reader = packet.reader();
    let header: storvsp_protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
    // You would expect that this should be limited to the current protocol
    // version's maximum packet size, but this is not what Hyper-V does, and
    // Linux 6.1 relies on this behavior during protocol initialization.
    let request_size = reader.len().min(storvsp_protocol::SCSI_REQUEST_LEN_MAX);
    let operation = header.operation;
    let flags = header.flags;
    let status = header.status;

    let mut data = [0_u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
    reader.read(&mut data).map_err(PacketError::Access)?;

    Ok(Packet::Data(StorvscDataPacket {
        transaction_id,
        request_size,
        operation,
        flags,
        status,
        data,
    }))
}

/// Save/restore states for storvsc driver and associated components.
pub mod save_restore {
    use mesh::payload::Protobuf;

    /// Save/restore state for storvsc driver.
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "storvsc_driver")]
    pub struct StorvscDriverSavedState {
        /// Protocol version (major_minor).
        #[mesh(1)]
        pub version: u16,
        /// Number of sub channels.
        #[mesh(2)]
        pub num_sub_channels: Option<u16>,
        /// Whether negotiation has completed.
        #[mesh(3)]
        pub has_negotiated: bool,
    }
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::TestStorvscWorker;
    use crate::test_helpers::TestStorvspWorker;
    use guestmem::GuestMemory;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use scsi_defs::ScsiOp;
    use test_with_tracing::test;
    use vmbus_async::queue::Queue;
    use vmbus_channel::connected_async_channels;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    // This function assumes the sector size is 512.
    fn generate_write_packet(
        target_id: u8,
        path_id: u8,
        lun: u8,
        block: u32,
        byte_len: usize,
    ) -> storvsp_protocol::ScsiRequest {
        let cdb = scsi_defs::Cdb10 {
            operation_code: ScsiOp::WRITE,
            logical_block: block.into(),
            transfer_blocks: ((byte_len / 512) as u16).into(),
            ..FromZeros::new_zeroed()
        };

        let mut scsi_req = storvsp_protocol::ScsiRequest {
            target_id,
            path_id,
            lun,
            length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: size_of::<scsi_defs::Cdb10>() as u8,
            data_transfer_length: byte_len as u32,
            ..FromZeros::new_zeroed()
        };

        scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());
        scsi_req
    }

    // This function assumes the sector size is 512.
    fn generate_read_packet(
        target_id: u8,
        path_id: u8,
        lun: u8,
        block: u32,
        byte_len: usize,
    ) -> storvsp_protocol::ScsiRequest {
        let cdb = scsi_defs::Cdb10 {
            operation_code: ScsiOp::READ,
            logical_block: block.into(),
            transfer_blocks: ((byte_len / 512) as u16).into(),
            ..FromZeros::new_zeroed()
        };

        let mut scsi_req = storvsp_protocol::ScsiRequest {
            target_id,
            path_id,
            lun,
            length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: size_of::<scsi_defs::Cdb10>() as u8,
            data_transfer_length: byte_len as u32,
            ..FromZeros::new_zeroed()
        };

        scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());
        scsi_req
    }

    #[async_test]
    async fn test_negotiation(driver: DefaultDriver) {
        let (guest, host) = connected_async_channels(16 * 1024);
        let host_queue = Queue::new(host).unwrap();
        let test_guest_mem = GuestMemory::allocate(16384);

        let storvsp = TestStorvspWorker::start(
            driver.clone(),
            test_guest_mem.clone(),
            host_queue,
            Vec::new(),
        );
        let mut storvsc = TestStorvscWorker::new();
        storvsc.start(driver.clone(), guest);

        // Wait for negotiation or panic.
        let mut timer = PolledTimer::new(&driver);
        let negotiation_timeout_millis = 1000;
        storvsc
            .wait_for_negotiation(&mut timer, negotiation_timeout_millis)
            .await;

        storvsc.stop().await;
        assert!(storvsc.get_mut().has_negotiated);

        storvsc.teardown().await;
        storvsp.teardown().await;
    }

    #[async_test]
    async fn test_request_response(driver: DefaultDriver) {
        let (guest, host) = connected_async_channels(16 * 1024);
        let host_queue = Queue::new(host).unwrap();
        let test_guest_mem = GuestMemory::allocate(16384);

        let storvsp = TestStorvspWorker::start(
            driver.clone(),
            test_guest_mem.clone(),
            host_queue,
            Vec::new(),
        );
        let mut storvsc = TestStorvscWorker::new();
        storvsc.start(driver.clone(), guest);

        // Wait for negotiation or panic.
        let mut timer = PolledTimer::new(&driver);
        let negotiation_timeout_millis = 1000;
        storvsc
            .wait_for_negotiation(&mut timer, negotiation_timeout_millis)
            .await;

        storvsc.stop().await;
        assert!(storvsc.get_mut().has_negotiated);
        storvsc.resume().await;

        // Send SCSI write request
        let write_buf = [7u8; 4096];
        test_guest_mem.write_at(4096, &write_buf).unwrap();
        storvsc
            .send_request(&generate_write_packet(0, 1, 2, 4096, 4096), 4096, 4096)
            .await
            .unwrap();

        // Send SCSI read request
        let write_buf = [7u8; 4096];
        test_guest_mem.write_at(4096, &write_buf).unwrap();
        storvsc
            .send_request(&generate_read_packet(0, 1, 2, 4096, 4096), 4096, 4096)
            .await
            .unwrap();

        storvsc.teardown().await;
        storvsp.teardown().await;
    }

    #[async_test]
    async fn test_enumerate_bus(driver: DefaultDriver) {
        let (guest, host) = connected_async_channels(16 * 1024);
        let host_queue = Queue::new(host).unwrap();
        let test_guest_mem = GuestMemory::allocate(16384);

        let mut storvsp = TestStorvspWorker::start(
            driver.clone(),
            test_guest_mem.clone(),
            host_queue,
            Vec::new(),
        );
        let mut storvsc = TestStorvscWorker::new();
        storvsc.start(driver.clone(), guest);

        // Wait for negotiation or panic.
        let mut timer = PolledTimer::new(&driver);
        let negotiation_timeout_millis = 1000;
        storvsc
            .wait_for_negotiation(&mut timer, negotiation_timeout_millis)
            .await;

        storvsc.stop().await;
        assert!(storvsc.get_mut().has_negotiated);
        storvsc.resume().await;

        // Inject an ENUMERATE_BUS command on the VMBus ring
        let enumerate_bus_packet = storvsp_protocol::Packet {
            operation: storvsp_protocol::Operation::ENUMERATE_BUS,
            flags: 0,
            status: storvsp_protocol::NtStatus::SUCCESS,
        };
        storvsp.send_vmbus_data_packet_no_completion(enumerate_bus_packet, 10, &());
        // Nothing to evaluate here since this is a no-op without a completion for the time being.

        storvsc.teardown().await;
        storvsp.teardown().await;
    }
}
