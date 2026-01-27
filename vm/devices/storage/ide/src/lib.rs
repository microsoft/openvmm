// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod drive;
mod protocol;

use crate::drive::save_restore::DriveSaveRestore;
use crate::protocol::BusMasterReg;
use crate::protocol::DeviceControlReg;
use crate::protocol::IdeCommand;
use crate::protocol::IdeConfigSpace;
use crate::protocol::Status;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use disk_backend::Disk;
use drive::DiskDrive;
use drive::DriveRegister;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use ide_resources::IdePath;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::HEADER_TYPE_00_SIZE;
use pci_core::spec::cfg_space::HeaderType00;
use protocol::BusMasterCommandReg;
use protocol::BusMasterStatusReg;
use scsi::CdbFlags;
use scsi::ScsiOp;
use scsi_core::AsyncScsiDisk;
use scsi_defs as scsi;
use std::fmt::Debug;
use std::mem::offset_of;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::task::Context;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use zerocopy::IntoBytes;

const PAGE_SIZE64: u64 = 4096;

open_enum! {
    pub enum IdeIoPort: u16 {
        PRI_ENLIGHTENED = 0x1E0,
        PRI_DATA = 0x1F0,
        PRI_ERROR_FEATURES = 0x1F1,
        PRI_SECTOR_COUNT = 0x1F2,
        PRI_SECTOR_NUM = 0x1F3,
        PRI_CYLINDER_LSB = 0x1F4,
        PRI_CYLINDER_MSB = 0x1F5,
        PRI_DEVICE_HEAD = 0x1F6,
        PRI_STATUS_CMD = 0x1F7,
        PRI_ALT_STATUS_DEVICE_CTL = 0x3F6,
        SEC_ENLIGHTENED = 0x160,
        SEC_DATA = 0x170,
        SEC_ERROR_FEATURES = 0x171,
        SEC_SECTOR_COUNT = 0x172,
        SEC_SECTOR_NUM = 0x173,
        SEC_CYLINDER_LSB = 0x174,
        SEC_CYLINDER_MSB = 0x175,
        SEC_DEVICE_HEAD = 0x176,
        SEC_STATUS_CMD = 0x177,
        SEC_ALT_STATUS_DEVICE_CTL = 0x376,
    }
}

enum Port {
    Data,
    Drive(DriveRegister),
    Enlightened,
    BusMaster(BusMasterReg),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
enum DmaType {
    /// Read from guest memory.
    Read,
    /// Write to guest memory.
    Write,
}

#[derive(Debug, Inspect)]
struct BusMasterState {
    #[inspect(hex)]
    cmd_status_reg: u32,
    #[inspect(hex)]
    port_addr_reg: u32,
    #[inspect(hex)]
    timing_reg: u32,
    #[inspect(hex)]
    secondary_timing_reg: u32,
    #[inspect(hex)]
    dma_ctl_reg: u32,
}

// bus master
const DEFAULT_BUS_MASTER_PORT_ADDR_REG: u32 = 0x0000_0001;
const DEFAULT_BUS_MASTER_CMD_STATUS_REG: u32 = 0x0280_0000;

impl BusMasterState {
    fn new() -> Self {
        Self {
            cmd_status_reg: DEFAULT_BUS_MASTER_CMD_STATUS_REG,
            port_addr_reg: DEFAULT_BUS_MASTER_PORT_ADDR_REG,
            // Enable IDE decode at startup, don't wait for the BIOS (this is
            // not actually configurable with our hardware).
            //
            // TODO: define a bitfield, hard wire these bits to 1 so that they
            // can't be cleared.
            timing_reg: 0x80008000,
            secondary_timing_reg: 0,
            dma_ctl_reg: 0,
        }
    }
}

#[derive(Debug, Default, Inspect)]
struct ChannelBusMasterState {
    command_reg: BusMasterCommandReg,
    status_reg: BusMasterStatusReg,
    #[inspect(hex)]
    desc_table_ptr: u32,
    dma_state: Option<DmaState>,
    dma_error: bool,
}

impl ChannelBusMasterState {
    fn dma_io_type(&self) -> DmaType {
        if self.command_reg.write() {
            DmaType::Write
        } else {
            DmaType::Read
        }
    }
}

/// PCI-based IDE controller
pub struct IdeDevice {
    /// IDE controllers can support up to two IDE channels (primary and secondary)
    /// with up to two devices (drives) per channel for a total of four IDE devices.
    channels: [Channel; 2],
    bus_master_state: BusMasterState,
    bus_master_pio_dynamic: Box<dyn ControlPortIoIntercept>,
}

impl InspectMut for IdeDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field_mut("primary", &mut self.channels[0])
            .field_mut("secondary", &mut self.channels[1])
            .field("bus_master_state", &self.bus_master_state);
    }
}

#[derive(Inspect, Debug)]
struct EnlightenedCdWrite {
    #[inspect(skip)]
    deferred: DeferredWrite,
    old_adapter_control_reg: u8,
    guest_address: u64,
    old_features_reg: u8,
    data_buffer: u32,
    skip_bytes_head: u16,
    byte_count: u32,
    block_count: u16,
    drive_index: usize,
}

#[derive(Inspect, Debug)]
struct EnlightenedHddWrite {
    #[inspect(skip)]
    deferred: DeferredWrite,
    old_adapter_control_reg: u8,
    guest_address: u64,
    drive_index: usize,
}

#[derive(Debug, Inspect)]
#[inspect(tag = "drive_type")]
enum EnlightenedWrite {
    Hard(#[inspect(rename = "write")] EnlightenedHddWrite),
    Optical(#[inspect(rename = "write")] EnlightenedCdWrite),
}

#[derive(Debug, Error)]
pub enum NewDeviceError {
    #[error("disk too large: {0} bytes")]
    DiskTooLarge(u64),
}

impl IdeDevice {
    /// Creates an IDE device from the provided channel drive configuration.
    pub fn new(
        guest_memory: GuestMemory,
        register_pio: &mut dyn RegisterPortIoIntercept,
        primary_channel_drives: [Option<DriveMedia>; 2],
        secondary_channel_drives: [Option<DriveMedia>; 2],
        primary_line_interrupt: LineInterrupt,
        secondary_line_interrupt: LineInterrupt,
    ) -> Result<Self, NewDeviceError> {
        let channels = [
            Channel::new(
                primary_channel_drives,
                ChannelType::Primary,
                primary_line_interrupt,
                guest_memory.clone(),
            )?,
            Channel::new(
                secondary_channel_drives,
                ChannelType::Secondary,
                secondary_line_interrupt,
                guest_memory,
            )?,
        ];

        Ok(Self {
            channels,
            bus_master_state: BusMasterState::new(),
            bus_master_pio_dynamic: register_pio.new_io_region("ide bus master", 16),
        })
    }

    fn parse_port(&self, io_port: u16) -> Option<(Port, usize)> {
        match IdeIoPort(io_port) {
            IdeIoPort::PRI_ENLIGHTENED => Some((Port::Enlightened, 0)),
            IdeIoPort::PRI_DATA => Some((Port::Data, 0)),
            IdeIoPort::PRI_ERROR_FEATURES => Some((Port::Drive(DriveRegister::ErrorFeatures), 0)),
            IdeIoPort::PRI_SECTOR_COUNT => Some((Port::Drive(DriveRegister::SectorCount), 0)),
            IdeIoPort::PRI_SECTOR_NUM => Some((Port::Drive(DriveRegister::LbaLow), 0)),
            IdeIoPort::PRI_CYLINDER_LSB => Some((Port::Drive(DriveRegister::LbaMid), 0)),
            IdeIoPort::PRI_CYLINDER_MSB => Some((Port::Drive(DriveRegister::LbaHigh), 0)),
            IdeIoPort::PRI_DEVICE_HEAD => Some((Port::Drive(DriveRegister::DeviceHead), 0)),
            IdeIoPort::PRI_STATUS_CMD => Some((Port::Drive(DriveRegister::StatusCmd), 0)),
            IdeIoPort::PRI_ALT_STATUS_DEVICE_CTL => {
                Some((Port::Drive(DriveRegister::AlternateStatusDeviceControl), 0))
            }
            IdeIoPort::SEC_ENLIGHTENED => Some((Port::Enlightened, 1)),
            IdeIoPort::SEC_DATA => Some((Port::Data, 1)),
            IdeIoPort::SEC_ERROR_FEATURES => Some((Port::Drive(DriveRegister::ErrorFeatures), 1)),
            IdeIoPort::SEC_SECTOR_COUNT => Some((Port::Drive(DriveRegister::SectorCount), 1)),
            IdeIoPort::SEC_SECTOR_NUM => Some((Port::Drive(DriveRegister::LbaLow), 1)),
            IdeIoPort::SEC_CYLINDER_LSB => Some((Port::Drive(DriveRegister::LbaMid), 1)),
            IdeIoPort::SEC_CYLINDER_MSB => Some((Port::Drive(DriveRegister::LbaHigh), 1)),
            IdeIoPort::SEC_DEVICE_HEAD => Some((Port::Drive(DriveRegister::DeviceHead), 1)),
            IdeIoPort::SEC_STATUS_CMD => Some((Port::Drive(DriveRegister::StatusCmd), 1)),
            IdeIoPort::SEC_ALT_STATUS_DEVICE_CTL => {
                Some((Port::Drive(DriveRegister::AlternateStatusDeviceControl), 1))
            }
            io_port
                if (IdeIoPort::PRI_ENLIGHTENED..=IdeIoPort::PRI_STATUS_CMD).contains(&io_port) =>
            {
                None
            }
            io_port
                if (IdeIoPort::SEC_ENLIGHTENED..=IdeIoPort::SEC_STATUS_CMD).contains(&io_port) =>
            {
                None
            }
            _ => {
                if self.bus_master_state.cmd_status_reg
                    & protocol::PCI_CONFIG_STATUS_IO_SPACE_ENABLE_MASK
                    == 0
                {
                    return None;
                }

                // If the port does not match any of the statically registered IO ports then
                // the port is part of the dynamically registered bus master range. The IDE
                // bus master range spans 16 ports with the first 8 corresponding to the primary
                // IDE channel.
                Some((
                    Port::BusMaster(BusMasterReg(io_port & 0x7)),
                    (io_port as usize & 0x8) >> 3,
                ))
            }
        }
    }
}

impl Channel {
    fn enlightened_port_write(
        &mut self,
        data: &[u8],
        bus_master_state: &BusMasterState,
    ) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if let Some(status) = self.current_drive_status() {
            if status.err() {
                tracelimit::warn_ratelimited!(
                    "drive is in error state, ignoring enlightened command",
                );
                return IoResult::Ok;
            } else if status.bsy() || status.drq() {
                tracelimit::warn_ratelimited!(
                    "command is already pending on this drive, ignoring enlightened command"
                );
                return IoResult::Ok;
            }
        }
        if self.enlightened_write.is_some() {
            tracelimit::error_ratelimited!("enlightened write while one is in progress, ignoring");
            return IoResult::Ok;
        }

        // Read the EnlightenedInt13Command packet directly from guest ram
        let addr = u32::from_ne_bytes(data.try_into().unwrap());

        let eint13_cmd = match self
            .guest_memory
            .read_plain::<protocol::EnlightenedInt13Command>(addr as u64)
        {
            Ok(cmd) => cmd,
            Err(err) => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to read enlightened IO command"
                );
                return IoResult::Ok;
            }
        };

