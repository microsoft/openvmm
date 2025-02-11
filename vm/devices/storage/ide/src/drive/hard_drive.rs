// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a generic ATA disk drive with 48-bit LBA support, wrapping a
//! [`Disk`].

use super::DriveRegister;
use crate::protocol;
use crate::protocol::DeviceControlReg;
use crate::protocol::DeviceHeadReg;
use crate::protocol::ErrorReg;
use crate::protocol::IdeCommand;
use crate::protocol::Status;
use crate::DmaType;
use crate::NewDeviceError;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::ranges::PagedRange;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use ide_resources::IdePath;
use inspect::Inspect;
use safeatomic::AtomicSliceOps;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use tracing_helpers::ErrorValueExt;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const MAX_CMD_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
enum IdeError {
    #[error("flush error")]
    Flush(#[source] DiskError),
    #[error("ide error bad location {lba:#x} + {sector_count:#x} > {disk_sector_count:#x}")]
    IdeBadLocation {
        lba: u64,
        sector_count: u32,
        disk_sector_count: u64,
    },
    #[error("ide error bad sector")]
    IdeBadSector(#[source] DiskError),
    #[error("sector number of zero in CHS mode")]
    ZeroSector,
    #[error("lba bit not set in 48-bit lba command")]
    LbaBitNotSet,
}

/// The device command and control register sets.
///
/// Status is a computed register and is not present here.
#[derive(Debug, Inspect)]
struct Registers {
    error: ErrorReg, // N.B. this may have a value even if !error_pending
    #[inspect(hex)]
    features: u8,
    device_head: DeviceHeadReg,
    #[inspect(hex)]
    lba_low: u16,
    #[inspect(hex)]
    lba_mid: u16,
    #[inspect(hex)]
    lba_high: u16,
    #[inspect(hex)]
    sector_count: u16,
    device_control_reg: DeviceControlReg,
}

impl Registers {
    fn at_reset() -> Self {
        Self {
            sector_count: 1,
            lba_low: 1,
            lba_mid: 0,
            lba_high: 0,
            device_head: DeviceHeadReg::new(),
            error: ErrorReg::new().with_amnf_ili_default(true),
            features: 0,
            device_control_reg: DeviceControlReg::new(),
        }
    }

    fn reset_signature(&mut self) {
        self.sector_count = 1;
        self.lba_low = 1;
        self.lba_mid = 0;
        self.lba_high = 0;
        self.device_head = DeviceHeadReg::new();
    }

    /// Computes the current LBA from the register state.
    fn lba(&self, use_48bit: bool, geometry: &MediaGeometry) -> Result<u64, IdeError> {
        if use_48bit {
            self.lba48()
        } else {
            self.lba28(geometry)
        }
    }

    /// Computes the current LBA from the register state for a 48-bit LBA
    /// command.
    fn lba48(&self) -> Result<u64, IdeError> {
        if !self.device_head.lba() {
            // CHS is not allowed for 48-bit commands.
            return Err(IdeError::LbaBitNotSet);
        }
        Ok((self.lba_low as u64 & 0xff)
            | ((self.lba_mid as u64 & 0xff) << 8)
            | ((self.lba_high as u64 & 0xff) << 16)
            | ((self.lba_low as u64 & 0xff00) << 16)
            | ((self.lba_mid as u64 & 0xff00) << 24)
            | ((self.lba_high as u64 & 0xff00) << 32))
    }

    /// Computes the current LBA from the register state for a non-48-bit LBA
    /// command.
    fn lba28(&self, geometry: &MediaGeometry) -> Result<u64, IdeError> {
        let lba = if self.device_head.lba() {
            // 28-bit LBA.
            (self.lba_low as u64 & 0xff)
                | ((self.lba_mid as u64 & 0xff) << 8)
                | ((self.lba_high as u64 & 0xff) << 16)
                | ((self.device_head.head() as u64) << 24)
        } else {
            // CHS
            let sector = self.lba_low as u64 & 0xff;
            let head = self.device_head.head() as u64;
            let cylinder_low = self.lba_mid as u64 & 0xff;
            let cylinder_high = self.lba_high as u64 & 0xff;
            let cylinder = (cylinder_high << 8) | cylinder_low;
            let track = cylinder * geometry.head_count as u64 + head;
            track * geometry.sectors_per_track as u64
                + (sector.checked_sub(1).ok_or(IdeError::ZeroSector)?)
        };
        Ok(lba)
    }

    /// Updates the registers to refer to the given LBA.
    fn seek(&mut self, use_48bit: bool, geometry: &MediaGeometry, lba: u64) {
        if use_48bit {
            self.seek48(lba)
        } else {
            self.seek28(geometry, lba)
        }
    }

    /// Updates the registers to refer to the given 48-bit LBA.
    fn seek48(&mut self, lba: u64) {
        if self.device_head.lba() {
            self.lba_low = (lba & 0xff) as u16;
            self.lba_mid = ((lba & 0xff00) >> 8) as u16;
            self.lba_high = ((lba & 0xff_0000) >> 16) as u16;
            self.lba_low |= ((lba & 0xff00_0000) >> 16) as u16;
            self.lba_mid |= ((lba & 0xff_0000_0000) >> 24) as u16;
            self.lba_high |= ((lba & 0xff00_0000_0000) >> 32) as u16;
        } else {
            tracelimit::warn_ratelimited!("48-bit LBA seek attempted when LBA bit is not set");
        }
    }

    /// Updates the registers to refer to the given 28-bit LBA.
    fn seek28(&mut self, geometry: &MediaGeometry, lba: u64) {
        if self.device_head.lba() {
            self.lba_low = (lba & 0xff) as u16;
            self.lba_mid = ((lba & 0xff00) >> 8) as u16;
            self.lba_high = ((lba & 0xff_0000) >> 16) as u16;
            self.device_head.set_head(((lba & 0xf00_0000) >> 24) as u8);
        } else {
            let sector = (lba % geometry.sectors_per_track as u64) + 1;
            let track = lba / geometry.sectors_per_track as u64;
            let cylinder = track / geometry.head_count as u64;
            let head = track % geometry.head_count as u64;
            self.lba_low = sector as u16 & 0xff;
            self.lba_mid = cylinder as u16 & 0xff;
            self.lba_high = (cylinder as u16 >> 8) & 0xff;
            self.device_head.set_head(head as u8);
        }
    }

    /// Gets the effective disk capacity in sectors for a command.
    fn capacity(&self, use_48bit: bool, geometry: &MediaGeometry) -> u64 {
        if use_48bit {
            geometry.total_sectors
        } else {
            self.capacity28(geometry)
        }
    }

    /// Gets the effective disk capacity in sectors for a non-48-bit LBA
    /// command.
    fn capacity28(&self, geometry: &MediaGeometry) -> u64 {
        if self.device_head.lba() {
            geometry
                .total_sectors
                .min(protocol::LBA_28BIT_MAX_SECTORS.into())
        } else {
            (geometry.head_count * geometry.sectors_per_track * geometry.cylinder_count) as u64
        }
    }
}

#[derive(Debug, Inspect)]
struct DriveState {
    regs: Registers,
    pending_software_reset: bool,
    pending_interrupt: bool,
    max_sector: u8,
    max_head: u8,
    error_pending: bool,
    block_sector_count: u16,
    command: Option<CommandState>,
    buffer: Option<BufferState>,
    prd_exhausted: bool,
}

impl DriveState {
    fn new() -> Self {
        Self {
            regs: Registers::at_reset(),
            pending_software_reset: false,
            pending_interrupt: false,
            max_sector: 0,
            max_head: 0,
            error_pending: false,
            block_sector_count: protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT,
            command: None,
            buffer: None,
            prd_exhausted: false,
        }
    }
}

#[derive(Inspect)]
pub(crate) struct HardDrive {
    disk: Disk,
    state: DriveState,
    geometry: MediaGeometry,
    disk_path: IdePath,
    read_only: bool,

    #[inspect(skip)]
    command_buffer: CommandBuffer,

    #[inspect(with = "Option::is_some")]
    io: Option<Io>,
    #[inspect(skip)]
    waker: Option<Waker>,
}

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

            head_count = std::cmp::max((cylinders_times_heads as u32 + 1023) / 1024, 4);

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
            buffer: Arc::new(AlignedHeapMemory::new(MAX_CMD_BUFFER_BYTES)),
        }
    }

    fn access(&self) -> CommandBufferAccess {
        CommandBufferAccess {
            memory: GuestMemory::new("ata_buffer", self.buffer.clone()),
        }
    }
}

