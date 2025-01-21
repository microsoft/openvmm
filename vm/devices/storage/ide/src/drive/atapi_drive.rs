// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements atapi commands handler of optical drive.

use super::DriveRegister;
use crate::protocol;
use crate::protocol::DeviceControlReg;
use crate::protocol::DeviceHeadReg;
use crate::protocol::ErrorReg;
use crate::protocol::IdeCommand;
use crate::protocol::Status;
use crate::DmaType;
use guestmem::ranges::PagedRange;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use ide_resources::IdePath;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use scsi::AdditionalSenseCode;
use scsi::SenseKey;
use scsi_buffers::RequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::ScsiResult;
use scsi_defs as scsi;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// This disk supports 12-byte CDBs.
const COMMAND_PACKET_SIZE: usize = 12;

const MAX_TRANSFER_LEN: usize = 128 * 1024;

#[derive(Debug, Copy, Clone, PartialEq, Inspect)]
struct Sense {
    #[inspect(debug)]
    sense_key: SenseKey,
    #[inspect(debug)]
    additional_sense_code: AdditionalSenseCode,
    additional_sense_code_qualifier: u8,
}

impl Default for Sense {
    fn default() -> Self {
        Self {
            sense_key: FromZeros::new_zeroed(),
            additional_sense_code: FromZeros::new_zeroed(),
            additional_sense_code_qualifier: Default::default(),
        }
    }
}

impl Sense {
    fn new(
        sense_key: SenseKey,
        additional_sense_code: AdditionalSenseCode,
        additional_sense_code_qualifier: u8,
    ) -> Self {
        Self {
            sense_key,
            additional_sense_code,
            additional_sense_code_qualifier,
        }
    }
}

/// The device command and control register sets.
///
#[derive(Debug, Inspect)]
struct Registers {
    error: ErrorReg, // N.B. this may have a value even if !error_pending
    #[inspect(hex)]
    features: u8,
    device_head: DeviceHeadReg,
    #[inspect(hex)]
    lba_low: u8, // Linux writes to it
    #[inspect(hex)]
    byte_count_low: u8,
    #[inspect(hex)]
    byte_count_high: u8,
    #[inspect(hex)]
    sector_count: u8,
    device_control_reg: DeviceControlReg,
    status: Status,
}

impl Registers {
    fn at_reset() -> Self {
        Self {
            byte_count_low: protocol::ATAPI_RESET_LBA_MID,
            byte_count_high: protocol::ATAPI_RESET_LBA_HIGH,
            lba_low: 1, // CHS mode
            device_head: DeviceHeadReg::new(),
            error: ErrorReg::new().with_amnf_ili_default(true),
            features: 0,
            sector_count: protocol::ATAPI_READY_FOR_PACKET_DEFAULT,
            device_control_reg: DeviceControlReg::new(),
            status: Status::new(),
        }
    }

    fn reset_signature(&mut self, reset_dev: bool) {
        self.byte_count_low = protocol::ATAPI_RESET_LBA_MID;
        self.byte_count_high = protocol::ATAPI_RESET_LBA_HIGH;
        self.lba_low = 1;
        self.sector_count = 1;
        let dev = if reset_dev {
            false
        } else {
            self.device_head.dev()
        };
        self.device_head = DeviceHeadReg::new().with_dev(dev);
    }
}

#[derive(Debug)]
struct CommandBuffer {
    buffer: Arc<AlignedHeapMemory>,
}

#[derive(Debug)]
struct CommandBufferAccess {
    memory: GuestMemory,
}

impl CommandBuffer {
    fn new() -> Self {
        Self {
            buffer: Arc::new(AlignedHeapMemory::new(MAX_TRANSFER_LEN)),
        }
    }

    fn access(&self) -> CommandBufferAccess {
        CommandBufferAccess {
            memory: GuestMemory::new("atapi_buffer", self.buffer.clone()),
        }
    }
}

impl CommandBufferAccess {
    fn buffers(&self, offset: usize, len: usize, is_write: bool) -> RequestBuffers<'_> {
        // The buffer is 32 4KB pages long.
        static BUFFER_RANGE: Option<PagedRange<'_>> = PagedRange::new(
            0,
            MAX_TRANSFER_LEN,
            &[
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
                23, 24, 25, 26, 27, 28, 29, 30, 31,
            ],
        );

        RequestBuffers::new(
            &self.memory,
            BUFFER_RANGE.unwrap().subrange(offset, len),
            is_write,
        )
    }
}