        // Write out the drive-head register FIRST because that is used to
        // select the current drive.
        self.write_drive_register(
            DriveRegister::DeviceHead,
            eint13_cmd.device_head.into(),
            bus_master_state,
        );

        let result = if let Some(drive_type) = self.current_drive_type() {
            match drive_type {
                DriveType::Optical => {
                    self.enlightened_cd_command(addr.into(), eint13_cmd, bus_master_state)
                }
                DriveType::Hard => {
                    self.enlightened_hdd_command(addr.into(), eint13_cmd, bus_master_state)
                }
            }
        } else {
            tracelimit::warn_ratelimited!(
                eint13_cmd = ?eint13_cmd,
                drive_idx = self.state.current_drive_idx,
                "Enlightened IO command: No attached drive"
            );
            IoResult::Ok
        };

        self.post_drive_access(bus_master_state);
        result
    }

    fn enlightened_hdd_command(
        &mut self,
        guest_address: u64,
        eint13_cmd: protocol::EnlightenedInt13Command,
        bus_master_state: &BusMasterState,
    ) -> IoResult {
        let mut lba48 = eint13_cmd.lba_high as u64;
        lba48 <<= 32;
        lba48 |= eint13_cmd.lba_low as u64;

        tracing::trace!(
            command = ?eint13_cmd.command,
            lba = lba48,
            block_count = eint13_cmd.block_count,
            buffer = eint13_cmd.data_buffer,
            guest_address,
            "enlightened hdd command"
        );

        // Write out the PRD register for the bus master
        self.write_bus_master_reg(
            BusMasterReg::TABLE_PTR,
            eint13_cmd.data_buffer.as_bytes(),
            bus_master_state,
        )
        .unwrap();

        // Now that we know what the IDE command is, disambiguate between
        // 28-bit LBA and 48-bit LBA
        let cmd = eint13_cmd.command;
        if cmd == IdeCommand::READ_DMA_EXT || cmd == IdeCommand::WRITE_DMA_EXT {
            // 48-bit LBA, high 24 bits of logical block address
            self.write_drive_register(
                DriveRegister::LbaLow,
                (eint13_cmd.lba_low >> 24) as u8,
                bus_master_state,
            );

            self.write_drive_register(
                DriveRegister::LbaMid,
                eint13_cmd.lba_high as u8,
                bus_master_state,
            );

            self.write_drive_register(
                DriveRegister::LbaHigh,
                (eint13_cmd.lba_high >> 8) as u8,
                bus_master_state,
            );

            // 48-bit LBA, high 8 bits of sector count
            // Write the low-byte of the sector count
            self.write_drive_register(
                DriveRegister::SectorCount,
                (eint13_cmd.block_count >> 8) as u8,
                bus_master_state,
            );
        }

        // Write the low-byte of the sector count
        self.write_drive_register(
            DriveRegister::SectorCount,
            eint13_cmd.block_count as u8,
            bus_master_state,
        );

        // Finish writing the LBA low bytes to the FIFOs
        self.write_drive_register(
            DriveRegister::LbaLow,
            eint13_cmd.lba_low as u8,
            bus_master_state,
        );
        self.write_drive_register(
            DriveRegister::LbaMid,
            (eint13_cmd.lba_low >> 8) as u8,
            bus_master_state,
        );
        self.write_drive_register(
            DriveRegister::LbaHigh,
            (eint13_cmd.lba_low >> 16) as u8,
            bus_master_state,
        );

        // Make sure that the IDE channel will not cause an interrupt since this
        // enlightened operation is intended to be 100% synchronous.
        let old_adapter_control_reg = self.state.shadow_adapter_control_reg;
        self.write_drive_register(
            DriveRegister::AlternateStatusDeviceControl,
            DeviceControlReg::new()
                .with_interrupt_mask(true)
                .into_bits(),
            bus_master_state,
        );

        // Start the dma engine.
        let mut bus_master_flags = BusMasterCommandReg::new().with_start(true);
        if cmd == IdeCommand::READ_DMA_EXT
            || cmd == IdeCommand::READ_DMA
            || cmd == IdeCommand::READ_DMA_ALT
        {
            // set rw flag to 1 to inticate that bus master is performing a read
            bus_master_flags.set_write(true);
        }

        self.write_bus_master_reg(
            BusMasterReg::COMMAND,
            &[bus_master_flags.into_bits() as u8],
            bus_master_state,
        )
        .unwrap();

        // Start the IDE command
        self.write_drive_register(DriveRegister::StatusCmd, cmd.0, bus_master_state);

        // Defer the write.
        let (write, token) = defer_write();
        self.enlightened_write = Some(EnlightenedWrite::Hard(EnlightenedHddWrite {
            deferred: write,
            old_adapter_control_reg,
            guest_address,
            drive_index: self.state.current_drive_idx,
        }));

        tracing::trace!(enlightened_write = ?self.enlightened_write, "enlightened_hdd_command");
        if let Some(status) = self.current_drive_status() {
            if status.drq() {
                tracelimit::warn_ratelimited!(
                    "command is waiting for data read from guest or data write to guest"
                );
                return IoResult::Ok;
            }
        }
        IoResult::Defer(token)
    }

    fn enlightened_cd_command(
        &mut self,
        guest_address: u64,
        eint13_cmd: protocol::EnlightenedInt13Command,
        bus_master_state: &BusMasterState,
    ) -> IoResult {
        tracing::trace!(
            guest_address,
            command = ?eint13_cmd,
            "enlightened cd command"
        );

        // Save the old Precompensation byte because we must NOT use traditional DMA
        // with this command. Just read the bytes into the track cache so we can use
        // WriteRamBytes to move them to the guest.
        let old_features_reg = self.state.shadow_features_reg;
        self.write_drive_register(DriveRegister::ErrorFeatures, 0, bus_master_state);

        // Make sure that any code path through the IDE completion and error code does NOT
        // generate an interrupt. This enlightenment is intended to operate completely
        // synchronously.
        let old_adapter_control_reg = self.state.shadow_adapter_control_reg;
        self.write_drive_register(
            DriveRegister::AlternateStatusDeviceControl,
            DeviceControlReg::new()
                .with_interrupt_mask(true)
                .into_bits(),
            bus_master_state,
        );

        // Start the Atapi packet command
        self.write_drive_register(
            DriveRegister::StatusCmd,
            IdeCommand::PACKET_COMMAND.0,
            bus_master_state,
        );

        // Construct the SCSI command packet in the single sector buffer that
        // we're about to dispatch.
        let cdb = scsi::Cdb10 {
            operation_code: ScsiOp::READ,
            flags: CdbFlags::new(),
            logical_block: eint13_cmd.lba_low.into(),
            reserved2: 0,
            transfer_blocks: eint13_cmd.block_count.into(),
            control: 0,
        };

        // Assume the device is configured for 12-byte commands.
        let mut command = [0; 12];
        command[..cdb.as_bytes().len()].copy_from_slice(cdb.as_bytes());

        // Spawn the IO read which eventually puts the data into the track cache
        // Wait synchronously for the command to complete reading the data into the track cache.
        self.write_drive_data(command.as_bytes(), bus_master_state);

        // Defer the write.
        let (write, token) = defer_write();
        self.enlightened_write = Some(EnlightenedWrite::Optical(EnlightenedCdWrite {
            deferred: write,
            old_adapter_control_reg,
            guest_address,
            old_features_reg,
            data_buffer: eint13_cmd.data_buffer,
            skip_bytes_head: eint13_cmd.skip_bytes_head,
            byte_count: eint13_cmd.byte_count,
            block_count: eint13_cmd.block_count,
            drive_index: self.state.current_drive_idx,
        }));

        tracing::trace!(enlightened_write = ?self.enlightened_write, "enlightened_cd_command");
        if let Some(status) = self.current_drive_status() {
            if status.drq() {
                tracelimit::warn_ratelimited!(
                    "command is waiting for data read from guest or data write to guest"
                );
                return IoResult::Ok;
            }
        }
        IoResult::Defer(token)
    }

    fn complete_enlightened_hdd_write(
        &mut self,
        write: EnlightenedHddWrite,
        bus_master_state: &BusMasterState,
    ) {
        // Stop the dma engine (probably stopped already, but this mimics the BIOS
        // code which this enlightenment replaces).
        self.write_bus_master_reg(BusMasterReg::COMMAND, &[0], bus_master_state)
            .unwrap();

        // Clear the pending interrupt and read status.
        let status = self.read_drive_register(DriveRegister::StatusCmd, bus_master_state);
        let status = Status::from_bits(status);

        if status.err() {
            // If there was an error, copy back the status into the enlightened INT13 command in
            // the guest so that the guest can check it.
            if let Err(err) = self.guest_memory.write_at(
                write.guest_address
                    + offset_of!(protocol::EnlightenedInt13Command, result_status) as u64,
                &[status.into_bits()],
            ) {
                tracelimit::error_ratelimited!(
                    ?status,
                    error = &err as &dyn std::error::Error,
                    "failed to write eint13 status back"
                );
            }
        }

        // Possibly re-enable IDE channel interrupts for the next operation
        self.write_drive_register(
            DriveRegister::AlternateStatusDeviceControl,
            write.old_adapter_control_reg,
            bus_master_state,
        );

        write.deferred.complete();
    }

    fn complete_enlightened_cd_write(
        &mut self,
        write: EnlightenedCdWrite,
        bus_master_state: &BusMasterState,
    ) {
        // Clear the pending interrupt and read status.
        let status = self.read_drive_register(DriveRegister::StatusCmd, bus_master_state);
        let status = Status::from_bits(status);

        if status.err() {
            // If there was an error, copy back the status into the enlightened INT13 command in
            // the guest so that the guest can check it.
            if let Err(err) = self.guest_memory.write_at(
                write.guest_address
                    + offset_of!(protocol::EnlightenedInt13Command, result_status) as u64,
                &[status.into_bits()],
            ) {
                tracelimit::error_ratelimited!(
                    ?status,
                    error = &err as &dyn std::error::Error,
                    "failed to write eint13 status back"
                );
            }
        } else {
            let mut remaining =
                (write.block_count as u32 * protocol::CD_DRIVE_SECTOR_BYTES) as usize;

            // Skip over the unused portion of the result.
            let skip = (write.skip_bytes_head as usize).min(remaining);
            remaining -= skip;
            self.skip_drive_data(skip, bus_master_state);

            // Read the requested part of the result.
            let byte_count = (write.byte_count as usize).min(remaining);
            remaining -= byte_count;

            let mut copied = 0;
            while copied < byte_count {
                let mut buf = [0; 512];
                let len = (byte_count - copied).min(buf.len());
                let buf = &mut buf[..len];
                self.read_drive_data(buf, bus_master_state);
                if let Err(err) = self
                    .guest_memory
                    .write_at((write.data_buffer as u64).wrapping_add(copied as u64), buf)
                {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "failed to write enlightened result to guest memory"
                    );
                }
                copied += buf.len();
            }

            // Read any remaining data to prepare the device for the next command.
            self.skip_drive_data(remaining, bus_master_state);
        }

        // Restore the precompensation setting which existed prior to this read
        self.write_drive_register(
            DriveRegister::ErrorFeatures,
            write.old_features_reg,
            bus_master_state,
        );

        // Possibly re-enable IDE channel interrupts for the next operation
        self.write_drive_register(
            DriveRegister::AlternateStatusDeviceControl,
            write.old_adapter_control_reg,
            bus_master_state,
        );

        tracing::trace!("enlightened cd write completed");
        write.deferred.complete();
    }

    fn gpa_to_gpn(gpa: u64) -> u64 {
        gpa / PAGE_SIZE64
    }

    fn perform_dma_memory_phase(&mut self) {
        let Some(drive) = &mut self.drives[self.state.current_drive_idx] else {
            return;
        };

        if self.bus_master_state.dma_error {
            if drive.handle_read_dma_descriptor_error() {
                self.bus_master_state.dma_error = false;
            }
            return;
        }

        let (dma_type, mut dma_avail) = match drive.dma_request() {
            Some((dma_type, avail)) if *dma_type == self.bus_master_state.dma_io_type() => {
                (Some(*dma_type), avail as u32)
            }
            _ => {
                // No active, appropriate DMA buffer.
                return;
            }
        };

        let Some(dma) = &mut self.bus_master_state.dma_state else {
            return;
        };

        while dma_avail > 0 {
            // If the number of bytes left in the transfer is zero, we need to read the next Dma descriptor
            if dma.transfer_bytes_left == 0 {
                assert!(!dma.transfer_complete);

                // The descriptor table pointer points to a table of Dma
                // scatter/gather descriptors built by the operating system.
                // This table tells us where the place the incoming data for
                // reads or where to get the outgoing data for writes. The
                // last valid table entry will have the high bit of the
                // EndOfTable field set.
                let descriptor_addr: u64 = self
                    .bus_master_state
                    .desc_table_ptr
                    .wrapping_add(8 * (dma.descriptor_idx as u32))
                    .into();

                let cur_desc_table_entry = match self
                    .guest_memory
                    .read_plain::<protocol::BusMasterDmaDesc>(descriptor_addr)
                {
                    Ok(cur_desc_table_entry) => cur_desc_table_entry,
                    Err(err) => {
                        self.bus_master_state.dma_state = None;
                        if !drive.handle_read_dma_descriptor_error() {
                            self.bus_master_state.dma_error = true;
                        }
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "dma descriptor read error"
                        );
                        return;
                    }
                };

                dma.transfer_bytes_left = cur_desc_table_entry.byte_count.into();
                // A zero byte length implies 64Kb
                if cur_desc_table_entry.byte_count == 0 {
                    dma.transfer_bytes_left = 0x10000;
                }

                // Check that the every page starting from the base address is within
                // the guest's physical address space.
                // This is a sanity check, the guest should not be able to program the DMA
                // controller with an invalid page access.

                let end_gpa = cur_desc_table_entry
                    .mem_physical_base
                    .checked_add(dma.transfer_bytes_left);

                let r = if let Some(end_gpa) = end_gpa {
                    let start_gpn = Self::gpa_to_gpn(cur_desc_table_entry.mem_physical_base.into());
                    let end_gpn = Self::gpa_to_gpn(end_gpa.into());
                    let gpns: Vec<u64> = (start_gpn..end_gpn).collect();

                    let paged_range = PagedRange::new(0, gpns.len() * PAGE_SIZE64 as usize, &gpns).unwrap();
                    Some(match dma_type.unwrap() {
                        DmaType::Read => {
                            self.guest_memory
                                .probe_gpn_readable_range(&paged_range)
                        },
                        DmaType::Write => {
                            self.guest_memory
                                .probe_gpn_writable_range(&paged_range)
                        },
                    })
                } else {
                    None
                };

                if r.is_some_and(|res| res.is_err()) || end_gpa.is_none() {
                        // If there is an error and there is no other IO in parallel,
                        // we need to stop the current DMA transfer and set the error bit
                        // in the Bus Master Status register.
                        self.bus_master_state.dma_state = None;
                        if !drive.handle_read_dma_descriptor_error() {
                            self.bus_master_state.dma_error = true;
                        }

                        tracelimit::error_ratelimited!(
                            "dma base address out-of-range error"
                        );
                        return;
                }

                dma.transfer_base_addr = cur_desc_table_entry.mem_physical_base.into();
                dma.transfer_complete = (cur_desc_table_entry.end_of_table & 0x80) != 0;

                // Increment to the next descriptor.
                dma.descriptor_idx += 1;
                if dma.transfer_complete {
                    dma.descriptor_idx = 0;
                }
            }

            // Transfer byte count is limited by the transfer size and the number of bytes in our IDE buffer.
            let bytes_to_transfer = dma_avail.min(dma.transfer_bytes_left);

            assert!(bytes_to_transfer != 0);

            drive.dma_transfer(
                &self.guest_memory,
                dma.transfer_base_addr,
                bytes_to_transfer as usize,
            );

            dma_avail -= bytes_to_transfer;
            dma.transfer_base_addr += bytes_to_transfer as u64;
            dma.transfer_bytes_left -= bytes_to_transfer;
            if dma.transfer_bytes_left == 0 && dma.transfer_complete {
                if dma_avail > 0 {
                    // If descriptor table ended before buffer was exhausted, advance buffer to end to indicate completion of operation
                    drive.set_prd_exhausted();
                    drive.dma_advance_buffer(dma_avail as usize);
                }
                tracing::trace!("dma transfer is complete");
                self.bus_master_state.dma_state = None;
                break;
            }
        }
    }
}