impl CommandBufferAccess {
    fn buffers(&self, offset: usize, len: usize, is_write: bool) -> RequestBuffers<'_> {
        // The buffer is 16 4KB pages long.
        static BUFFER_RANGE: Option<PagedRange<'_>> = PagedRange::new(
            0,
            MAX_CMD_BUFFER_BYTES,
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        );

        RequestBuffers::new(
            &self.memory,
            BUFFER_RANGE.unwrap().subrange(offset, len),
            is_write,
        )
    }
}

struct Io(Pin<Box<dyn Send + Future<Output = Result<(), DiskError>>>>);

impl std::fmt::Debug for Io {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("io")
    }
}

#[derive(Debug, PartialEq, Eq, Inspect)]
enum IoType {
    Read,
    Write,
    Flush,
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
        assert!((len as usize) <= MAX_CMD_BUFFER_BYTES);
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

#[derive(Debug, Inspect)]
struct CommandState {
    io_type: IoType,
    interrupt_after_each: bool,
    use_48bit_lba: bool,
    use_dma: bool,
    fua: bool,
    verify_only: bool,
    next_lba: u64,
    sectors_remaining: u32,
    sectors_before_interrupt: u32,
}

impl CommandState {
    fn read(sector_count: u16) -> Self {
        Self {
            io_type: IoType::Read,
            interrupt_after_each: false,
            use_48bit_lba: false,
            use_dma: false,
            fua: false,
            verify_only: false,
            next_lba: 0,
            sectors_remaining: sector_count.into(),
            sectors_before_interrupt: 0,
        }
    }

    fn write(sector_count: u16) -> Self {
        Self {
            io_type: IoType::Write,
            interrupt_after_each: false,
            use_48bit_lba: false,
            use_dma: false,
            fua: false,
            verify_only: false,
            next_lba: 0,
            sectors_remaining: sector_count.into(),
            sectors_before_interrupt: 0,
        }
    }

    fn verify(sector_count: u16) -> Self {
        Self {
            io_type: IoType::Read,
            interrupt_after_each: false,
            use_48bit_lba: false,
            use_dma: false,
            fua: false,
            verify_only: true,
            next_lba: 0,
            sectors_remaining: sector_count.into(),
            sectors_before_interrupt: 0,
        }
    }

    fn flush() -> Self {
        Self {
            io_type: IoType::Flush,
            interrupt_after_each: false,
            use_48bit_lba: false,
            use_dma: false,
            fua: false,
            verify_only: false,
            next_lba: 0,
            sectors_remaining: 0,
            sectors_before_interrupt: 0,
        }
    }
}

enum IoPortData<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

impl HardDrive {
    pub fn new(disk: Disk, disk_path: IdePath) -> Result<Self, NewDeviceError> {
        // Initialize drive geometry
        let read_only = disk.is_read_only();
        let geometry = MediaGeometry::new(disk.sector_count(), disk.sector_size())?;
        Ok(Self {
            disk,
            state: DriveState::new(),
            geometry,
            disk_path,
            read_only,
            command_buffer: CommandBuffer::new(),
            io: None,
            waker: None,
        })
    }