#[derive(Debug, Inspect)]
struct BufferState {
    /// The current byte into the buffer.
    current_byte: u32,
    /// The total number of bytes in buffer.
    total_bytes: u32,
    /// If this buffer can be accessed via DMA, the direction of access.
    dma_type: Option<DmaType>,
}

impl BufferState {
    fn new(len: u32, dma: Option<DmaType>) -> Self {
        assert!(len != 0);
        assert!((len as usize) <= MAX_TRANSFER_LEN);
        Self {
            current_byte: 0,
            total_bytes: len,
            dma_type: dma,
        }
    }

    fn range(&self) -> std::ops::Range<usize> {
        self.current_byte as usize..self.total_bytes as usize
    }

    /// Returns true if the buffer is exhausted.
    #[must_use]
    fn advance(&mut self, n: u32) -> bool {
        self.current_byte += n;
        assert!(self.current_byte <= self.total_bytes);
        self.is_empty()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> u32 {
        self.total_bytes - self.current_byte
    }
}

enum IoPortData<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

#[derive(Debug, Inspect)]
struct AtapiDriveState {
    regs: Registers,
    pending_software_reset: bool,
    pending_interrupt: bool,
    error_pending: bool,
    pending_packet_command: bool,
    buffer: Option<BufferState>,
}

impl AtapiDriveState {
    fn new() -> Self {
        Self {
            regs: Registers::at_reset(),
            pending_software_reset: false,
            pending_interrupt: false,
            error_pending: false,
            pending_packet_command: false,
            buffer: None,
        }
    }
}

struct Io(Pin<Box<dyn Send + Future<Output = ScsiResult>>>);

impl std::fmt::Debug for Io {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("io")
    }
}

#[derive(Inspect)]
pub(crate) struct AtapiDrive {
    scsi_disk: Arc<dyn AsyncScsiDisk>,
    state: AtapiDriveState,
    disk_path: IdePath,

    #[inspect(skip)]
    command_buffer: CommandBuffer,

    #[inspect(skip)]
    io: Option<Io>,
    #[inspect(skip)]
    waker: Option<Waker>,
}

impl AtapiDrive {
    pub fn reset(&mut self) {
        tracing::debug!(path = ?self.disk_path, "drive reset");
        self.state = AtapiDriveState::new();
    }

    pub fn pio_read(&mut self, data: &mut [u8]) {
        if self.is_selected() {
            self.data_port_io(IoPortData::Read(data));
        }
    }

    pub fn pio_write(&mut self, data: &[u8]) {
        if self.is_selected() {
            self.data_port_io(IoPortData::Write(data));
        }
    }

    pub fn read_register(&mut self, register: DriveRegister) -> u8 {
        tracing::trace!(path = ?self.disk_path, ?register, "read_register");
        if !self.is_selected() {
            // Align with ATA-7 "If the device implements the PACKET Command feature set, a read of the Control Block or
            // Command Block registers shall return the value 00h." in a single device configuration where Device 0 is
            // the only device and the host selects Device 1.
            // Note: legacy implementation returns Device 0's content on DeviceHead & SectorCount and 0x7f on StatusCmd &
            // AlternateStatusDeviceControl in this case.
            return 0;
        }

        let regs = &self.state.regs;
        match register {
            DriveRegister::ErrorFeatures => regs.error.into_bits(),
            DriveRegister::SectorCount => regs.sector_count,
            DriveRegister::LbaLow => regs.lba_low,
            DriveRegister::LbaMid => regs.byte_count_low,
            DriveRegister::LbaHigh => regs.byte_count_high,
            DriveRegister::DeviceHead => regs.device_head.into(),
            DriveRegister::StatusCmd => {
                let status = self.state.regs.status;
                tracing::trace!(status = ?status, path = ?self.disk_path, "status query, deasserting");
                self.request_interrupt(false);
                status.into_bits()
            }
            DriveRegister::AlternateStatusDeviceControl => {
                let status = self.state.regs.status;
                tracing::trace!(status = ?status, path = ?self.disk_path, "alter status query");
                status.into_bits()
            }
        }
    }

