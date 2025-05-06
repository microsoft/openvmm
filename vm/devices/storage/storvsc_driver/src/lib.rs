// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Storvsc driver for use as a disk backend.

#[cfg(test)]
mod test_helpers;

use async_channel::Receiver;
use async_channel::RecvError;
use async_channel::Sender;
use futures::FutureExt;
use futures_concurrency::future::Race;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::ranges::PagedRange;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use task_control::AsyncRun;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use tracing_helpers::ErrorValueExt;
use vmbus_async::queue;
use vmbus_async::queue::CompletionPacket;
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
pub struct StorvscDriver<T: Send + Sync + RingMem> {
    storvsc: TaskControl<StorvscState, Storvsc<T>>,
    version: storvsp_protocol::ProtocolVersion,
    driver_source: VmTaskDriverSource,
    new_request_sender: Option<Sender<StorvscRequest>>,
}

/// Storvsc backend for SCSI devices.
struct Storvsc<T: Send + Sync + RingMem> {
    inner: StorvscInner,
    version: storvsp_protocol::ProtocolVersion,
    queue: Queue<T>,
    num_sub_channels: Option<u16>,
    has_negotiated: bool,
}

struct StorvscInner {
    next_transaction_id: AtomicU64,
    new_request_receiver: Receiver<StorvscRequest>,
    transactions: Mutex<HashMap<u64, PendingOperation>>,
}

struct StorvscRequest {
    request: storvsp_protocol::ScsiRequest,
    buf_gpa: u64,
    byte_len: usize,
    completion_sender: Sender<StorvscCompletion>,
}

/// Result of a Storvsc operation. If None, then operation was cancelled.
pub struct StorvscCompletion {
    completion: Option<storvsp_protocol::ScsiRequest>,
}

struct PendingOperation {
    sender: Sender<StorvscCompletion>,
}

impl PendingOperation {
    fn new(sender: Sender<StorvscCompletion>) -> Self {
        Self { sender }
    }

    fn complete(&mut self, result: storvsp_protocol::ScsiRequest) -> Result<(), StorvscError> {
        self.sender
            .send_blocking(StorvscCompletion {
                completion: Some(result),
            })
            .map_err(|_err| StorvscError::NotificationError)
    }

    fn cancel(&mut self) {
        // Sending completion with an empty result indicates cancellation or other error.
        self.sender
            .send_blocking(StorvscCompletion { completion: None })
            .unwrap();
    }
}