impl ChangeDeviceState for IdeDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.bus_master_pio_dynamic.unmap();
        self.bus_master_state = BusMasterState::new();
        for channel in &mut self.channels {
            channel.reset();
        }
    }
}

impl ChipsetDevice for IdeDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for IdeDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        for channel in &mut self.channels {
            channel.poll_device(cx, &self.bus_master_state);
        }
    }
}

impl PortIoIntercept for IdeDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        match self.parse_port(io_port) {
            Some((port, index)) => match port {
                Port::Data => {
                    self.channels[index].read_drive_data(data, &self.bus_master_state);
                    IoResult::Ok
                }
                Port::Drive(register) => {
                    data[0] =
                        self.channels[index].read_drive_register(register, &self.bus_master_state);
                    IoResult::Ok
                }
                Port::Enlightened => IoResult::Err(IoError::InvalidRegister),
                Port::BusMaster(offset) => self.channels[index].read_bus_master_reg(offset, data),
            },
            None => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        match self.parse_port(io_port) {
            Some((port, index)) => match port {
                Port::Data => {
                    self.channels[index].write_drive_data(data, &self.bus_master_state);
                    IoResult::Ok
                }
                Port::Drive(register) => {
                    self.channels[index].write_drive_register(
                        register,
                        data[0],
                        &self.bus_master_state,
                    );
                    IoResult::Ok
                }
                Port::Enlightened => {
                    self.channels[index].enlightened_port_write(data, &self.bus_master_state)
                }
                Port::BusMaster(offset) => {
                    self.channels[index].write_bus_master_reg(offset, data, &self.bus_master_state)
                }
            },
            None => IoResult::Err(IoError::InvalidRegister),
        }
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            (
                "ide primary channel",
                IdeIoPort::PRI_ENLIGHTENED.0..=IdeIoPort::PRI_STATUS_CMD.0,
            ),
            (
                "ide primary channel control",
                IdeIoPort::PRI_ALT_STATUS_DEVICE_CTL.0..=IdeIoPort::PRI_ALT_STATUS_DEVICE_CTL.0,
            ),
            (
                "ide secondary channel",
                IdeIoPort::SEC_ENLIGHTENED.0..=IdeIoPort::SEC_STATUS_CMD.0,
            ),
            (
                "ide secondary channel control",
                IdeIoPort::SEC_ALT_STATUS_DEVICE_CTL.0..=IdeIoPort::SEC_ALT_STATUS_DEVICE_CTL.0,
            ),
        ]
    }
}