    pub fn write_register(&mut self, register: DriveRegister, data: u8) {
        tracing::trace!(path = ?self.disk_path, ?register, ?data, "write_register");
        let regs = &mut self.state.regs;
        match register {
            DriveRegister::ErrorFeatures => regs.features = data,
            DriveRegister::SectorCount => regs.sector_count = data,
            DriveRegister::LbaLow => regs.lba_low = data,
            DriveRegister::LbaMid => regs.byte_count_low = data,
            DriveRegister::LbaHigh => regs.byte_count_high = data,
            DriveRegister::DeviceHead => {
                self.write_device_head(data);
            }
            DriveRegister::StatusCmd => {
                // Ignore commands targeted at the wrong disk due to missing media.
                //
                // EXECUTE DEVICE DIAGNOSTIC command sets error register for both attachments on channel
                if self.is_selected() || data == IdeCommand::EXECUTE_DEVICE_DIAGNOSTIC.0 {
                    self.handle_command(data);
                }
            }
            DriveRegister::AlternateStatusDeviceControl => {
                let v = DeviceControlReg::from_bits_truncate(data);
                self.state.regs.device_control_reg = v.with_reset(false);
                if v.reset() && !self.state.pending_software_reset {
                    if !self.state.regs.status.bsy() {
                        self.reset();
                    } else {
                        self.state.pending_software_reset = true;
                        self.state.regs.status.set_bsy(true);
                    }
                }
            }
        }
    }

    pub fn interrupt_pending(&self) -> bool {
        self.state.pending_interrupt
            && !self.state.regs.device_control_reg.interrupt_mask()
            && self.is_selected()
            && !self.state.pending_software_reset
    }

    pub fn dma_request(&self) -> Option<(&DmaType, usize)> {
        if let Some(buffer) = &self.state.buffer {
            buffer
                .dma_type
                .as_ref()
                .map(|ty| (ty, buffer.len() as usize))
        } else {
            None
        }
    }

    pub fn dma_transfer(&mut self, guest_memory: &GuestMemory, gpa: u64, len: usize) {
        let buffer = self.state.buffer.as_ref().unwrap();
        assert!(len <= buffer.len() as usize);
        let dma_type = *buffer.dma_type.as_ref().unwrap();
        tracing::trace!(
            ?dma_type,
            gpa,
            len,
            cur_byte = buffer.current_byte,
            "performing dma"
        );
        let range = buffer.range();
        let buffer_ptr = &self.command_buffer.buffer[range][..len];
        let r = match dma_type {
            DmaType::Write => guest_memory.write_from_atomic(gpa, buffer_ptr),
            DmaType::Read => guest_memory.read_to_atomic(gpa, buffer_ptr),
        };
        if let Err(err) = r {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dma transfer failed"
            );
        }
        if self.state.buffer.as_mut().unwrap().advance(len as u32) {
            match dma_type {
                DmaType::Write => self.read_data_port_buffer_complete(),
                DmaType::Read => self.write_data_port_buffer_complete(),
            };
            if self.state.buffer.is_none() {
                assert!(!self.state.regs.status.bsy());
                assert!(!self.state.regs.status.drq());
                self.request_interrupt(true);
            }
        }
    }

    pub fn dma_advance_buffer(&mut self, len: usize) {
        let buffer = self.state.buffer.as_ref().unwrap();
        assert!(len <= buffer.len() as usize);
        let dma_type = *buffer.dma_type.as_ref().unwrap();
        tracing::trace!(
            ?dma_type,
            len,
            cur_byte = buffer.current_byte,
            "advancing dma buffer"
        );
        if self.state.buffer.as_mut().unwrap().advance(len as u32) {
            match dma_type {
                DmaType::Write => self.read_data_port_buffer_complete(),
                DmaType::Read => self.write_data_port_buffer_complete(),
            };
            if self.state.buffer.is_none() {
                assert!(!self.state.regs.status.bsy());
                assert!(!self.state.regs.status.drq());
                self.request_interrupt(true);
            }
        }
    }
}