    pub fn reset(&mut self) {
        self.state = DriveState::new();
        self.io = None;
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
        // Select a byte from a two-byte FIFO register based on the state of
        // the HOB bit.
        let select_byte = |x: u16| {
            if self.state.regs.device_control_reg.high_order_byte() {
                (x >> 8) as u8
            } else {
                x as u8
            }
        };
        match register {
            DriveRegister::ErrorFeatures => self.state.regs.error.into_bits(),
            DriveRegister::SectorCount => select_byte(self.state.regs.sector_count),
            DriveRegister::LbaLow => select_byte(self.state.regs.lba_low),
            DriveRegister::LbaMid => select_byte(self.state.regs.lba_mid),
            DriveRegister::LbaHigh => select_byte(self.state.regs.lba_high),
            DriveRegister::DeviceHead => self.state.regs.device_head.into(),
            DriveRegister::StatusCmd => {
                let status = self.status();
                tracing::trace!(status = ?status, "status query, deasserting");
                self.state.pending_interrupt = false;
                status.into_bits()
            }
            DriveRegister::AlternateStatusDeviceControl => self.status().into_bits(),
        }
    }

    pub fn write_register(&mut self, register: DriveRegister, data: u8) {
        // Shift a data byte into the register.
        let shift_byte = |x: &mut u16| {
            *x = x.wrapping_shl(8);
            *x |= data as u16;
        };
        let mut clear_hob = true;
        match register {
            DriveRegister::ErrorFeatures => self.state.regs.features = data,
            DriveRegister::SectorCount => shift_byte(&mut self.state.regs.sector_count),
            DriveRegister::LbaLow => shift_byte(&mut self.state.regs.lba_low),
            DriveRegister::LbaMid => shift_byte(&mut self.state.regs.lba_mid),
            DriveRegister::LbaHigh => shift_byte(&mut self.state.regs.lba_high),
            DriveRegister::DeviceHead => {
                // Writing to the device register in the middle of an operation leads to "indeterminate" behavior,
                // so reset the other disk if we write to this register in the middle of an operation. This will
                // avoid a stuck state later on if we come back to this device.
                if self.is_selected()
                    && data != self.disk_path.drive
                    && self.state.command.is_some()
                {
                    tracing::warn!("Changing selected drive in the middle of operation. Resetting previously selected drive");
                    self.reset();
                }
                self.state.regs.device_head = data.into();
            }
            DriveRegister::StatusCmd => {
                clear_hob = false;
                let command = IdeCommand(data);

                // Ignore commands targeted at the wrong disk due to missing media.
                //
                // EXECUTE DEVICE DIAGNOSTIC command sets error register for both attachments on channel
                if self.is_selected() || command == IdeCommand::EXECUTE_DEVICE_DIAGNOSTIC {
                    if let Err(err) = self.handle_command(command) {
                        self.log_and_update_error(err, Some(register));
                    }
                }
            }
            DriveRegister::AlternateStatusDeviceControl => {
                clear_hob = false;
                let v = DeviceControlReg::from_bits_truncate(data);
                self.state.regs.device_control_reg = v.with_reset(false);
                if v.reset() && !self.state.pending_software_reset {
                    if self.state.command.is_none() {
                        self.reset();
                    } else {
                        self.state.pending_software_reset = true;
                    }
                }
            }
        }
        // Clear the HOB bit on command register writes, as specified in the
        // ATA spec.
        if clear_hob {
            self.state
                .regs
                .device_control_reg
                .set_high_order_byte(false);
        }
    }

    pub fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(io) = self.io.as_mut() {
            if let Poll::Ready(result) = io.0.as_mut().poll(cx) {
                self.io = None;
                self.handle_io_completion(result);

                // Wait until the command that initiated this IO is completed
                if self.state.command.is_none() && self.state.pending_software_reset {
                    self.reset();
                }
            }
        }
        self.waker = Some(cx.waker().clone());
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
        let buffer = self.state.buffer.as_mut().unwrap();
        let buffer_ptr = &self.command_buffer.buffer[buffer.range()][..len];
        {
            let dma_type = buffer.dma_type.as_ref().unwrap();

            tracing::trace!(
                ?dma_type,
                gpa,
                len,
                cur_byte = buffer.current_byte,
                "performing dma"
            );
        }