impl PciConfigSpace for IdeDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        *value = if offset < HEADER_TYPE_00_SIZE {
            match HeaderType00(offset) {
                HeaderType00::DEVICE_VENDOR => protocol::BX_PCI_ISA_BRIDGE_IDE_IDREG_VALUE,
                HeaderType00::STATUS_COMMAND => self.bus_master_state.cmd_status_reg,
                HeaderType00::CLASS_REVISION => protocol::BX_PCI_IDE_CLASS_WORD,
                HeaderType00::BAR4 => self.bus_master_state.port_addr_reg,
                offset => {
                    tracing::debug!(?offset, "undefined type00 header read");
                    0
                }
            }
        } else {
            match IdeConfigSpace(offset) {
                IdeConfigSpace::PRIMARY_TIMING_REG_ADDR => self.bus_master_state.timing_reg,
                IdeConfigSpace::SECONDARY_TIMING_REG_ADDR => {
                    self.bus_master_state.secondary_timing_reg
                }
                IdeConfigSpace::UDMA_CTL_REG_ADDR => self.bus_master_state.dma_ctl_reg,
                IdeConfigSpace::MANUFACTURE_ID_REG_ADDR => {
                    // This is a private versioning register, and no one is supposed to use it.
                    // But a real Triton chipset returns this value...
                    0x00000F30
                }
                offset => {
                    // Allow reads from undefined areas.
                    tracing::trace!(?offset, "undefined ide pci config space read");
                    return IoResult::Err(IoError::InvalidRegister);
                }
            }
        };

        tracing::trace!(?offset, value, "ide pci config space read");
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        if offset < HEADER_TYPE_00_SIZE {
            let offset = HeaderType00(offset);
            tracing::trace!(?offset, value, "ide pci config space write");

            const BUS_MASTER_IO_ENABLE_MASK: u32 = Command::new()
                .with_pio_enabled(true)
                .with_bus_master(true)
                .into_bits() as u32;

            match offset {
                HeaderType00::STATUS_COMMAND => {
                    // Several bits are used to reset status bits when written as 1s.
                    self.bus_master_state.cmd_status_reg &= !(0x38000000 & value);
                    // Only allow writes to two bits (0 and 2). All other bits are read-only.
                    self.bus_master_state.cmd_status_reg &= !BUS_MASTER_IO_ENABLE_MASK;

                    self.bus_master_state.cmd_status_reg |= value & BUS_MASTER_IO_ENABLE_MASK;

                    // Is the io address space enabled for bus mastering and is bus mastering enabled?
                    if (self.bus_master_state.cmd_status_reg
                        & protocol::CFCS_BUS_MASTER_IO_ENABLE_MASK)
                        != protocol::CFCS_BUS_MASTER_IO_ENABLE_MASK
                    {
                        self.bus_master_pio_dynamic.unmap();
                        tracing::trace!("disabling bus master io range");
                    } else {
                        // Install the callbacks for the current bus primary I/O port range (which
                        // is dynamically configurable through PCI config registers)
                        let first_port = (self.bus_master_state.port_addr_reg as u16) & 0xFFF0;
                        tracing::trace!(?first_port, "enabling bus master range");

                        // Change the io range for the bus master registers. Some of the bus master
                        // registers can be cached on writes.
                        self.bus_master_pio_dynamic.map(first_port);
                    }
                }
                HeaderType00::BAR4 => {
                    // Only allow writes to bits 4 to 15
                    self.bus_master_state.port_addr_reg =
                        (value & 0x0000FFF0) | DEFAULT_BUS_MASTER_PORT_ADDR_REG;
                }
                _ => tracing::debug!(?offset, "undefined type00 header write"),
            }
        } else {
            let offset = IdeConfigSpace(offset);
            tracing::trace!(?offset, value, "ide pci config space write");

            match offset {
                IdeConfigSpace::PRIMARY_TIMING_REG_ADDR => self.bus_master_state.timing_reg = value,
                IdeConfigSpace::SECONDARY_TIMING_REG_ADDR => {
                    self.bus_master_state.secondary_timing_reg = value
                }
                IdeConfigSpace::UDMA_CTL_REG_ADDR => self.bus_master_state.dma_ctl_reg = value,
                _ => tracing::trace!(?offset, "undefined ide pci config space write"),
            }
        }

        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        Some((0, 7, 1)) // as per PIIX4 spec
    }
}

/// IDE Channel
enum ChannelType {
    Primary,
    Secondary,
}

#[derive(Inspect)]
#[inspect(tag = "drive_type")]
pub enum DriveMedia {
    HardDrive(#[inspect(rename = "backend")] Disk),
    OpticalDrive(#[inspect(rename = "backend")] Arc<dyn AsyncScsiDisk>),
}

impl DriveMedia {
    pub fn hard_disk(disk: Disk) -> Self {
        DriveMedia::HardDrive(disk)
    }

    pub fn optical_disk(scsi_disk: Arc<dyn AsyncScsiDisk>) -> Self {
        DriveMedia::OpticalDrive(scsi_disk)
    }
}

#[derive(Debug, Default, Inspect)]
struct ChannelState {
    current_drive_idx: usize,
    shadow_adapter_control_reg: u8,
    shadow_features_reg: u8,
}

#[derive(InspectMut)]
struct Channel {
    #[inspect(mut, with = "inspect_drives")]
    drives: [Option<DiskDrive>; 2],
    interrupt: LineInterrupt,
    state: ChannelState,
    bus_master_state: ChannelBusMasterState,
    enlightened_write: Option<EnlightenedWrite>,
    guest_memory: GuestMemory,
    #[inspect(skip)]
    channel: u8,
}

fn inspect_drives(drives: &mut [Option<DiskDrive>]) -> impl '_ + InspectMut {
    inspect::adhoc_mut(|req| {
        let mut resp = req.respond();
        for (i, drive) in drives.iter_mut().enumerate() {
            resp.field_mut(&i.to_string(), drive);
        }
    })
}

impl Channel {
    fn new(
        channel_drives: [Option<DriveMedia>; 2],
        channel_type: ChannelType,
        interrupt: LineInterrupt,
        guest_memory: GuestMemory,
    ) -> Result<Self, NewDeviceError> {
        let [primary_media, secondary_media] = channel_drives;

        let channel_number = match channel_type {
            ChannelType::Primary => 0,
            ChannelType::Secondary => 1,
        };

        Ok(Self {
            drives: [
                primary_media
                    .map(|media| {
                        DiskDrive::new(
                            media,
                            IdePath {
                                channel: channel_number,
                                drive: 0,
                            },
                        )
                    })
                    .transpose()?,
                secondary_media
                    .map(|media| {
                        DiskDrive::new(
                            media,
                            IdePath {
                                channel: channel_number,
                                drive: 1,
                            },
                        )
                    })
                    .transpose()?,
            ],
            interrupt,
            state: ChannelState::default(),
            bus_master_state: ChannelBusMasterState::default(),
            enlightened_write: None,
            guest_memory,
            channel: channel_number,
        })
    }

    fn reset(&mut self) {
        tracelimit::info_ratelimited!(channel = self.channel, "channel reset");
        self.interrupt.set_level(false);
        self.state = ChannelState::default();
        self.bus_master_state = ChannelBusMasterState::default();
        for drive in self.drives.iter_mut().flatten() {
            drive.reset();
        }
    }

    fn poll_device(&mut self, cx: &mut Context<'_>, bus_master_state: &BusMasterState) {
        for drive in self.drives.iter_mut().flatten() {
            drive.poll_device(cx);
        }
        self.post_drive_access(bus_master_state);
    }

    fn current_drive_status(&mut self) -> Option<Status> {
        if let Some(drive) = &mut self.drives[self.state.current_drive_idx] {
            let status = drive.read_register(DriveRegister::AlternateStatusDeviceControl);
            Some(Status::from_bits(status))
        } else {
            None
        }
    }

    fn drive_status(&mut self, drive_index: usize) -> Status {
        // Reading the values from Status register without clearing any bits
        assert!(self.drives[drive_index].is_some());
        let status = self.drives[drive_index]
            .as_mut()
            .unwrap()
            .read_register(DriveRegister::AlternateStatusDeviceControl);
        Status::from_bits(status)
    }

    fn current_drive_type(&self) -> Option<DriveType> {
        self.drives[self.state.current_drive_idx]
            .as_ref()
            .map(|drive| drive.drive_type())
    }

    fn drive_type(&mut self, drive_index: usize) -> DriveType {
        assert!(self.drives[drive_index].is_some());
        self.drives[drive_index]
            .as_ref()
            .map(|drive| drive.drive_type())
            .unwrap()
    }

    fn post_drive_access(&mut self, bus_master_state: &BusMasterState) {
        // DMA first, since this may affect the other operations.
        self.perform_dma_memory_phase();

        // Enlightened writes.
        if let Some(enlightened_write) = &self.enlightened_write {
            let drive_index = match enlightened_write {
                EnlightenedWrite::Hard(enlightened_hdd_write) => enlightened_hdd_write.drive_index,
                EnlightenedWrite::Optical(enlightened_cd_write) => enlightened_cd_write.drive_index,
            };

            let status = self.drive_status(drive_index);
            let completed = match self.drive_type(drive_index) {
                DriveType::Hard => !(status.bsy() || status.drq()),
                DriveType::Optical => status.drdy(),
            };
            if completed {
                // The command is done.
                let write = self.enlightened_write.take().unwrap();
                match write {
                    EnlightenedWrite::Hard(write) => {
                        self.complete_enlightened_hdd_write(write, bus_master_state)
                    }
                    EnlightenedWrite::Optical(write) => {
                        self.complete_enlightened_cd_write(write, bus_master_state)
                    }
                }
            }
        }

        // Update interrupt state.
        let interrupt = self
            .drives
            .iter()
            .flatten()
            .any(|drive| drive.interrupt_pending());
        if interrupt {
            tracing::trace!(channel = self.channel, interrupt, "post_drive_access");
            self.bus_master_state.status_reg.set_interrupt(true);
        }
        self.interrupt.set_level(interrupt);
    }

    /// Returns Ok(true) if an interrupt should be delivered. The whole result
    /// of this should be passed to `io_port_completion`
    fn read_drive_register(
        &mut self,
        port: DriveRegister,
        bus_master_state: &BusMasterState,
    ) -> u8 {
        // Call the selected drive, but fall back to drive 0 if drive 1 is not present.
        let mut drive = self.drives[self.state.current_drive_idx].as_mut();
        if drive.is_none() {
            drive = self.drives[0].as_mut();
        }

        let data = if let Some(drive) = drive {
            // Fill with zeroes if the drive doesn't write it (which can happen for some registers if
            // the device is not selected).
            drive.read_register(port)
        } else {
            // Returns 0x7f if neither device is present. This is align with ATA-5 "Devices shall not have a pull-up resistor on DD7"
            // Note: Legacy implementation returns 0xff in this case.
            0x7f
        };

        tracing::trace!(?port, ?data, channel = self.channel, "io port read");
        self.post_drive_access(bus_master_state);
        data
    }