impl AtapiDrive {
    fn request_interrupt(&mut self, v: bool) {
        tracing::trace!(pending_interrupt = v, "request_interrupt");
        self.state.pending_interrupt = v;
    }

    fn write_device_head(&mut self, data: u8) {
        let old_device_head = self.state.regs.device_head;
        self.state.regs.device_head = data.into();
        tracing::trace!(
            path = ?self.disk_path,
            old_device = old_device_head.dev() as u8,
            new_device = self.state.regs.device_head.dev() as u8,
            "write_device_head"
        );
        // After the device select, update status for the selected device.
        if self.is_selected() && self.state.regs.status.into_bits() == 0 {
            // Update the status of the device if it is 0
            self.state.regs.status = Status::new().with_drdy(true).with_dsc(true);
        }
    }

    fn read_data_port_buffer_complete(&mut self) {
        if self.state.pending_packet_command {
            self.signal_atapi_command_done(Sense::default());
        } else {
            self.complete_data_port_read();
        }
    }

    fn complete_data_port_read(&mut self) {
        self.state.regs.status.set_bsy(false);
        self.state.regs.status.set_err(false);
        self.state.regs.status.set_drq(false);
        self.state.regs.status.set_drdy(true);
        self.state.regs.status.set_dsc(true);

        self.state.regs.sector_count = protocol::ATAPI_COMMAND_COMPLETE;
        self.state.buffer = None;
    }

    fn write_data_port_buffer_complete(&mut self) {
        self.handle_atapi_packet_command()
    }

    fn handle_command(&mut self, command: u8) {
        self.handle_atapi_command(command);
    }

    /// Returns whether this device is currently selected.
    ///
    /// This will be false when this device is being targeted due to the other
    /// device being missing.
    fn is_selected(&self) -> bool {
        self.state.regs.device_head.dev() as u8 == self.disk_path.drive
    }

    fn data_port_io(&mut self, mut io_type: IoPortData<'_>) {
        let Some(buffer_state) = &mut self.state.buffer else {
            tracelimit::warn_ratelimited!("no buffer available");
            return;
        };

        let length = match io_type {
            IoPortData::Read(ref data) => {
                tracing::trace!(
                    cur_byte = buffer_state.current_byte,
                    total_bytes = buffer_state.total_bytes,
                    length = data.len(),
                    path = ?self.disk_path,
                    "data port read"
                );

                data.len()
            }
            IoPortData::Write(data) => {
                tracing::trace!(
                    cur_byte = buffer_state.current_byte,
                    total_bytes = buffer_state.total_bytes,
                    length = data.len(),
                    path = ?self.disk_path,
                    "data port write"
                );
                data.len()
            }
        } as u32;

        let range = buffer_state.range();
        let current_buffer = &self.command_buffer.buffer[range];

        let length = length.min(current_buffer.len() as u32);
        if length == 0 {
            return;
        }

        // Any buffer size errors at this point are fatal.
        match io_type {
            IoPortData::Read(ref mut data) => {
                current_buffer[..length as usize].atomic_read(&mut data[..length as usize]);
                tracing::trace!(?data, "data payload");
            }
            IoPortData::Write(data) => {
                current_buffer[..length as usize].atomic_write(&data[..length as usize]);
                tracing::trace!(?current_buffer, ?data, "data_port_io");
            }
        }

        if self.state.buffer.as_mut().unwrap().advance(length) {
            match io_type {
                IoPortData::Read(_) => self.read_data_port_buffer_complete(),
                IoPortData::Write(_) => self.write_data_port_buffer_complete(),
            }
        }
    }
}

impl AtapiDrive {
    pub fn new(scsi_disk: Arc<dyn AsyncScsiDisk>, disk_path: IdePath) -> Self {
        Self {
            scsi_disk,
            state: AtapiDriveState::new(),
            disk_path,
            command_buffer: CommandBuffer::new(),
            io: None,
            waker: None,
        }
    }

    fn state(&self) -> &AtapiDriveState {
        &self.state
    }
    fn state_mut(&mut self) -> &mut AtapiDriveState {
        &mut self.state
    }