        let r = match buffer.dma_type.as_ref().unwrap() {
            DmaType::Write => guest_memory.write_from_atomic(gpa, buffer_ptr),
            DmaType::Read => guest_memory.read_to_atomic(gpa, buffer_ptr),
        };
        if let Err(err) = r {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dma transfer failed"
            );
        }
        if buffer.advance(len as u32) {
            match buffer.dma_type.as_ref().unwrap() {
                DmaType::Write => self.read_data_port_buffer_complete(),
                DmaType::Read => self.write_data_port_buffer_complete(),
            };
            if self.state.buffer.is_none() && self.state.command.is_none() {
                self.state.pending_interrupt = true;
            }
        }
    }

    pub fn dma_advance_buffer(&mut self, len: usize) {
        let buffer = self.state.buffer.as_mut().unwrap();
        let p = buffer.advance(len as u32);
        {
            let dma_type = buffer.dma_type.as_ref().unwrap();

            tracing::trace!(
                ?dma_type,
                len,
                cur_byte = buffer.current_byte,
                "advancing dma buffer"
            );
        }
        if p {
            match buffer.dma_type.as_ref().unwrap() {
                DmaType::Write => self.read_data_port_buffer_complete(),
                DmaType::Read => self.write_data_port_buffer_complete(),
            };
            if self.state.buffer.is_none() && self.state.command.is_none() {
                self.state.pending_interrupt = true;
            }
        }
    }

    pub fn set_prd_exhausted(&mut self) {
        tracing::trace!("PRD has been exhausted");
        self.state.prd_exhausted = true;
    }

    pub fn handle_read_dma_descriptor_error(&mut self) -> bool {
        // Check if there's any pending IO
        // handle_io_completion might start another io (even though DMA engine has stalled), but we can let it complete before stopping DMA engine
        if self.io.is_none() {
            if self.state.pending_software_reset {
                self.reset();
            } else {
                self.state.buffer = None;
                self.state.command = None;
            }
            return true;
        }

        // yet to clear out dma_error
        false
    }

    fn handle_io_completion(&mut self, result: Result<(), DiskError>) {
        let command = self.state.command.as_ref().unwrap();
        tracing::trace!(io_type = ?command.io_type, path = %self.disk_path, ?result, "io completion");

        let result = match command.io_type {
            IoType::Read => match result {
                Ok(()) => self.read_media_sectors_complete(),
                Err(err) => Err(IdeError::IdeBadSector(err)),
            },
            IoType::Write => match result {
                Ok(()) => self.write_media_sectors_complete(),
                Err(err) => Err(IdeError::IdeBadSector(err)),
            },
            IoType::Flush => match result {
                Ok(()) => {
                    self.flush_complete();
                    Ok(())
                }
                Err(err) => {
                    // Supposed to return the first LBA that failed flush but we
                    // don't know what it is.
                    self.state
                        .regs
                        .seek(command.use_48bit_lba, &self.geometry, 0);
                    Err(IdeError::Flush(err))
                }
            },
        };

        if let Err(err) = result {
            self.log_and_update_error(err, None);
        }
    }

    /// Returns whether this device is currently selected.
    ///
    /// This will be false when this device is being targeted due to the other
    /// device being missing.
    fn is_selected(&self) -> bool {
        self.state.regs.device_head.dev() as u8 == self.disk_path.drive
    }

    fn status(&self) -> Status {
        if !self.is_selected() {
            // The drive was not selected, so don't return status for the wrong
            // drive. This is used to support configurations with only device 0
            // enabled.
            Status::new()
        } else {
            let mut status = Status::new();
            if self.state.pending_software_reset {
                status.set_bsy(true);
            } else if self.state.buffer.is_some() {
                // Set DRQ if there is an accessible buffer.
                status.set_drq(true);
                status.set_drdy(true);
            } else if self.state.command.is_some() {
                // Set BSY if there is no buffer and a command is pending.
                status.set_bsy(true);
            } else {
                status.set_drdy(true);
            }
            if self.state.error_pending {
                status.set_err(true);
            }
            // Always set DSC.
            status.set_dsc(true);
            status
        }
    }

    fn handle_command(&mut self, command: IdeCommand) -> Result<(), IdeError> {
        tracing::trace!(path = ?self.disk_path, ?command, "ide command");

        if self.state.command.is_some() {
            tracelimit::warn_ratelimited!(?command, "command is already pending");
            return Ok(());
        }

        if self.state.buffer.is_some() {
            tracelimit::warn_ratelimited!(?command, "data transfer is ongoing");
            return Ok(());
        }

        self.state.error_pending = false;
        self.state.regs.error = ErrorReg::new();

        let command = match command {
            IdeCommand::EXECUTE_DEVICE_DIAGNOSTIC => {
                self.state.regs.reset_signature();
                self.state.regs.error = ErrorReg::new().with_amnf_ili_default(true);
                return Ok(());
            }
            x if (IdeCommand::RECALIBRATE_START..=IdeCommand::RECALIBRATE_END).contains(&x) => {
                self.state.regs.seek28(&self.geometry, 0);
                None
            }
            IdeCommand::READ_SECTORS | IdeCommand::READ_SECTORS_ALT => {
                // Multi-sector reads require an interrupt after each sector,
                // even though the entire transfer consists of multiple sectors.
                Some(CommandState {
                    interrupt_after_each: true,
                    ..CommandState::read(self.state.regs.sector_count & 0xff)
                })
            }
            IdeCommand::READ_ONE_SECTOR | IdeCommand::READ_ONE_SECTOR_ALT => {
                Some(CommandState::read(1))
            }
            IdeCommand::READ_MULTI_SECTORS_EXT => Some(CommandState {
                interrupt_after_each: true,
                use_48bit_lba: true,
                ..CommandState::read(self.state.regs.sector_count)
            }),
            IdeCommand::READ_DMA_EXT => Some(CommandState {
                use_dma: true,
                use_48bit_lba: true,
                ..CommandState::read(self.state.regs.sector_count)
            }),
            IdeCommand::READ_MULTI_BLOCKS_EXT => Some(CommandState {
                use_48bit_lba: true,
                ..CommandState::read(self.state.regs.sector_count)
            }),
            IdeCommand::WRITE_SECTORS | IdeCommand::WRITE_SECTORS_ALT => Some(CommandState {
                interrupt_after_each: true,
                ..CommandState::write(self.state.regs.sector_count & 0xff)
            }),
            IdeCommand::WRITE_ONE_SECTOR | IdeCommand::WRITE_ONE_SECTOR_ALT => {
                Some(CommandState::write(1))
            }
            IdeCommand::WRITE_MULTI_SECTORS_EXT => {
                // Multi-sector writes require an interrupt after each sector
                // even though the entire transfer consists of multiple sectors.
                Some(CommandState {
                    interrupt_after_each: true,
                    use_48bit_lba: true,
                    ..CommandState::write(self.state.regs.sector_count & 0xff)
                })
            }
            IdeCommand::WRITE_DMA_EXT | IdeCommand::WRITE_DMA_FUA_EXT => Some(CommandState {
                use_dma: true,
                use_48bit_lba: true,
                fua: command == IdeCommand::WRITE_DMA_FUA_EXT,
                ..CommandState::write(self.state.regs.sector_count)
            }),
            IdeCommand::WRITE_MULTI_BLOCKS_EXT | IdeCommand::WRITE_MULTIPLE_BLOCKS_EXT_FUA => {
                // Multi-block writes are like multi-sector writes
                // but we don't need to interrupt after every sector.
                Some(CommandState {
                    use_48bit_lba: true,
                    fua: command == IdeCommand::WRITE_MULTIPLE_BLOCKS_EXT_FUA,
                    ..CommandState::write(self.state.regs.sector_count)
                })
            }
            IdeCommand::VERIFY_MULTI_SECTORS | IdeCommand::VERIFY_MULTI_SECTORS_ALT => {
                Some(CommandState::verify(self.state.regs.sector_count & 0xff))
            }
            IdeCommand::VERIFY_MULTI_SECTORS_EXT => Some(CommandState {
                use_48bit_lba: true,
                ..CommandState::verify(self.state.regs.sector_count)
            }),
            // Format a track. do nothing as we've never seen this command used in practice
            IdeCommand::FORMAT_TRACK => None,
            IdeCommand::FLUSH_CACHE => {
                if self.read_only {
                    None
                } else {
                    Some(CommandState::flush())
                }
            }
            IdeCommand::FLUSH_CACHE_EXT => {
                if self.read_only {
                    None
                } else {
                    Some(CommandState {
                        use_48bit_lba: true,
                        ..CommandState::flush()
                    })
                }
            }
            x if (IdeCommand::SEEK_START..=IdeCommand::SEEK_END).contains(&x) => None,
            IdeCommand::INIT_DRIVE_PARAMETERS => {
                // Sets the maximum sector and head number for the drive
                // as passed in the sector and drive/head register.
                self.state.max_sector = self.state.regs.sector_count as u8;
                self.state.max_head = self.state.regs.device_head.head();
                None
            }
            IdeCommand::SET_MULTI_BLOCK_MODE => {
                // Sets the block size (in number of sectors) for block-mode read/write operations.
                let sector_count = self.state.regs.sector_count as u8;
                if sector_count == 0
                    || sector_count as u16 > protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT
                {
                    self.state.block_sector_count = protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT;
                } else {
                    self.state.block_sector_count = sector_count.into();
                }
                None
            }
            IdeCommand::READ_DMA | IdeCommand::READ_DMA_ALT => Some(CommandState {
                use_dma: true,
                ..CommandState::read(self.state.regs.sector_count & 0xff)
            }),
            IdeCommand::WRITE_DMA | IdeCommand::WRITE_DMA_ALT => Some(CommandState {
                use_dma: true,
                ..CommandState::write(self.state.regs.sector_count & 0xff)
            }),
            // Checks whether the drive is actually spinning or idle.
            // Specify that drive is actively spinning (i.e. in "idle"
            // state vs. the "standby" state).
            IdeCommand::CHECK_POWER_MODE => {
                self.state.regs.sector_count = protocol::DEVICE_ACTIVE_OR_IDLE as u16;
                None
            }
            IdeCommand::IDENTIFY_DEVICE => {
                self.identify_device();
                None
            }
            IdeCommand::SET_FEATURES => {
                // TODO
                //
                // Saw in Gen 1 VM boot but this is likely not necessary because
                // agents should be able to configure caching directly on ASAP
                // controller from host or SOC side
                None
            }
            IdeCommand::SLEEP
            | IdeCommand::STANDBY
            | IdeCommand::IDLE
            | IdeCommand::IDLE_IMMEDIATE
            | IdeCommand::STANDBY_IMMEDIATE => None,
            command => {
                tracing::debug!(?command, "unknown command");
                self.state.error_pending = true;
                self.state.regs.error = ErrorReg::new().with_unknown_command(true);
                None
            }
        };

        if let Some(mut command) = command {
            // Adjuts the sector count to account for zero input.
            if command.sectors_remaining == 0 {
                if command.use_48bit_lba {
                    command.sectors_remaining = protocol::MAX_48BIT_SECTOR_COUNT;
                } else {
                    command.sectors_remaining = 0x100;
                }
            }

            match command.io_type {
                IoType::Read | IoType::Write => {
                    // Validate and set the LBA.
                    let total_sectors = self
                        .state
                        .regs
                        .capacity(command.use_48bit_lba, &self.geometry);

                    let lba = self.state.regs.lba(command.use_48bit_lba, &self.geometry)?;
                    if total_sectors < lba || total_sectors - lba < command.sectors_remaining as u64
                    {
                        return Err(IdeError::IdeBadLocation {
                            lba,
                            sector_count: command.sectors_remaining,
                            disk_sector_count: total_sectors,
                        });
                    }
                    command.next_lba = lba;
                }
                IoType::Flush => {}
            }

            let command = self.state.command.insert(command);
            match command.io_type {
                IoType::Read => self.read(),
                IoType::Write => self.write(),
                IoType::Flush => self.flush(),
            }
        } else {
            self.state.pending_interrupt = true;
        };

        Ok(())
    }

    /// IDENTIFY DEVICE command enables the host to receive parameter information
    /// from the device. The features structure is 256 words of device identification
    /// data that can be transferred to the host by reading the Data register.
    fn identify_device(&mut self) {
        let total_chs_sectors: u32 = self.geometry.sectors_per_track
            * self.geometry.cylinder_count
            * self.geometry.head_count;
        let (cylinders, heads, sectors_per_track) = if total_chs_sectors < protocol::MAX_CHS_SECTORS
        {
            (
                self.geometry.cylinder_count as u16,
                self.geometry.head_count as u16,
                self.geometry.sectors_per_track as u16,
            )
        } else {
            (0x3FFF, 16, 63)
        };

        let firmware_revision = if self.disk_path.channel == 0 {
            ".1.1 0  "
        } else {
            ".1.1 1  "
        }
        .as_bytes()[..]
            .try_into()
            .unwrap();

        let user_addressable_sectors =
            if self.geometry.total_sectors > (protocol::LBA_28BIT_MAX_SECTORS as u64) {
                protocol::LBA_28BIT_MAX_SECTORS
            } else {
                self.geometry.total_sectors as u32
            };

        let features = protocol::IdeFeatures {
            config_bits: 0x045A,
            cylinders,
            heads,
            unformatted_sectors_per_track: (protocol::HARD_DRIVE_SECTOR_BYTES
                * self.geometry.sectors_per_track)
                as u16,
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
            log_cylinders: self.geometry.cylinder_count as u16,
            log_heads: self.geometry.head_count as u16,
            log_sectors_per_track: self.geometry.sectors_per_track as u16,
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
            total_sectors_48_bit: self.geometry.total_sectors.into(),
            default_sector_size_config: 0x4000, // describes the sector size related info. Reflect the underlying device sector size and logical:physical ratio
            logical_block_alignment: 0x4000, // describes alignment of logical blocks within physical block
            ..FromZeros::new_zeroed()
        };

        self.command_buffer.buffer[..protocol::IDENTIFY_DEVICE_BYTES].atomic_write_obj(&features);

        self.state.buffer = Some(BufferState::new(
            protocol::IDENTIFY_DEVICE_BYTES as u32,
            None,
        ));
    }

    fn data_port_io(&mut self, mut io_type: IoPortData<'_>) {
        let Some(buffer_state) = self.state.buffer.as_mut() else {
            tracelimit::warn_ratelimited!("no buffer available");
            return;
        };

        let length = match io_type {
            IoPortData::Read(ref data) => {
                tracing::trace!(
                    cur_byte = buffer_state.current_byte,
                    total_bytes = buffer_state.total_bytes,
                    length = data.len(),
                    "data port read"
                );

                data.len()
            }
            IoPortData::Write(data) => {
                tracing::trace!(
                    cur_byte = buffer_state.current_byte,
                    total_bytes = buffer_state.total_bytes,
                    length = data.len(),
                    "data port write"
                );
                data.len()
            }
        } as u32;

        let current_buffer = &self.command_buffer.buffer[buffer_state.range()];

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
            }
        }

        if buffer_state.advance(length) {
            match io_type {
                IoPortData::Read(_) => self.read_data_port_buffer_complete(),
                IoPortData::Write(_) => self.write_data_port_buffer_complete(),
            }
        }
    }

    fn read_data_port_buffer_complete(&mut self) {
        let buffer = self.state.buffer.take().unwrap();
        assert!(buffer.is_empty());

        // If it wasn't a read operation (e.g. a "READ FEATURES") then
        // we don't need to do anything at completion time. If it was a
        // read operation then we may need to kick off another read.
        if matches!(
            self.state.command,
            Some(CommandState {
                io_type: IoType::Read,
                ..
            })
        ) {
            self.read();
        }
    }

    fn write_data_port_buffer_complete(&mut self) {
        let buffer = self.state.buffer.take().unwrap();
        assert!(buffer.is_empty());

        if matches!(
            self.state.command,
            Some(CommandState {
                io_type: IoType::Write,
                ..
            })
        ) {
            let command = self.state.command.as_ref().unwrap();
            let lba = command.next_lba;
            let size = buffer.total_bytes;
            let write_sector_count = size / protocol::HARD_DRIVE_SECTOR_BYTES;
            let fua = command.fua;

            let command_buffer = self.command_buffer.access();
            tracing::trace!(
                lba,
                sector_count = write_sector_count,
                "starting disk write"
            );
            self.set_io(|disk| async move {
                let buffers = command_buffer.buffers(0, size as usize, false);
                disk.write_vectored(&buffers, lba, fua).await
            });
        }
    }

    fn update_command_io_sector_count(&mut self) {
        let command = self.state.command.as_mut().unwrap();
        let sectors = if command.use_dma {
            command
                .sectors_remaining
                .min(protocol::MAX_SECTORS_MULT_TRANSFER_DEFAULT as u32)
        } else if command.interrupt_after_each {
            1
        } else {
            command
                .sectors_remaining
                .min(self.state.block_sector_count as u32)
        };
        command.sectors_before_interrupt = sectors;
    }

    /// Reads data from the attachment.
    fn read(&mut self) {
        self.update_command_io_sector_count();
        let command = self.state.command.as_ref().unwrap();
        let lba = command.next_lba;

        assert!(self.state.buffer.is_none());

        // Data is not in track cache. Kick off read on task thread.
        // Request interrupt after read completes on task thread.

        let command_buffer = self.command_buffer.access();

        let sector_count = command.sectors_before_interrupt;
        tracing::trace!(lba, sector_count, "starting disk read");
        self.set_io(|disk| async move {
            let buffers = command_buffer.buffers(
                0,
                protocol::HARD_DRIVE_SECTOR_BYTES as usize * sector_count as usize,
                true,
            );
            disk.read_vectored(&buffers, lba).await
        });
    }

    // This function is called when we are done reading from a hard drive image
    // asynchronously or if the data was already in the cache.
    fn read_media_sectors_complete(&mut self) -> Result<(), IdeError> {
        let command = self.state.command.as_mut().unwrap();

        // If there wasn't an error, the buffer is marked "in use" and we can read
        // directly out of it until we release it.
        let sectors_read = command.sectors_before_interrupt;

        assert!(sectors_read != 0);

        let use_dma = command.use_dma;
        let verify_only = command.verify_only;

        // Reduce sector count by number we just read to prepare for next read.
        if self.state.prd_exhausted {
            // If no more PRDs, no more to read
            command.sectors_remaining = 0;
        } else {
            command.sectors_remaining -= sectors_read;
        }
        command.next_lba += sectors_read as u64;

        // Update the last successful LBA in case the next read fails.
        self.state
            .regs
            .seek(command.use_48bit_lba, &self.geometry, command.next_lba);

        // Update the buffer.
        if verify_only || self.state.prd_exhausted {
            // If no more PRDs, no more to read
            self.state.buffer = None;
        } else {
            self.state.buffer = Some(BufferState::new(
                sectors_read * protocol::HARD_DRIVE_SECTOR_BYTES,
                use_dma.then_some(DmaType::Write),
            ));
        }

        if command.sectors_remaining == 0 {
            self.state.command = None;
        } else if verify_only {
            self.read();
        }

        if !use_dma {
            self.state.pending_interrupt = true;
        }
        Ok(())
    }

    // Updates drive state with information from the current write. This function
    // does not actually write to disk. Rather, the data is received in subsequent
    // data port writes.
    fn write(&mut self) {
        self.update_command_io_sector_count();
        let command = self.state.command.as_mut().unwrap();
        assert!(self.state.buffer.is_none());

        // Specify that the buffer is ready to receive bytes.
        self.state.buffer = Some(BufferState::new(
            command.sectors_before_interrupt * protocol::HARD_DRIVE_SECTOR_BYTES,
            command.use_dma.then_some(DmaType::Read),
        ));
    }

    // returns true if interrupt should be requested
    fn write_media_sectors_complete(&mut self) -> Result<(), IdeError> {
        let command = self.state.command.as_mut().unwrap();
        let sectors_written = command.sectors_before_interrupt;
        command.sectors_remaining -= sectors_written;
        command.next_lba += sectors_written as u64;
        // Update the registers with the last written LBA.
        self.state
            .regs
            .seek(command.use_48bit_lba, &self.geometry, command.next_lba);

        if command.sectors_remaining > 0 {
            if !command.use_dma {
                self.state.pending_interrupt = true;
            }
            self.write();
        } else {
            self.state.command = None;
            self.state.pending_interrupt = true;
        }

        Ok(())
    }

    fn flush(&mut self) {
        self.set_io(|disk| async move { disk.sync_cache().await });
    }

    fn flush_complete(&mut self) {
        self.state.command = None;
        self.state.pending_interrupt = true;
    }

    // Returns true if an interrupt should be asserted. Interrupts should be asserted
    // for all ATA defined IDE errors. All other errors are traced with ERROR level
    // and no interrupt is asserted.
    fn log_and_update_error(&mut self, error: IdeError, register: Option<DriveRegister>) {
        let ide_error = match error {
            IdeError::IdeBadLocation { .. } | IdeError::ZeroSector | IdeError::LbaBitNotSet => {
                ErrorReg::new().with_bad_location(true)
            }
            IdeError::IdeBadSector { .. } => ErrorReg::new().with_bad_sector(true),
            IdeError::Flush { .. } => ErrorReg::new().with_unknown_command(true), // strange, but matches Hyper-V behavior
        };

        tracelimit::warn_ratelimited!(
            path = %self.disk_path,
            error = error.as_error(),
            ?register,
            "io port failure"
        );
        self.state.regs.error = ide_error;
        self.state.error_pending = true;
        self.state.command = None;
        self.state.buffer = None;
        self.state.pending_interrupt = true;
    }

    /// Sets the asynchronous IO to be polled in `poll_device`.
    fn set_io<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Disk) -> Fut,
        Fut: 'static + Future<Output = Result<(), DiskError>> + Send,
    {
        let fut = (f)(self.disk.clone());
        assert!(self.io.is_none());
        self.io = Some(Io(Box::pin(fut)));
        // Ensure poll_device gets called again.
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

pub(crate) mod save_restore {
    use self::state::SavedDmaType;
    use self::state::SavedRegisterState;
    use super::*;
    use std::sync::atomic::Ordering;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;

    pub mod state {
        use mesh::payload::Protobuf;

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.hdd")]
        pub struct SavedRegisterState {
            #[mesh(1)]
            pub error: u8,
            #[mesh(2)]
            pub features: u8,
            #[mesh(3)]
            pub device_head: u8,
            #[mesh(4)]
            pub lba_low: u16,
            #[mesh(5)]
            pub lba_mid: u16,
            #[mesh(6)]
            pub lba_high: u16,
            #[mesh(7)]
            pub sector_count: u16,
            #[mesh(8)]
            pub device_control_reg: u8,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.hdd")]
        pub enum SavedIoType {
            #[mesh(1)]
            Read,
            #[mesh(2)]
            Write,
            #[mesh(3)]
            Flush,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.hdd")]
        pub struct SavedCommandState {
            #[mesh(1)]
            pub io_type: SavedIoType,
            #[mesh(2)]
            pub interrupt_after_each: bool,
            #[mesh(3)]
            pub use_48bit_lba: bool,
            #[mesh(4)]
            pub use_dma: bool,
            #[mesh(5)]
            pub fua: bool,
            #[mesh(6)]
            pub verify_only: bool,
            #[mesh(7)]
            pub next_lba: u64,
            #[mesh(8)]
            pub sectors_remaining: u32,
            #[mesh(9)]
            pub sectors_before_interrupt: u32,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.hdd")]
        pub enum SavedDmaType {
            #[mesh(1)]
            Read,
            #[mesh(2)]
            Write,
        }

        #[derive(Protobuf)]
        #[mesh(package = "storage.ide.device.hdd")]
        pub struct SavedHardDriveState {
            #[mesh(1)]
            pub registers: SavedRegisterState,

            // Miscellaneous state
            #[mesh(2)]
            pub pending_interrupt: bool,
            #[mesh(3)]
            pub max_sector: u8,
            #[mesh(4)]
            pub max_head: u8,
            #[mesh(5)]
            pub error_pending: bool,
            #[mesh(6)]
            pub block_sector_count: u16,

            // Command state
            #[mesh(7)]
            pub command: Option<SavedCommandState>,

            // Buffer state
            #[mesh(8)]
            pub dma_type: Option<SavedDmaType>,
            #[mesh(9)]
            pub command_buffer: Vec<u8>,

            // Software reset state
            #[mesh(10)]
            pub pending_software_reset: bool,

            // Whether PRDs have been exhausted for current operation
            #[mesh(11)]
            pub prd_exhausted: bool,
        }
    }

    impl HardDrive {
        pub fn save(&self) -> Result<state::SavedHardDriveState, SaveError> {
            let DriveState {
                regs:
                    Registers {
                        error,
                        features,
                        device_head,
                        lba_low,
                        lba_mid,
                        lba_high,
                        sector_count,
                        device_control_reg,
                    },
                pending_software_reset,
                pending_interrupt,
                max_sector,
                max_head,
                error_pending,
                block_sector_count,
                command,
                buffer,
                prd_exhausted,
            } = &self.state;

            let command_buffer = if let Some(buffer_state) = &self.state.buffer {
                self.command_buffer.buffer[buffer_state.range()]
                    .iter()
                    .map(|val| val.load(Ordering::Relaxed))
                    .collect()
            } else {
                Vec::new()
            };

            Ok(state::SavedHardDriveState {
                registers: SavedRegisterState {
                    error: error.into_bits(),
                    features: *features,
                    device_head: (*device_head).into(),
                    lba_low: *lba_low,
                    lba_mid: *lba_mid,
                    lba_high: *lba_high,
                    sector_count: *sector_count,
                    device_control_reg: device_control_reg.into_bits(),
                },
                pending_interrupt: *pending_interrupt,
                max_sector: *max_sector,
                max_head: *max_head,
                error_pending: *error_pending,
                block_sector_count: *block_sector_count,
                command: command.as_ref().map(|cmd| {
                    let CommandState {
                        io_type,
                        interrupt_after_each,
                        use_48bit_lba,
                        use_dma,
                        fua,
                        verify_only,
                        next_lba,
                        sectors_remaining,
                        sectors_before_interrupt,
                    } = cmd;

                    state::SavedCommandState {
                        io_type: match io_type {
                            IoType::Read => state::SavedIoType::Read,
                            IoType::Write => state::SavedIoType::Write,
                            IoType::Flush => state::SavedIoType::Flush,
                        },
                        interrupt_after_each: *interrupt_after_each,
                        use_48bit_lba: *use_48bit_lba,
                        use_dma: *use_dma,
                        fua: *fua,
                        verify_only: *verify_only,
                        next_lba: *next_lba,
                        sectors_remaining: *sectors_remaining,
                        sectors_before_interrupt: *sectors_before_interrupt,
                    }
                }),
                dma_type: match buffer {
                    Some(buffer_state) => buffer_state.dma_type.as_ref().map(|dma| match dma {
                        DmaType::Read => SavedDmaType::Read,
                        DmaType::Write => SavedDmaType::Write,
                    }),
                    None => None,
                },
                command_buffer,
                pending_software_reset: *pending_software_reset,
                prd_exhausted: *prd_exhausted,
            })
        }

        pub fn restore(&mut self, state: state::SavedHardDriveState) -> Result<(), RestoreError> {
            let state::SavedHardDriveState {
                registers:
                    SavedRegisterState {
                        error,
                        features,
                        device_head,
                        lba_low,
                        lba_mid,
                        lba_high,
                        sector_count,
                        device_control_reg,
                    },
                pending_interrupt,
                max_sector,
                max_head,
                error_pending,
                block_sector_count,
                command,
                dma_type,
                command_buffer,
                pending_software_reset,
                prd_exhausted,
            } = state;

            self.state = DriveState {
                regs: Registers {
                    error: error.into(),
                    features,
                    device_head: device_head.into(),
                    lba_low,
                    lba_mid,
                    lba_high,
                    sector_count,
                    device_control_reg: DeviceControlReg::from_bits(device_control_reg),
                },
                pending_software_reset,
                pending_interrupt,
                max_sector,
                max_head,
                error_pending,
                block_sector_count,
                command: command.map(|cmd| {
                    let state::SavedCommandState {
                        io_type,
                        interrupt_after_each,
                        use_48bit_lba,
                        use_dma,
                        fua,
                        verify_only,
                        next_lba,
                        sectors_remaining,
                        sectors_before_interrupt,
                    } = cmd;

                    CommandState {
                        io_type: match io_type {
                            state::SavedIoType::Read => IoType::Read,
                            state::SavedIoType::Write => IoType::Write,
                            state::SavedIoType::Flush => IoType::Flush,
                        },
                        interrupt_after_each,
                        use_48bit_lba,
                        use_dma,
                        fua,
                        verify_only,
                        next_lba,
                        sectors_remaining,
                        sectors_before_interrupt,
                    }
                }),
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
                prd_exhausted,
            };

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DriveState;
    use super::MediaGeometry;
    use super::Registers;

    #[test]
    fn test_lba() {
        let geometry = MediaGeometry::new(0x7ff_ffff, 512).unwrap();

        let roundtrip_48 = |regs: &mut Registers, lba| {
            regs.device_head.set_lba(true);
            regs.seek48(lba);
            assert_eq!(regs.lba48().unwrap(), lba);
        };

        let roundtrip_28 = |regs: &mut Registers, lba| {
            regs.device_head.set_lba(true);
            assert!(lba < regs.capacity28(&geometry));
            regs.seek28(&geometry, lba);
            assert_eq!(regs.lba28(&geometry).unwrap(), lba);
        };

        let roundtrip_chs = |regs: &mut Registers, lba| {
            regs.device_head.set_lba(false);
            assert!(lba < regs.capacity28(&geometry));
            regs.seek28(&geometry, lba);
            assert_eq!(regs.lba28(&geometry).unwrap(), lba);
        };

        let mut state = DriveState::new();
        roundtrip_48(&mut state.regs, 0);
        roundtrip_48(&mut state.regs, 0x10000);
        roundtrip_48(&mut state.regs, 0x12345678abcd);

        roundtrip_28(&mut state.regs, 0);
        roundtrip_28(&mut state.regs, 0x10000);
        roundtrip_28(&mut state.regs, 0x678abcd);

        roundtrip_chs(&mut state.regs, 0);
        roundtrip_chs(&mut state.regs, 0x10000);
        roundtrip_chs(&mut state.regs, 0xfbffff);
    }
}