    /// Returns Ok(true) if an interrupt should be delivered. The whole result
    /// of this should be passed to `io_port_completion` except in the case of
    /// the enlightened port path.
    fn write_drive_register(
        &mut self,
        port: DriveRegister,
        data: u8,
        bus_master_state: &BusMasterState,
    ) {
        tracing::trace!(?port, ?data, channel = self.channel, "io port write");

        match port {
            DriveRegister::DeviceHead => {
                // Shadow the device bit for use in the DMA engine.
                self.state.current_drive_idx = ((data >> 4) & 1) as usize;
            }
            DriveRegister::AlternateStatusDeviceControl => {
                // Save this for restoring in the enlightened path.
                self.state.shadow_adapter_control_reg = data;
                let v = DeviceControlReg::from_bits_truncate(data);
                if v.reset() && (self.drives[0].is_some() || self.drives[1].is_some()) {
                    self.state = ChannelState::default();
                }
            }
            DriveRegister::ErrorFeatures => {
                // Save this for restoring in the enlightened path.
                self.state.shadow_features_reg = data;
            }
            _ => {}
        }

        // Call both drives.
        if let Some(drive) = &mut self.drives[1] {
            drive.write_register(port, data);
        }
        if let Some(drive) = &mut self.drives[0] {
            drive.write_register(port, data);
        }

        self.post_drive_access(bus_master_state);
    }

    fn read_drive_data(&mut self, data: &mut [u8], bus_master_state: &BusMasterState) {
        // Call the selected drive, but fall back to drive 0 if drive 1 is not present.
        let mut drive = self.drives[self.state.current_drive_idx].as_mut();
        if drive.is_none() {
            drive = self.drives[0].as_mut();
        }

        data.fill(0xff);
        // DD7 must be low to conform to the ATA spec.
        data[0] = 0x7f;

        if let Some(drive) = drive {
            drive.pio_read(data);
        };

        self.post_drive_access(bus_master_state);
    }

    fn skip_drive_data(&mut self, mut len: usize, bus_master_state: &BusMasterState) {
        let mut buf = [0; 512];
        while len > 0 {
            let this_len = len.min(buf.len());
            let buf = &mut buf[..this_len];
            self.read_drive_data(buf, bus_master_state);
            len -= buf.len();
        }
    }

    fn write_drive_data(&mut self, data: &[u8], bus_master_state: &BusMasterState) {
        if let Some(drive) = &mut self.drives[0] {
            drive.pio_write(data);
        }
        if let Some(drive) = &mut self.drives[1] {
            drive.pio_write(data);
        }
        self.post_drive_access(bus_master_state);
    }