    pub fn handle_read_dma_descriptor_error(&mut self) -> bool {
        // Check if there's any pending IO
        if self.io.is_none() {
            if self.state.pending_software_reset {
                self.reset();
            }
            self.state.regs.status.set_bsy(false);
            self.state.regs.status.set_drq(false);
            return true;
        }

        // yet to clear out dma_error
        false
    }

    fn handle_atapi_command(&mut self, command: u8) {
        let command = IdeCommand(command);
        tracing::debug!(path = ?self.disk_path, ?command, ?self.state.regs, "atapi command");

        if self.state.regs.status.bsy() {
            tracelimit::warn_ratelimited!(new_command = ?command, "A command is already pending");
            return;
        }

        if self.state.regs.status.drq() {
            tracelimit::warn_ratelimited!(new_command = ?command, "data transfer is in progress");
            return;
        }

        self.state.regs.status.set_drdy(false);
        self.state.regs.status.set_bsy(true);
        self.state.regs.error = ErrorReg::new();
        self.state.regs.status.set_err(false);

        match command {
            IdeCommand::DEVICE_RESET => {
                // Needn't issue interrupt
                return self.handle_soft_reset(false);
            }
            IdeCommand::EXECUTE_DEVICE_DIAGNOSTIC => {
                // As specified by ATA-6 9.12.
                self.state.regs.reset_signature(true);
                self.state.regs.error = ErrorReg::new().with_amnf_ili_default(true);
            }
            IdeCommand::PACKET_COMMAND => {
                // Needn't issue interrupt
                return self.prepare_atapi_command_packet();
            }
            IdeCommand::IDENTIFY_PACKET_DEVICE => {
                self.read_features();
            }
            // Checks whether the drive is actually spinning or idle.
            // Specify that drive is actively spinning (i.e. in "idle"
            // state vs. the "standby" state).
            IdeCommand::CHECK_POWER_MODE => {
                self.state.regs.sector_count = protocol::DEVICE_ACTIVE_OR_IDLE;
            }
            IdeCommand::SET_FEATURES => {
                // Nothing for optical drive.
            }
            IdeCommand::IDENTIFY_DEVICE | IdeCommand::READ_SECTORS => {
                // As specified by ATA-6 9.12.
                self.state.regs.reset_signature(false);
                self.state.regs.status.set_err(true);
                self.state.regs.error = ErrorReg::new().with_unknown_command(true);
            }
            command => {
                tracing::debug!(?command, "unknown command");
                self.state.regs.status.set_err(true);
                self.state.regs.error = ErrorReg::new().with_unknown_command(true);
            }
        };

        self.state.regs.status.set_bsy(false);
        self.state.regs.status.set_drdy(true);
        self.state.regs.status.set_dsc(true);
        self.request_interrupt(true);
    }

    fn get_dma(&self) -> Option<DmaType> {
        if self.state.regs.features & 0x1 != 0x0 {
            Some(DmaType::Write)
        } else {
            None
        }
    }

    // This function is called when an IDE CmdPacket command is sent
    // to the IDE controller. This tells the controller that an
    // ATAPI command is going to follow in the write buffer.
    fn prepare_atapi_command_packet(&mut self) {
        let dma = self.get_dma();
        tracing::trace!(?dma, "prepare atapi command");
        // Specify that the buffer is ready to receive bytes
        self.state.buffer = Some(BufferState::new(COMMAND_PACKET_SIZE as u32, dma));
        self.state.regs.sector_count = protocol::ATAPI_READY_FOR_PACKET_DEFAULT;
        self.state.regs.status.set_bsy(false);
        self.state.regs.status.set_drdy(true);
        self.state.regs.status.set_dsc(true);
        self.state.regs.status.set_drq(true);
    }

    fn handle_soft_reset(&mut self, reset_dev: bool) {
        tracing::debug!(path = ?self.disk_path, "Command Soft Reset");
        self.state.buffer = None;

        self.state.regs.reset_signature(reset_dev);
        self.state.regs.error = ErrorReg::new().with_amnf_ili_default(true);
        self.state.regs.status = Status::new();
    }