/// Errors resulting from storvsc.
#[derive(Debug, Error)]
pub enum StorvscError {
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
    /// Error notifying completion of operation.
    #[error("error notifying completion of operation")]
    NotificationError,
    /// Error sending request to storvsc driver.
    #[error("error sending request to storvsc")]
    RequestError,
    /// Error receiving new operations from channel.
    #[error("error receiving new operations from channel")]
    RequestReceiveError(#[source] RecvError),
    /// Error waiting for completion of operation.
    #[error("error waiting for completion of operation")]
    CompletionError(#[source] RecvError),
    /// Operation cancelled.
    #[error("pending operation cancelled")]
    Cancelled,
    /// Storvsc driver not fully initialized.
    #[error("driver not initialized")]
    Uninitialized,
}

/// Errors with packet parsing between storvsc and storvsp.
#[derive(Debug, Error)]
pub enum PacketError {
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
    #[error("Access error: {0}")]
    Access(#[source] AccessError),
    /// Range error.
    #[error("Range error")]
    Range(#[source] ExternalDataError),
}

impl<T: 'static + Send + Sync + RingMem> StorvscDriver<T> {
    /// Create a new driver instance connected to storvsp over VMBus.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        version: storvsp_protocol::ProtocolVersion,
    ) -> Self {
        Self {
            storvsc: TaskControl::new(StorvscState),
            version,
            driver_source: driver_source.clone(),
            new_request_sender: None,
        }
    }

    /// Start Storvsc.
    pub fn run(&mut self, channel: RawAsyncChannel<T>, target_vp: u32) -> Result<(), StorvscError> {
        let driver = self
            .driver_source
            .builder()
            .target_vp(target_vp)
            .run_on_target(true)
            .build("storvsc");
        let (new_request_sender, new_request_receiver) =
            async_channel::unbounded::<StorvscRequest>();
        let storvsc = Storvsc::new(channel, self.version, new_request_receiver)?;
        self.new_request_sender = Some(new_request_sender);

        self.storvsc.insert(&driver, "storvsc", storvsc);
        self.storvsc.start();
        Ok(())
    }

    /// Stop Storvsc.
    pub async fn stop(&mut self) {
        self.storvsc.stop().await;
        self.storvsc.remove();
    }

    /// Send a SCSI request to storvsp over VMBus.
    pub async fn send_request(
        &mut self,
        request: &storvsp_protocol::ScsiRequest,
        buf_gpa: u64,
        byte_len: usize,
    ) -> Result<storvsp_protocol::ScsiRequest, StorvscError> {
        let (sender, receiver) = async_channel::unbounded::<StorvscCompletion>();
        let storvsc_request = StorvscRequest {
            request: *request,
            buf_gpa,
            byte_len,
            completion_sender: sender,
        };
        match &self.new_request_sender {
            Some(request_sender) => {
                request_sender
                    .send(storvsc_request)
                    .await
                    .map_err(|_err| StorvscError::RequestError)?;
                Ok(())
            }
            None => Err(StorvscError::Uninitialized),
        }?;

        let resp = receiver
            .recv()
            .await
            .map_err(StorvscError::CompletionError)?;

        if resp.completion.is_some() {
            Ok(resp.completion.unwrap())
        } else {
            Err(StorvscError::Cancelled)
        }
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

impl<T: 'static + Send + Sync + RingMem> Storvsc<T> {
    pub(crate) fn new(
        channel: RawAsyncChannel<T>,
        version: storvsp_protocol::ProtocolVersion,
        new_request_receiver: Receiver<StorvscRequest>,
    ) -> Result<Self, StorvscError> {
        let queue = Queue::new(channel).map_err(StorvscError::Queue)?;

        Ok(Self {
            inner: StorvscInner {
                next_transaction_id: AtomicU64::new(1),
                new_request_receiver,
                transactions: Mutex::new(HashMap::new()),
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
                &(),
            )
            .await?;

        // Step 2: QUERY_PROTOCOL_VERSION - request latest version
        self.inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::QUERY_PROTOCOL_VERSION,
                &self.version,
            )
            .await
            .map_err(|err| match err {
                StorvscError::PacketError(PacketError::UnexpectedStatus(
                    storvsp_protocol::NtStatus::INVALID_DEVICE_STATE,
                )) => StorvscError::UnsupportedProtocolVersion,
                _ => err,
            })?;

        // Step 3: QUERY_PROPERTIES
        let properties_packet = self
            .inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::QUERY_PROPERTIES,
                &(),
            )
            .await?;
        let properties = storvsp_protocol::ChannelProperties::ref_from_prefix(
            &properties_packet.data[0..properties_packet.data_size],
        )
        .map_err(|_err| StorvscError::UnexpectedOperation)?
        .0
        .to_owned();

        // Step 4: CREATE_SUB_CHANNELS (if supported)
        if properties.maximum_sub_channel_count > 0 {
            self.num_sub_channels = Some(properties.maximum_sub_channel_count);
            // Decrease by 1 until we are able to negotiate (or give up if we reach 0)
            while self.num_sub_channels.unwrap() > 0 {
                match self
                    .inner
                    .send_packet_and_expect_completion(
                        &mut self.queue,
                        storvsp_protocol::Operation::CREATE_SUB_CHANNELS,
                        &self.num_sub_channels.unwrap(),
                    )
                    .await
                {
                    Ok(_packet) => break,
                    Err(_err) => {
                        self.num_sub_channels = Some(self.num_sub_channels.unwrap() - 1);
                    }
                };
            }
        }

        // Step 5: END_INITIALIZATION
        self.inner
            .send_packet_and_expect_completion(
                &mut self.queue,
                storvsp_protocol::Operation::END_INITIALIZATION,
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
            Err(StorvscError::Queue(err2)) => {
                if err2.is_closed_error() {
                    // This is expected, cancel any pending completions
                    self.inner.cancel_pending_completions().await;
                    Ok(())
                } else {
                    Err(StorvscError::Queue(err2))
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
                        Err(StorvscError::RequestError)
                    }
                },
                Event::VmbusPacketReceived(result) => match result {
                    Ok(packet_ref) => self.handle_packet(packet_ref.as_ref()),
                    Err(err) => {
                        tracing::error!("Error receiving VMBus packet, err={:?}", err);
                        Err(StorvscError::Queue(err))
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
        // Fetch a transaction ID for this operation
        let transaction_id = self.get_next_transaction_id();

        // Create pending transaction record
        {
            self.transactions
                .lock()
                .insert(transaction_id, PendingOperation::new(completion_sender));
        }

        self.send_gpa_direct_packet(
            writer,
            storvsp_protocol::Operation::EXECUTE_SRB,
            storvsp_protocol::NtStatus::SUCCESS,
            transaction_id,
            request,
            buf_gpa,
            byte_len,
        )
    }

    async fn cancel_pending_completions(&mut self) {
        for transaction in self.transactions.lock().values_mut() {
            transaction.cancel();
        }
    }

    fn handle_packet<M: RingMem>(
        &mut self,
        packet: &IncomingPacket<'_, M>,
    ) -> Result<(), StorvscError> {
        let packet = parse_packet(packet)?;
        let completion = match packet {
            //Packet::Data(_) => Err(StorvscError::UnexpectedOperation),
            Packet::Completion(completion) => Ok(completion),
        }?;

        // Parse ScsiRequest (contains response) from bytes
        let result = storvsp_protocol::ScsiRequest::ref_from_bytes(completion.data.as_slice())
            .map_err(|_err| StorvscError::UnexpectedOperation)?
            .to_owned();

        // Match completion against pending transactions
        {
            match self.transactions.lock().get_mut(&completion.transaction_id) {
                Some(t) => Ok(t),
                None => Err(StorvscError::PacketError(
                    PacketError::UnexpectedTransaction(completion.transaction_id),
                )),
            }
        }?
        .complete(result)?;

        Ok(())
    }

    /// Awaits the next incoming packet. Increments the count of outstanding packets when returning `Ok(Packet)`.
    async fn next_packet<'a, M: RingMem>(
        &mut self,
        reader: &'a mut queue::ReadHalf<'a, M>,
    ) -> Result<Packet, StorvscError> {
        let packet = reader.read().await.map_err(StorvscError::Queue)?;
        parse_packet(&packet)
    }

    fn get_next_transaction_id(&mut self) -> u64 {
        self.next_transaction_id.fetch_add(1, Ordering::AcqRel)
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
            payload_bytes.len(),
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
            payload_bytes.len(),
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
        request_size: usize,
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

        let packet_size = size_of_val(&header) + request_size;

        // Zero pad or truncate the payload to the queue's packet size. This is
        // necessary because Windows guests check that each packet's size is
        // exactly the largest possible packet size for the negotiated protocol
        // version.
        let len = size_of_val(&header) + size_of_val(payload);
        let padding = [0; storvsp_protocol::SCSI_REQUEST_LEN_MAX];
        let (payload_bytes, padding_bytes) = if len > packet_size {
            (&payload[..packet_size - size_of_val(&header)], &[][..])
        } else {
            (payload, &padding[..packet_size - len])
        };
        assert_eq!(
            size_of_val(&header) + payload_bytes.len() + padding_bytes.len(),
            packet_size
        );
        writer
            .try_write(&OutgoingPacket {
                transaction_id,
                packet_type,
                payload: &[header.as_bytes(), payload_bytes, padding_bytes],
            })
            .map_err(|err| match err {
                queue::TryWriteError::Full(_) => StorvscError::NotEnoughSpace,
                queue::TryWriteError::Queue(err) => StorvscError::Queue(err),
            })
    }

    async fn send_packet_and_expect_completion<
        M: RingMem,
        P: IntoBytes + Immutable + KnownLayout,
    >(
        &mut self,
        queue: &mut Queue<M>,
        operation: storvsp_protocol::Operation,
        payload: &P,
    ) -> Result<StorvscCompletionPacket, StorvscError> {
        let (mut reader, mut writer) = queue.split();
        let transaction_id = self.get_next_transaction_id();
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
            //Packet::Data(_) => Err(StorvscError::PacketError(PacketError::InvalidPacketType)),
        }?;
        expect_success(
            expect_transaction_id(completion, transaction_id).map_err(StorvscError::PacketError)?,
        )
        .map_err(StorvscError::PacketError)
    }
}

enum Packet {
    Completion(StorvscCompletionPacket),
    //Data(StorvscDataPacket),
}

#[derive(Debug)]
struct StorvscCompletionPacket {
    transaction_id: u64,
    status: storvsp_protocol::NtStatus,
    data_size: usize,
    data: [u8; storvsp_protocol::SCSI_REQUEST_LEN_MAX],
}

fn parse_packet<T: RingMem>(packet: &IncomingPacket<'_, T>) -> Result<Packet, StorvscError> {
    match packet {
        IncomingPacket::Completion(completion) => {
            parse_completion(completion).map_err(StorvscError::PacketError)
        }
        IncomingPacket::Data(_) => Err(StorvscError::PacketError(PacketError::InvalidPacketType)),
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

fn expect_success(packet: StorvscCompletionPacket) -> Result<StorvscCompletionPacket, PacketError> {
    if packet.status != storvsp_protocol::NtStatus::SUCCESS {
        return Err(PacketError::UnexpectedStatus(packet.status));
    }
    Ok(packet)
}

fn expect_transaction_id(
    packet: StorvscCompletionPacket,
    transaction_id: u64,
) -> Result<StorvscCompletionPacket, PacketError> {
    if packet.transaction_id != transaction_id {
        return Err(PacketError::UnexpectedTransaction(packet.transaction_id));
    }
    Ok(packet)
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
    use std::time;
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

        let mut timer = PolledTimer::new(&driver);
        timer.sleep(time::Duration::from_secs(1)).await;

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

        let mut timer = PolledTimer::new(&driver);
        timer.sleep(time::Duration::from_secs(1)).await;

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
}