    fn read_bus_master_reg(&mut self, bus_master_reg: BusMasterReg, data: &mut [u8]) -> IoResult {
        let data_len = data.len();
        match bus_master_reg {
            BusMasterReg::COMMAND => match data_len {
                1 | 2 => data.copy_from_slice(
                    &self.bus_master_state.command_reg.into_bits().to_ne_bytes()[..data_len],
                ),
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            },
            BusMasterReg::STATUS => {
                let mut status = self.bus_master_state.status_reg;

                if self.bus_master_state.dma_state.is_some() {
                    status.set_active(true);
                }

                match data_len {
                    1 | 2 => data.copy_from_slice(&status.into_bits().to_ne_bytes()[..data_len]),
                    _ => return IoResult::Err(IoError::InvalidAccessSize),
                }
            }
            BusMasterReg::TABLE_PTR => match data_len {
                2 | 4 => data.copy_from_slice(
                    &self.bus_master_state.desc_table_ptr.to_ne_bytes()[..data_len],
                ),
                _ => return IoResult::Err(IoError::InvalidAccessSize),
            },
            BusMasterReg::TABLE_PTR2 => {
                if data_len == 2 {
                    data.copy_from_slice(&self.bus_master_state.desc_table_ptr.to_ne_bytes()[2..4]);
                } else {
                    return IoResult::Err(IoError::InvalidAccessSize);
                }
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        tracing::trace!(?bus_master_reg, ?data, "bus master register read");
        IoResult::Ok
    }

    fn write_bus_master_reg(
        &mut self,
        bus_master_reg: BusMasterReg,
        data: &[u8],
        bus_master_state: &BusMasterState,
    ) -> IoResult {
        let value: u64 = match data.len() {
            1 => u8::from_ne_bytes(data.as_bytes().try_into().unwrap()).into(),
            2 => u16::from_ne_bytes(data.as_bytes().try_into().unwrap()).into(),
            4 => u32::from_ne_bytes(data.as_bytes().try_into().unwrap()).into(),
            _ => return IoResult::Err(IoError::InvalidAccessSize),
        };

        tracing::trace!(?bus_master_reg, value, "bus master register write");

        match bus_master_reg {
            BusMasterReg::COMMAND => {
                // A write to this register will begin/end a dma transfer
                // and control its direction (write vs. read).
                if data.len() > 2 {
                    return IoResult::Err(IoError::InvalidAccessSize);
                }

                let old_value = self.bus_master_state.command_reg;
                // TODO: make sure all bits that should be preserved are defined.
                let mut new_value = BusMasterCommandReg::from_bits_truncate(value as u32);

                // The read/write bit is marked as read-only when dma is active.
                if old_value.start() {
                    // Set the new value of the read/write flag to match the
                    // existing value, regardless of the new value of the flag.
                    new_value.set_write(old_value.write());
                    if !new_value.start() {
                        self.bus_master_state.dma_state = None
                    }
                } else if new_value.start() {
                    self.bus_master_state.dma_state = Some(Default::default());
                };

                self.bus_master_state.command_reg = new_value;
            }
            BusMasterReg::STATUS => {
                if data.len() > 2 {
                    return IoResult::Err(IoError::InvalidAccessSize);
                }

                let value = BusMasterStatusReg::from_bits_truncate(value as u32);
                let old_value = self.bus_master_state.status_reg;
                let mut new_value = old_value.with_settable(value.settable());

                // These bits are reset if a one is written
                if value.interrupt() {
                    new_value.set_interrupt(false);
                }
                if value.dma_error() {
                    new_value.set_dma_error(false);
                }

                tracing::trace!(?old_value, ?new_value, "set bus master status");
                self.bus_master_state.status_reg = new_value;
            }
            BusMasterReg::TABLE_PTR => {
                if data.len() < 2 {
                    return IoResult::Err(IoError::InvalidAccessSize);
                }

                if data.len() == 4 {
                    // Writing the whole 32-bits pointer with the lowest 2 bits zeroed-out.
                    self.bus_master_state.desc_table_ptr = value as u32 & 0xffff_fffc;
                } else {
                    // Writing the low 16-bits of the 32-bits pointer, with the
                    // lowest 2 bits zeroed-out
                    self.bus_master_state.desc_table_ptr = (self.bus_master_state.desc_table_ptr
                        & 0xffff_0000)
                        | (value as u32 & 0x0000_fffc);
                }
            }
            BusMasterReg::TABLE_PTR2 => {
                // Documentation doesn't mention this offset. Apparently OS/2 writes to this port.
                // Port not written to in recent Windows boot.
                self.bus_master_state.desc_table_ptr = (self.bus_master_state.desc_table_ptr
                    & 0xffff)
                    | ((value as u32 & 0xffff) << 16);
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        self.post_drive_access(bus_master_state);
        IoResult::Ok
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum DriveType {
    Hard,
    Optical,
}

#[derive(Debug, Default, Inspect)]
struct DmaState {
    descriptor_idx: u8,
    transfer_complete: bool,
    transfer_bytes_left: u32,
    transfer_base_addr: u64,
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use crate::drive::save_restore::state::SavedDriveState;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.controller")]
        pub struct SavedBusMasterState {
            #[mesh(1)]
            pub cmd_status_reg: u32,
            #[mesh(2)]
            pub port_addr_reg: u32,
            #[mesh(3)]
            pub timing_reg: u32,
            #[mesh(4)]
            pub secondary_timing_reg: u32,
            #[mesh(5)]
            pub dma_ctl_reg: u32,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "storage.ide.controller")]
        pub struct SavedState {
            #[mesh(1)]
            pub bus_master: SavedBusMasterState,
            #[mesh(2)]
            pub channel0: SavedChannelState,
            #[mesh(3)]
            pub channel1: SavedChannelState,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.controller")]
        pub struct SavedDmaState {
            #[mesh(1)]
            pub descriptor_idx: u8,
            #[mesh(2)]
            pub transfer_complete: bool,
            #[mesh(3)]
            pub transfer_bytes_left: u32,
            #[mesh(4)]
            pub transfer_base_addr: u64,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.controller")]
        pub struct SavedChannelBusMasterState {
            #[mesh(1)]
            pub command_reg: u32,
            #[mesh(2)]
            pub status_reg: u32,
            #[mesh(3)]
            pub desc_table_ptr: u32,
            #[mesh(4)]
            pub dma_state: Option<SavedDmaState>,
            #[mesh(5)]
            pub dma_error: bool,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.controller")]
        pub struct SavedChannelState {
            #[mesh(1)]
            pub current_drive_idx: u8,
            #[mesh(2)]
            pub shadow_adapter_control_reg: u8,
            #[mesh(3)]
            pub shadow_features_reg: u8,
            #[mesh(4)]
            pub bus_master: SavedChannelBusMasterState,
            #[mesh(5)]
            pub drive0: Option<SavedDriveState>,
            #[mesh(6)]
            pub drive1: Option<SavedDriveState>,
        }
    }

    impl SaveRestore for IdeDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let BusMasterState {
                cmd_status_reg,
                port_addr_reg,
                timing_reg,
                secondary_timing_reg,
                dma_ctl_reg,
            } = self.bus_master_state;

            let bus_master = state::SavedBusMasterState {
                cmd_status_reg,
                port_addr_reg,
                timing_reg,
                secondary_timing_reg,
                dma_ctl_reg,
            };

            let saved_state = state::SavedState {
                bus_master,
                channel0: self.channels[0].save()?,
                channel1: self.channels[1].save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                bus_master:
                    state::SavedBusMasterState {
                        cmd_status_reg,
                        port_addr_reg,
                        timing_reg,
                        secondary_timing_reg,
                        dma_ctl_reg,
                    },
                channel0,
                channel1,
            } = state;

            self.bus_master_state = BusMasterState {
                cmd_status_reg,
                port_addr_reg,
                timing_reg,
                secondary_timing_reg,
                dma_ctl_reg,
            };

            self.channels[0].restore(channel0)?;
            self.channels[1].restore(channel1)?;

            Ok(())
        }
    }

    #[derive(Debug, Error)]
    enum ChannelRestoreError {
        #[error("missing drive for state")]
        MissingDriveForState,
        #[error("missing state for drive")]
        MissingStateForDrive,
    }

    impl Channel {
        fn save(&mut self) -> Result<state::SavedChannelState, SaveError> {
            // We wait for the completion of deferred IOs as part of pause.
            assert!(self.enlightened_write.is_none());

            let ChannelState {
                current_drive_idx,
                shadow_adapter_control_reg,
                shadow_features_reg,
            } = self.state;

            let ChannelBusMasterState {
                command_reg,
                status_reg,
                desc_table_ptr,
                dma_state,
                dma_error,
            } = &self.bus_master_state;

            let saved_state = state::SavedChannelState {
                current_drive_idx: current_drive_idx as u8,
                shadow_adapter_control_reg,
                shadow_features_reg,
                bus_master: state::SavedChannelBusMasterState {
                    command_reg: command_reg.into_bits(),
                    status_reg: status_reg.into_bits(),
                    desc_table_ptr: *desc_table_ptr,
                    dma_state: dma_state.as_ref().map(|dma| {
                        let DmaState {
                            descriptor_idx,
                            transfer_complete,
                            transfer_bytes_left,
                            transfer_base_addr,
                        } = dma;

                        state::SavedDmaState {
                            descriptor_idx: *descriptor_idx,
                            transfer_complete: *transfer_complete,
                            transfer_bytes_left: *transfer_bytes_left,
                            transfer_base_addr: *transfer_base_addr,
                        }
                    }),
                    dma_error: *dma_error,
                },
                drive0: self.drives[0]
                    .as_mut()
                    .map(|drive| drive.save())
                    .transpose()?,
                drive1: self.drives[1]
                    .as_mut()
                    .map(|drive| drive.save())
                    .transpose()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: state::SavedChannelState) -> Result<(), RestoreError> {
            let state::SavedChannelState {
                current_drive_idx,
                shadow_adapter_control_reg,
                shadow_features_reg,
                bus_master:
                    state::SavedChannelBusMasterState {
                        command_reg,
                        status_reg,
                        desc_table_ptr,
                        dma_state,
                        dma_error,
                    },
                drive0,
                drive1,
            } = state;

            self.state = ChannelState {
                current_drive_idx: current_drive_idx as usize,
                shadow_adapter_control_reg,
                shadow_features_reg,
            };

            self.bus_master_state = ChannelBusMasterState {
                command_reg: BusMasterCommandReg::from_bits(command_reg),
                status_reg: BusMasterStatusReg::from_bits(status_reg),
                desc_table_ptr,
                dma_state: dma_state.map(|dma| {
                    let state::SavedDmaState {
                        descriptor_idx,
                        transfer_complete,
                        transfer_bytes_left,
                        transfer_base_addr,
                    } = dma;

                    DmaState {
                        descriptor_idx,
                        transfer_complete,
                        transfer_bytes_left,
                        transfer_base_addr,
                    }
                }),
                dma_error,
            };

            for (drive, state) in self.drives.iter_mut().zip([drive0, drive1]) {
                match (drive, state) {
                    (Some(drive), Some(state)) => drive.restore(state)?,
                    (None, None) => {}
                    (Some(_), None) => {
                        return Err(RestoreError::InvalidSavedState(
                            ChannelRestoreError::MissingStateForDrive.into(),
                        ));
                    }
                    (None, Some(_)) => {
                        return Err(RestoreError::InvalidSavedState(
                            ChannelRestoreError::MissingDriveForState.into(),
                        ));
                    }
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdeIoPort;
    use crate::protocol::BusMasterDmaDesc;
    use crate::protocol::DeviceHeadReg;
    use crate::protocol::IdeCommand;
    use chipset_device::pio::ExternallyManagedPortIoIntercepts;
    use disk_file::FileDisk;
    use pal_async::async_test;
    use scsidisk::atapi_scsi::AtapiScsiDisk;
    use scsidisk::scsidvd::SimpleScsiDvd;
    use std::fs::File;
    use std::future::poll_fn;
    use std::io::Read;
    use std::io::Write;
    use std::task::Poll;
    use tempfile::NamedTempFile;
    use test_with_tracing::test;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    #[derive(Debug, Inspect)]
    struct MediaGeometry {
        sectors_per_track: u32,
        cylinder_count: u32,
        head_count: u32,
        total_sectors: u64,
    }

    impl MediaGeometry {
        fn new(total_sectors: u64, sector_size: u32) -> Result<Self, NewDeviceError> {
            if total_sectors > protocol::MAX_BYTES_48BIT_LBA / sector_size as u64 {
                return Err(NewDeviceError::DiskTooLarge(
                    total_sectors * sector_size as u64,
                ));
            }
            let hard_drive_sectors = total_sectors.min(protocol::MAX_CHS_SECTORS as u64);
            let mut sectors_per_track;
            let mut cylinders_times_heads;
            let mut head_count;

            if hard_drive_sectors > (16 * 63 * 0xFFFF) {
                sectors_per_track = 255;
                head_count = 16;
                cylinders_times_heads = hard_drive_sectors / (sectors_per_track as u64);
            } else {
                sectors_per_track = 17;
                cylinders_times_heads = hard_drive_sectors / (sectors_per_track as u64);

                head_count = std::cmp::max((cylinders_times_heads as u32).div_ceil(1024), 4);

                if (cylinders_times_heads >= (head_count as u64) * 1024) || head_count > 16 {
                    // Always use 16 heads
                    head_count = 16;
                    sectors_per_track = 31;
                    cylinders_times_heads = hard_drive_sectors / (sectors_per_track as u64);
                }

                if cylinders_times_heads >= (head_count as u64) * 1024 {
                    // Always use 16 heads
                    head_count = 16;
                    sectors_per_track = 63;
                    cylinders_times_heads = hard_drive_sectors / (sectors_per_track as u64);
                }
            }
            Ok(MediaGeometry {
                sectors_per_track,
                cylinder_count: (cylinders_times_heads / (head_count as u64)) as u32,
                head_count,
                total_sectors,
            })
        }
    }

    struct CommandParams {
        sector_count: u8,
        sector_num: u8,
        cylinder_lsb: u8,
        cylinder_msb: u8,
        device_head: u8,
    }

    #[expect(dead_code)]
    enum Addressing {
        Chs,
        Lba28Bit,
        Lba48Bit,
    }

    fn ide_test_setup(
        guest_memory: Option<GuestMemory>,
        drive_type: DriveType,
    ) -> (IdeDevice, File, Vec<u32>, MediaGeometry) {
        let test_guest_mem = match guest_memory {
            Some(test_gm) => test_gm,
            None => GuestMemory::allocate(16 * 1024),
        };

        // prep file (write 4GB, numbers 1-1048576 (1GB))
        let temp_file = NamedTempFile::new().unwrap();
        let mut handle1 = temp_file.reopen().unwrap();
        let handle2 = temp_file.reopen().unwrap();
        let data = (0..0x100000_u32).collect::<Vec<_>>();
        handle1.write_all(data.as_bytes()).unwrap();

        let disk = Disk::new(FileDisk::open(handle1, false).unwrap()).unwrap();
        let geometry = MediaGeometry::new(disk.sector_count(), disk.sector_size()).unwrap();

        let media = match drive_type {
            DriveType::Hard => DriveMedia::hard_disk(disk),
            DriveType::Optical => DriveMedia::optical_disk(Arc::new(AtapiScsiDisk::new(Arc::new(
                SimpleScsiDvd::new(Some(disk)),
            )))),
        };

        let ide_device = IdeDevice::new(
            test_guest_mem,
            &mut ExternallyManagedPortIoIntercepts,
            [Some(media), None],
            [None, None],
            LineInterrupt::detached(),
            LineInterrupt::detached(),
        )
        .unwrap();

        (ide_device, handle2, data, geometry)
    }

    // IDE Test Host protocol functions
    fn get_status(ide_controller: &mut IdeDevice, dev_path: &IdePath) -> Status {
        let mut data = [0_u8; 1];
        ide_controller
            .io_read(
                io_port(IdeIoPort::PRI_STATUS_CMD, dev_path.channel.into()),
                &mut data,
            )
            .unwrap();

        Status::from_bits(data[0])
    }

    async fn check_status_loop(ide_device: &mut IdeDevice, dev_path: &IdePath) -> Status {
        // loop until device is not busy and is ready to transfer data.
        wait_for(ide_device, |ide_device| {
            let status: Status = get_status(ide_device, dev_path);
            (!status.bsy() && !status.drq()).then_some(status)
        })
        .await
    }

    async fn check_command_ready(ide_device: &mut IdeDevice, dev_path: &IdePath) -> Status {
        // loop until device is not busy and is ready to transfer data.
        wait_for(ide_device, |ide_device| {
            let status: Status = get_status(ide_device, dev_path);
            (!status.bsy() && status.drdy()).then_some(status)
        })
        .await
    }

    fn io_port(io_port: IdeIoPort, channel_idx: usize) -> u16 {
        if channel_idx == 0 {
            io_port.0
        } else {
            io_port.0 - IdeIoPort::PRI_DATA.0 + IdeIoPort::SEC_DATA.0
        }
    }

    // Host: setup command parameters by writing to features, sector count/number,
    // cylinder low/high, dev/head registers
    fn write_command_params(
        controller: &mut IdeDevice,
        dev_path: &IdePath,
        sector: u32,
        sector_count: u8,
        addr: Addressing,
        geometry: &MediaGeometry,
    ) {
        let channel_idx: usize = dev_path.channel as usize;

        let io_params = match addr {
            Addressing::Chs => {
                let sectors_per_track = geometry.sectors_per_track;
                let head_count = geometry.head_count;

                let sector_num: u8 = ((sector % sectors_per_track) as u8) + 1;
                let cylinders: u16 = (sector / (head_count * sectors_per_track)) as u16;
                let cylinder_lsb: u8 = cylinders as u8;
                let cylinder_msb: u8 = (cylinders >> 8) as u8;
                let device_head: u8 = (sector / sectors_per_track % head_count) as u8;

                CommandParams {
                    sector_count,
                    sector_num,
                    cylinder_lsb,
                    cylinder_msb,
                    device_head,
                }
            }
            Addressing::Lba28Bit => {
                let sector_num = sector as u8;
                let cylinder = (sector & 0x00FF_FF00) >> 8;
                let cylinder_lsb: u8 = cylinder as u8;
                let cylinder_msb: u8 = (cylinder >> 8) as u8;
                let device_head = DeviceHeadReg::new()
                    .with_head((sector >> 24) as u8)
                    .with_lba(true)
                    .into();

                CommandParams {
                    sector_count,
                    sector_num,
                    cylinder_lsb,
                    cylinder_msb,
                    device_head,
                }
            }
            Addressing::Lba48Bit => todo!(),
        };

        controller
            .io_write(
                io_port(IdeIoPort::PRI_SECTOR_COUNT, channel_idx),
                &[io_params.sector_count],
            )
            .unwrap();
        controller
            .io_write(
                io_port(IdeIoPort::PRI_SECTOR_NUM, channel_idx),
                &[io_params.sector_num],
            )
            .unwrap();
        controller
            .io_write(
                io_port(IdeIoPort::PRI_CYLINDER_LSB, channel_idx),
                &[io_params.cylinder_lsb],
            )
            .unwrap();
        controller
            .io_write(
                io_port(IdeIoPort::PRI_CYLINDER_MSB, channel_idx),
                &[io_params.cylinder_msb],
            )
            .unwrap();
        controller
            .io_write(
                io_port(IdeIoPort::PRI_DEVICE_HEAD, channel_idx),
                &[io_params.device_head],
            )
            .unwrap();
    }

    // Host: Execute device selection protocol
    async fn device_select(ide_controller: &mut IdeDevice, dev_path: &IdePath) {
        check_status_loop(ide_controller, dev_path).await;

        let dev_idx: u8 = dev_path.drive;
        ide_controller
            .io_write(
                io_port(IdeIoPort::PRI_DEVICE_HEAD, dev_path.channel.into()),
                &[dev_idx],
            )
            .unwrap();

        check_status_loop(ide_controller, dev_path).await;
    }

    // Host: Write command code to command register, wait 400ns before reading status register
    fn execute_command(ide_controller: &mut IdeDevice, dev_path: &IdePath, command: u8) {
        ide_controller
            .io_write(
                io_port(IdeIoPort::PRI_STATUS_CMD, dev_path.channel.into()),
                &[command],
            )
            .unwrap();
    }

    fn execute_soft_reset_command(ide_controller: &mut IdeDevice, dev_path: &IdePath, command: u8) {
        ide_controller
            .io_write(
                io_port(
                    IdeIoPort::PRI_ALT_STATUS_DEVICE_CTL,
                    dev_path.channel.into(),
                ),
                &[command],
            )
            .unwrap();
    }

    fn prep_ide_channel(ide_controller: &mut IdeDevice, drive_type: DriveType, dev_path: &IdePath) {
        match drive_type {
            DriveType::Hard => {
                // SET MULTIPLE MODE - sets number of sectors per block to 128
                // this is needed when computing IO sector count for a read/write
                execute_command(ide_controller, dev_path, IdeCommand::SET_MULTI_BLOCK_MODE.0);
            }
            DriveType::Optical => {
                // Optical is a ATAPI (PACKET) drive and does not support the SET_MULTI_BLOCK_MODE command
            }
        }
    }

    // Waits for a condition on the IDE device, polling the device until then.
    async fn wait_for<T>(
        ide_device: &mut IdeDevice,
        mut f: impl FnMut(&mut IdeDevice) -> Option<T>,
    ) -> T {
        poll_fn(|cx| {
            ide_device.poll_device(cx);
            let r = f(ide_device);
            if let Some(r) = r {
                Poll::Ready(r)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    // IDE Command Tests
    // Command: WRITE SECTOR(S)
    #[async_test]
    async fn write_sectors_test() {
        const START_SECTOR: u32 = 0;
        const SECTOR_COUNT: u8 = 4;

        let dev_path = IdePath::default();
        let (mut ide_device, mut disk, _file_contents, geometry) =
            ide_test_setup(None, DriveType::Hard);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);

        // write to first 4 sectors
        write_command_params(
            &mut ide_device,
            &dev_path,
            START_SECTOR,
            SECTOR_COUNT,
            Addressing::Lba28Bit,
            &geometry,
        );

        execute_command(&mut ide_device, &dev_path, IdeCommand::WRITE_SECTORS.0);

        // drive status should contain DRQ as data is ready to be exchanged with host
        let status = get_status(&mut ide_device, &dev_path);
        assert!(status.drq() && !status.bsy());

        // PIO - writes to data port
        let data = &[0xFF_u8; 2][..];
        for _ in 0..SECTOR_COUNT {
            let status = check_command_ready(&mut ide_device, &dev_path).await;
            assert!(status.drq());
            assert!(!status.err());
            for _ in 0..protocol::HARD_DRIVE_SECTOR_BYTES / 2 {
                ide_device.io_write(IdeIoPort::PRI_DATA.0, data).unwrap();
            }
        }

        let status = check_command_ready(&mut ide_device, &dev_path).await;
        assert!(!status.err());
        assert!(!status.drq());

        let buffer =
            &mut [0_u8; (protocol::HARD_DRIVE_SECTOR_BYTES * SECTOR_COUNT as u32) as usize][..];
        disk.read_exact(buffer).unwrap();
        for byte in buffer {
            assert_eq!(*byte, 0xFF);
        }
    }

    #[async_test]
    async fn software_reset_test() {
        const START_SECTOR: u32 = 0;
        const SECTOR_COUNT: u8 = 4;

        let dev_path = IdePath::default();
        let (mut ide_device, _disk, _file_contents, geometry) =
            ide_test_setup(None, DriveType::Hard);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);

        // write to first 4 sectors
        write_command_params(
            &mut ide_device,
            &dev_path,
            START_SECTOR,
            SECTOR_COUNT,
            Addressing::Lba28Bit,
            &geometry,
        );

        execute_command(&mut ide_device, &dev_path, IdeCommand::WRITE_SECTORS.0);
        // drive status should contain DRQ as data is ready to be exchanged with host
        let status = get_status(&mut ide_device, &dev_path);
        assert!(status.drq() && !status.bsy());

        execute_soft_reset_command(&mut ide_device, &dev_path, IdeCommand::SOFT_RESET.0);
        let status = get_status(&mut ide_device, &dev_path);
        assert!(status.bsy());
    }

    // Command: READ SECTOR(S)
    #[async_test]
    async fn read_sectors_test() {
        const START_SECTOR: u32 = 0;
        const SECTOR_COUNT: u8 = 4;

        let dev_path = IdePath::default();
        let (mut ide_device, _disk, file_contents, geometry) =
            ide_test_setup(None, DriveType::Hard);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);

        // read the first 4 sectors
        write_command_params(
            &mut ide_device,
            &dev_path,
            START_SECTOR,
            SECTOR_COUNT,
            Addressing::Lba28Bit,
            &geometry,
        );

        // PIO - writes sectors to track cache buffer
        execute_command(&mut ide_device, &dev_path, IdeCommand::READ_SECTORS.0);

        let status = check_command_ready(&mut ide_device, &dev_path).await;
        assert!(status.drq());
        assert!(!status.err());

        // PIO - reads data from track cache buffer
        let content_bytes = file_contents.as_bytes();
        for sector in 0..SECTOR_COUNT {
            let status = check_command_ready(&mut ide_device, &dev_path).await;
            assert!(status.drq());
            assert!(!status.err());
            for word in 0..protocol::HARD_DRIVE_SECTOR_BYTES / 2 {
                let data = &mut [0, 0][..];
                ide_device.io_read(IdeIoPort::PRI_DATA.0, data).unwrap();

                let i = sector as usize * protocol::HARD_DRIVE_SECTOR_BYTES as usize / 2
                    + word as usize;
                assert_eq!(data[0], content_bytes[i * 2]);
                assert_eq!(data[1], content_bytes[i * 2 + 1]);
            }
        }
    }

    // Command: READ SECTOR(S) - enlightened
    async fn enlightened_cmd_test(drive_type: DriveType) {
        const SECTOR_COUNT: u16 = 4;
        const BYTE_COUNT: u16 = SECTOR_COUNT * protocol::HARD_DRIVE_SECTOR_BYTES as u16;

        let test_guest_mem = GuestMemory::allocate(16384);

        let table_gpa = 0x1000;
        let data_gpa = 0x2000;
        test_guest_mem
            .write_plain(
                table_gpa,
                &BusMasterDmaDesc {
                    mem_physical_base: data_gpa,
                    byte_count: BYTE_COUNT,
                    unused: 0,
                    end_of_table: 0x80,
                },
            )
            .unwrap();

        let (data_buffer, byte_count) = match drive_type {
            DriveType::Hard => (table_gpa as u32, 0),
            DriveType::Optical => (data_gpa, BYTE_COUNT.into()),
        };

        let eint13_command = protocol::EnlightenedInt13Command {
            command: IdeCommand::READ_DMA_EXT,
            device_head: DeviceHeadReg::new().with_lba(true),
            flags: 0,
            result_status: 0,
            lba_low: 0,
            lba_high: 0,
            block_count: SECTOR_COUNT,
            byte_count,
            data_buffer,
            skip_bytes_head: 0,
            skip_bytes_tail: 0,
        };
        test_guest_mem.write_plain(0, &eint13_command).unwrap();

        let dev_path = IdePath::default();
        let (mut ide_device, _disk, file_contents, _geometry) =
            ide_test_setup(Some(test_guest_mem.clone()), drive_type);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, drive_type, &dev_path);

        // READ SECTORS - enlightened
        let r = ide_device.io_write(IdeIoPort::PRI_ENLIGHTENED.0, 0_u32.as_bytes()); // read from gpa 0

        match r {
            IoResult::Defer(mut deferred) => {
                poll_fn(|cx| {
                    ide_device.poll_device(cx);
                    deferred.poll_write(cx)
                })
                .await
                .unwrap();
            }
            _ => panic!("{:?}", r),
        }

        let mut buffer = vec![0u8; BYTE_COUNT as usize];
        test_guest_mem
            .read_at(data_gpa.into(), &mut buffer)
            .unwrap();
        assert_eq!(buffer, file_contents.as_bytes()[..buffer.len()]);
    }

    // Command: READ SECTOR(S) - enlightened
    #[async_test]
    async fn enlightened_cd_cmd_test() {
        enlightened_cmd_test(DriveType::Optical).await
    }

    #[async_test]
    async fn enlightened_hdd_cmd_test() {
        enlightened_cmd_test(DriveType::Hard).await
    }

    // Command: READ SECTOR(S) - enlightened
    // However, provide incomplete PRD table
    #[async_test]
    async fn enlightened_cmd_test_incomplete_prd() {
        const SECTOR_COUNT: u16 = 8;
        const BYTE_COUNT: u16 = SECTOR_COUNT * protocol::HARD_DRIVE_SECTOR_BYTES as u16;

        let test_guest_mem = GuestMemory::allocate(16384);

        let table_gpa = 0x1000;
        let data_gpa = 0x2000;
        test_guest_mem
            .write_plain(
                table_gpa,
                &BusMasterDmaDesc {
                    mem_physical_base: data_gpa,
                    byte_count: BYTE_COUNT / 2,
                    unused: 0,
                    end_of_table: 0x80, // Mark end before second PRD
                },
            )
            .unwrap();
        test_guest_mem
            .write_plain(
                table_gpa + size_of::<BusMasterDmaDesc>() as u64,
                &BusMasterDmaDesc {
                    mem_physical_base: data_gpa,
                    byte_count: BYTE_COUNT / 2,
                    unused: 0,
                    end_of_table: 0x80,
                },
            )
            .unwrap();

        let data_buffer = table_gpa as u32;
        let byte_count = 0;

        let eint13_command = protocol::EnlightenedInt13Command {
            command: IdeCommand::READ_DMA_EXT,
            device_head: DeviceHeadReg::new().with_lba(true),
            flags: 0,
            result_status: 0,
            lba_low: 0,
            lba_high: 0,
            block_count: SECTOR_COUNT,
            byte_count,
            data_buffer,
            skip_bytes_head: 0,
            skip_bytes_tail: 0,
        };
        test_guest_mem.write_plain(0, &eint13_command).unwrap();

        let dev_path = IdePath::default();
        let (mut ide_device, _disk, file_contents, _geometry) =
            ide_test_setup(Some(test_guest_mem.clone()), DriveType::Hard);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);

        // READ SECTORS - enlightened
        let r = ide_device.io_write(IdeIoPort::PRI_ENLIGHTENED.0, 0_u32.as_bytes()); // read from gpa 0

        match r {
            IoResult::Defer(mut deferred) => {
                poll_fn(|cx| {
                    ide_device.poll_device(cx);
                    deferred.poll_write(cx)
                })
                .await
                .unwrap();
            }
            _ => panic!("{:?}", r),
        }

        let mut buffer = vec![0u8; BYTE_COUNT as usize / 2];
        test_guest_mem
            .read_at(data_gpa.into(), &mut buffer)
            .unwrap();
        assert_eq!(buffer, file_contents.as_bytes()[..buffer.len()]);
    }

    #[async_test]
    async fn enlightened_cmd_test_invalid_dma_base() {
        /*
        This is a negative test case where the DMA base address is invalid.
        The test sets the DMA base address to an out-of-bounds memory
        address of the guest range and expects the device to not read any data.
        */
        const SECTOR_COUNT: u16 = 8;
        const BYTE_COUNT: u16 = SECTOR_COUNT * protocol::HARD_DRIVE_SECTOR_BYTES as u16;

        let test_guest_mem = GuestMemory::allocate(16384);

        let table_gpa = 0x1000;
        let data_gpa = 0x4000; // Invalid out-of-bounds memory address
        test_guest_mem
            .write_plain(
                table_gpa,
                &BusMasterDmaDesc {
                    mem_physical_base: data_gpa,
                    byte_count: BYTE_COUNT / 2,
                    unused: 0,
                    end_of_table: 0x80,
                },
            )
            .unwrap();

        let data_buffer = table_gpa as u32;
        let byte_count = 0;

        let eint13_command = protocol::EnlightenedInt13Command {
            command: IdeCommand::WRITE_DMA_ALT,
            device_head: DeviceHeadReg::new().with_lba(true),
            flags: 0,
            result_status: 0,
            lba_low: 0,
            lba_high: 0,
            block_count: SECTOR_COUNT,
            byte_count,
            data_buffer,
            skip_bytes_head: 0,
            skip_bytes_tail: 0,
        };
        test_guest_mem.write_plain(0, &eint13_command).unwrap();

        let dev_path = IdePath::default();
        let (mut ide_device, _disk, _file_contents, _geometry) =
            ide_test_setup(Some(test_guest_mem.clone()), DriveType::Hard);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);

        // READ SECTORS - enlightened
        let r = ide_device.io_write(IdeIoPort::PRI_ENLIGHTENED.0, 0_u32.as_bytes()); // read from gpa 0

        match r {
            IoResult::Defer(mut deferred) => {
                poll_fn(|cx| {
                    ide_device.poll_device(cx);
                    deferred.poll_write(cx)
                })
                .await
                .unwrap();
            }
            _ => panic!("{:?}", r),
        }
    }

    #[async_test]
    async fn identify_test_cd() {
        let dev_path = IdePath::default();
        let (mut ide_device, _disk, _file_contents, _geometry) =
            ide_test_setup(None, DriveType::Optical);

        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Optical, &dev_path);

        // PIO - writes sectors to track cache buffer
        execute_command(
            &mut ide_device,
            &dev_path,
            IdeCommand::IDENTIFY_PACKET_DEVICE.0,
        );

        let status = check_command_ready(&mut ide_device, &dev_path).await;
        assert!(status.drq());
        assert!(!status.err());

        // PIO - reads data from track cache buffer
        let data = &mut [0_u8; protocol::IDENTIFY_DEVICE_BYTES];
        ide_device.io_read(IdeIoPort::PRI_DATA.0, data).unwrap();
        let features = protocol::IdeFeatures::read_from_prefix(&data[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let ex_features = protocol::IdeFeatures {
            config_bits: 0x85C0,
            serial_no: *b"                    ",
            buffer_size: 0x0080,
            firmware_revision: *b"        ",
            model_number: "iVtrau lDC                              ".as_bytes()[..]
                .try_into()
                .unwrap(),
            capabilities: 0x0300,
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
        assert_eq!(features.as_bytes(), ex_features.as_bytes());
    }

    #[async_test]
    async fn identify_test_hdd() {
        let dev_path = IdePath::default();
        let (mut ide_device, _disk, _file_contents, geometry) =
            ide_test_setup(None, DriveType::Hard);
        // select device [0,0] = primary channel, primary drive
        device_select(&mut ide_device, &dev_path).await;
        prep_ide_channel(&mut ide_device, DriveType::Hard, &dev_path);
        // PIO - writes sectors to track cache buffer
        execute_command(&mut ide_device, &dev_path, IdeCommand::IDENTIFY_DEVICE.0);

        let status = check_command_ready(&mut ide_device, &dev_path).await;
        assert!(status.drq());
        assert!(!status.err());

        // PIO - reads data from track cache buffer
        let data = &mut [0_u8; protocol::IDENTIFY_DEVICE_BYTES];
        ide_device.io_read(IdeIoPort::PRI_DATA.0, data).unwrap();
        let features = protocol::IdeFeatures::read_from_prefix(&data[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        let total_chs_sectors: u32 =
            geometry.sectors_per_track * geometry.cylinder_count * geometry.head_count;
        let (cylinders, heads, sectors_per_track) = if total_chs_sectors < protocol::MAX_CHS_SECTORS
        {
            (
                geometry.cylinder_count as u16,
                geometry.head_count as u16,
                geometry.sectors_per_track as u16,
            )
        } else {
            (0x3FFF, 16, 63)
        };

        let firmware_revision = if dev_path.channel == 0 {
            ".1.1 0  "
        } else {
            ".1.1 1  "
        }
        .as_bytes()[..]
            .try_into()
            .unwrap();

        let user_addressable_sectors =
            if geometry.total_sectors > (protocol::LBA_28BIT_MAX_SECTORS as u64) {
                protocol::LBA_28BIT_MAX_SECTORS
            } else {
                geometry.total_sectors as u32
            };

        let ex_features = protocol::IdeFeatures {
            config_bits: 0x045A,
            cylinders,
            heads,
            unformatted_sectors_per_track: (protocol::HARD_DRIVE_SECTOR_BYTES
                * geometry.sectors_per_track) as u16,
            unformatted_bytes_per_sector: protocol::HARD_DRIVE_SECTOR_BYTES as u16,
            sectors_per_track,
            compact_flash: [0xABCD, 0xDCBA],
            vendor0: 0x0123,
            serial_no: *b"                    ",
            buffer_type: 3,
            buffer_size: 0x0080,
            firmware_revision,
            model_number: "iVtrau lDH                              ".as_bytes()[..]
                .try_into()
                .unwrap(),
            max_sectors_mult_transfer: (0x8000 | protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT),
            capabilities: 0x0F00,          // supports Dma, IORDY, LBA
            pio_cycle_times: 0x0200,       // indicate fast I/O
            dma_cycle_times: 0x0200,       // indicate fast I/O
            new_words_valid_flags: 0x0003, // indicate next words are valid
            log_cylinders: geometry.cylinder_count as u16,
            log_heads: geometry.head_count as u16,
            log_sectors_per_track: geometry.sectors_per_track as u16,
            log_total_sectors: total_chs_sectors.into(),
            multi_sector_capabilities: 0x0100_u16 | protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT,
            user_addressable_sectors: user_addressable_sectors.into(),
            single_word_dma_mode: 0x0007, // support up to mode 3, no mode active
            multi_word_dma_mode: 0x0407,  // support up to mode 3, mode 3 active
            enhanced_pio_mode: 0x0003,    // PIO mode 3 and 4 supported
            min_multi_dma_time: 0x0078,
            recommended_multi_dma_time: 0x0078,
            min_pio_cycle_time_no_flow: 0x014D,
            min_pio_cycle_time_flow: 0x0078,
            major_version_number: 0x01F0, // claim support for ATA4-ATA8
            minor_version_number: 0,
            command_set_supported: 0x0028, // support caching and power management
            command_sets_supported: 0x7400, // support flushing
            command_set_supported_ext: 0x4040, // write fua support for default write hardening
            command_set_enabled1: 0x0028,  // support caching and power management
            command_set_enabled2: 0x3400,  // support flushing
            command_set_default: 0x4040,   // write fua support for default write hardening
            total_sectors_48_bit: geometry.total_sectors.into(),
            default_sector_size_config: 0x4000, // describes the sector size related info. Reflect the underlying device sector size and logical:physical ratio
            logical_block_alignment: 0x4000, // describes alignment of logical blocks within physical block
            ..FromZeros::new_zeroed()
        };
        assert_eq!(features.as_bytes(), ex_features.as_bytes());
    }
}