    /// IDENTIFY DEVICE command enables the host to receive parameter information
    /// from the device. The features structure is 256 words of device identification
    /// data that can be transferred to the host by reading the Data register.
    fn read_features(&mut self) {
        let features = protocol::IdeFeatures {
            config_bits: 0x85C0, // Indicate Accelerated DRQ
            serial_no: *b"                    ",
            buffer_size: 0x0080,
            firmware_revision: *b"        ",
            model_number: "iVtrau lDC                              ".as_bytes()[..]
                .try_into()
                .unwrap(),
            capabilities: 0x0300,          // LBA and Dma are supported
            pio_cycle_times: 0x0200,       // indicate fast I/O
            dma_cycle_times: 0x0200,       // indicate fast I/O
            new_words_valid_flags: 0x0003, // indicate next words are valid
            multi_sector_capabilities: 0x0100_u16 | protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT,
            single_word_dma_mode: 0x0007, // support up to mode 3, no mode active
            multi_word_dma_mode: 0x0407,  // support up to mode 3, mode 3 active
            enhanced_pio_mode: 0x0003,    // PIO mode 3 and 4 supported
            min_multi_dma_time: 0x0078,
            recommended_multi_dma_time: 0x0078,
            min_pio_cycle_time_no_flow: 0x01FC, // Taken from a real CD device
            min_pio_cycle_time_flow: 0x00B4,    // Taken from a real CD device
            ..FromZeros::new_zeroed()
        };

        self.command_buffer.buffer[..protocol::IDENTIFY_DEVICE_BYTES].atomic_write_obj(&features);
        self.state.buffer = Some(BufferState::new(
            protocol::IDENTIFY_DEVICE_BYTES as u32,
            None,
        ));
        self.state.regs.sector_count = protocol::ATAPI_DATA_FOR_HOST;
        self.state.regs.status.set_drq(true);
    }

    // This function handles ATAPI commands. These are only used
    // for CD-Rom devices currently.
    fn handle_atapi_packet_command(&mut self) {
        assert!(self.state.buffer.is_some());

        if self.state.pending_packet_command {
            tracelimit::error_ratelimited!(path = ?self.disk_path, "Unexpected: pending_packet_command at beginning of atapi packet command");
        }

        self.state.regs.status.set_drdy(false);
        self.state.regs.status.set_drq(false);
        self.state.regs.status.set_bsy(true);

        self.state.regs.byte_count_high = 0;
        self.state.regs.byte_count_low = 0;
        self.state.regs.sector_count = protocol::ATAPI_COMMAND_COMPLETE;

        let len = COMMAND_PACKET_SIZE;
        let buffer_ptr = &self.command_buffer.buffer[..len];

        let mut cdb = [0_u8; size_of::<scsi::Cdb16>()];
        // Copy from CommandPacket into the CDB.
        buffer_ptr.atomic_read(&mut cdb[..len]);
        tracing::debug!(path = ?self.disk_path, ?buffer_ptr, ?cdb, "Handle ATAPI packet command");

        self.state.buffer = None;
        let request = scsi_core::Request { cdb, srb_flags: 0 };
        self.set_io(request);
    }

    fn process_atapi_command_result(&mut self, result: ScsiResult) {
        // Command Completed: If error, signal it and return
        let sense = if let Some(sense_data) = result.sense_data {
            Sense::new(
                sense_data.header.sense_key,
                sense_data.additional_sense_code,
                sense_data.additional_sense_code_qualifier,
            )
        } else {
            Sense::default()
        };

        if sense != Sense::default() {
            tracing::debug!(path = ?self.disk_path, ?sense, "Issue ATAPI command error");
            return self.signal_atapi_command_done(sense);
        }

        // These commands do not need to signal any data
        if result.tx == 0 {
            self.signal_atapi_command_done(sense);
        } else {
            self.signal_atapi_data_ready(result.tx);
        }
    }

    // This function is called when an ATAPI command is complete.
    // Unlike normal IDE commands, ATAPI uses a final interrupt
    // to tell the host that the operation is complete and the
    // status can be read.
    fn signal_atapi_command_done(&mut self, sense: Sense) {
        tracing::debug!(path = ?self.disk_path, "Signal ATAPI Command done");

        // We are done with the operation, but we now need to
        // signal the status can be read. We do this by updating
        // the status and requesting an interrupt.
        self.state.regs.error = (sense.sense_key.0 << 4).into();
        self.state.regs.status.set_bsy(false);
        self.state.regs.status.set_err(false);
        self.state.regs.status.set_drq(false);
        self.state.regs.status.set_drdy(true);
        self.state.regs.status.set_dsc(true);

        if self.state.regs.error != ErrorReg::new() {
            // Set error flag
            if sense.sense_key == SenseKey::ILLEGAL_REQUEST
                || sense.sense_key == SenseKey::ABORTED_COMMAND
            {
                self.state.regs.error.set_unknown_command(true);
            }

            self.state.regs.status.set_err(true);
        }

        self.state.regs.sector_count = protocol::ATAPI_COMMAND_COMPLETE;
        self.state.buffer = None;
        self.state.pending_packet_command = false;
        self.request_interrupt(true);
    }

    // This function is called when an ATAPI command has been
    // processed and is ready to return data to the PC.
    fn signal_atapi_data_ready(&mut self, tx: usize) {
        assert!(tx > 0);
        tracing::trace!(path = ?self.disk_path, tx, "Signal ATAPI data ready");
        let use_dma = (self.state.regs.features & 0x1) != 0;
        if use_dma {
            assert!(tx <= MAX_TRANSFER_LEN);
        }
        self.state.buffer = Some(BufferState {
            current_byte: 0,
            total_bytes: tx as u32,
            dma_type: if use_dma { Some(DmaType::Write) } else { None },
        });

        // Prepare the IDE Controller state to return the data
        // this cast is safe because of the above max-check
        self.state.pending_packet_command = true;
        self.state.regs.byte_count_low = (tx & 0x00FF) as u8;
        self.state.regs.byte_count_high = ((tx & 0xFF00) >> 8) as u8;
        self.state.regs.sector_count = protocol::ATAPI_DATA_FOR_HOST;

        self.state.regs.status.set_bsy(false);
        self.state.regs.status.set_drq(true);
        self.state.regs.status.set_drdy(true);
        self.request_interrupt(true);
    }

    /// Sets the asynchronous IO to be polled in `poll_device`.
    fn set_io(&mut self, request: scsi_core::Request) {
        assert!(self.io.is_none());
        let scsi_disk = self.scsi_disk.clone();
        let access = self.command_buffer.access();
        let fut = async move {
            let buffers = access.buffers(0, MAX_TRANSFER_LEN, true);
            scsi_disk.execute_scsi(&buffers, &request).await
        };
        self.io = Some(Io(Box::pin(fut)));
        // Ensure poll_device gets called again.
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    pub fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(io) = self.io.as_mut() {
            if let Poll::Ready(result) = io.0.as_mut().poll(cx) {
                self.io = None;
                self.process_atapi_command_result(result);

                // Wait until the command that initiated this IO is completed
                if !self.state.regs.status.bsy() && self.state.pending_software_reset {
                    self.reset();
                }
            }
        }
        self.waker = Some(cx.waker().clone());
    }
}

pub(crate) mod save_restore {
    use self::state::SavedAtapiDriveState;
    use self::state::SavedDmaType;
    use self::state::SavedRegisterState;
    use super::*;
    use std::sync::atomic::Ordering;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;

    pub mod state {
        use mesh::payload::Protobuf;
        use scsi_core::save_restore::ScsiSavedState;

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.atapi")]
        pub struct SavedRegisterState {
            #[mesh(1)]
            pub error: u8,
            #[mesh(2)]
            pub features: u8,
            #[mesh(3)]
            pub device_head: u8,
            #[mesh(4)]
            pub lba_low: u8,
            #[mesh(5)]
            pub byte_count_low: u8,
            #[mesh(6)]
            pub byte_count_high: u8,
            #[mesh(7)]
            pub sector_count: u8,
            #[mesh(8)]
            pub device_control_reg: u8,
            #[mesh(9)]
            pub status: u8,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.atapi")]
        pub enum SavedDmaType {
            #[mesh(1)]
            Read,
            #[mesh(2)]
            Write,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.atapi")]
        pub struct SavedAtapiDriveState {
            #[mesh(1)]
            pub registers: SavedRegisterState,

            // Miscellaneous state
            #[mesh(2)]
            pub pending_interrupt: bool,
            #[mesh(3)]
            pub error_pending: bool,

            // Scsi State
            #[mesh(4)]
            pub scsi: ScsiSavedState,

            // Buffer state
            #[mesh(5)]
            pub dma_type: Option<SavedDmaType>,
            #[mesh(6)]
            pub command_buffer: Vec<u8>,

            // New states are added at the end of the struct to be compatible

            // There is an ATAPI packet command in progress, meaning specifically:
            //   The IDE controller has indicated that data is available to the guest
            //   (signal_atapi_data_ready), but the guest did not yet finish reading
            //   all the data associated with the command.
            #[mesh(7)]
            pub pending_packet_command: bool,
            #[mesh(8)]
            pub pending_software_reset: bool,
        }
    }

    impl AtapiDrive {
        pub fn save(&self) -> Result<SavedAtapiDriveState, SaveError> {
            let AtapiDriveState {
                regs:
                    Registers {
                        error,
                        features,
                        device_head,
                        byte_count_low,
                        byte_count_high,
                        lba_low,
                        sector_count,
                        device_control_reg,
                        status,
                    },
                pending_software_reset,
                pending_interrupt,
                error_pending,
                pending_packet_command,
                buffer,
            } = self.state();

            let scsi = self.scsi_disk.save()?.unwrap();

            let command_buffer = if let Some(buffer_state) = &self.state.buffer {
                self.command_buffer.buffer[buffer_state.range()]
                    .iter()
                    .map(|val| val.load(Ordering::Relaxed))
                    .collect()
            } else {
                Vec::new()
            };

            Ok(SavedAtapiDriveState {
                registers: SavedRegisterState {
                    error: error.into_bits(),
                    features: *features,
                    device_head: (*device_head).into(),
                    lba_low: *lba_low,
                    byte_count_low: *byte_count_low,
                    byte_count_high: *byte_count_high,
                    sector_count: *sector_count,
                    device_control_reg: device_control_reg.into_bits(),
                    status: status.into_bits(),
                },
                pending_interrupt: *pending_interrupt,
                error_pending: *error_pending,
                scsi,
                dma_type: match buffer {
                    Some(buffer_state) => buffer_state.dma_type.as_ref().map(|dma| match dma {
                        DmaType::Read => SavedDmaType::Read,
                        DmaType::Write => SavedDmaType::Write,
                    }),
                    None => None,
                },
                command_buffer,
                pending_packet_command: *pending_packet_command,
                pending_software_reset: *pending_software_reset,
            })
        }

        pub fn restore(&mut self, state: SavedAtapiDriveState) -> Result<(), RestoreError> {
            let SavedAtapiDriveState {
                registers:
                    SavedRegisterState {
                        error,
                        features,
                        device_head,
                        lba_low,
                        byte_count_low: lba_mid,
                        byte_count_high: lba_high,
                        sector_count,
                        device_control_reg,
                        status,
                    },
                pending_interrupt,
                error_pending,
                scsi,
                dma_type,
                command_buffer,
                pending_packet_command,
                pending_software_reset,
            } = state;

            self.scsi_disk.restore(&scsi)?;

            *self.state_mut() = AtapiDriveState {
                regs: Registers {
                    error: error.into(),
                    features,
                    device_head: device_head.into(),
                    byte_count_low: lba_mid,
                    byte_count_high: lba_high,
                    lba_low,
                    sector_count,
                    device_control_reg: DeviceControlReg::from_bits(device_control_reg),
                    status: Status::from_bits(status),
                },
                pending_software_reset,
                pending_interrupt,
                error_pending,
                pending_packet_command,
                buffer: if command_buffer.is_empty() {
                    None
                } else {
                    self.command_buffer.buffer[..command_buffer.len()]
                        .atomic_write(command_buffer.as_bytes());

                    Some(BufferState {
                        current_byte: 0,
                        total_bytes: command_buffer.len() as u32,
                        dma_type: dma_type.map(|dma| match dma {
                            SavedDmaType::Read => DmaType::Read,
                            SavedDmaType::Write => DmaType::Write,
                        }),
                    })
                },
            };

            Ok(())
        }
    }
}
