// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Emulator for the Intel 82077AA CHMOS Single-Chip Floppy Disk Controller.
//!
//! Some notable limitations of the current implementation:
//!
//! - no support for more than one attached floppy drive
//! - no support for hot-add/remove of floppy disks
//!
//! While there's no _pressing_ need to address these limitations, it would
//! certainly be _cool_ if we could implement that functionality at some point.
//!
//! # Accuracy
//!
//! This emulator is not 100% accurate, and does not implement all documented
//! features of the 82077AA floppy disk controller. Rather, it implements a
//! "pragmatic subset" of features that allow it to have "good-enough"
//! compatibility with both modern and legacy operating systems.
//!
//! New features are only added on a case-by-case basis whenever a particular
//! bit of software happens to require it.
//
// DEVNOTE: this implementation began life as a straight port of the existing
// C++ code from Hyper-V, and while there has been some effort put into
// reorganizing and refactoring the code to be more Rust-y, there's still quite
// a ways to go.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use self::floppy_sizes::FloppyImageType;
use self::protocol::FloppyCommand;
use self::protocol::RegisterOffset;
use self::protocol::FLOPPY_TOTAL_CYLINDERS;
use self::protocol::INVALID_COMMAND_STATUS;
use self::protocol::STANDARD_FLOPPY_SECTOR_SIZE;
use arrayvec::ArrayVec;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use core::sync::atomic::Ordering;
use disk_backend::SimpleDisk;
use guestmem::ranges::PagedRange;
use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::isa_dma_channel::IsaDmaChannel;
use vmcore::isa_dma_channel::IsaDmaDirection;
use vmcore::line_interrupt::LineInterrupt;

mod floppy_sizes {
    use super::protocol::FLOPPY_TOTAL_CYLINDERS;
    use super::protocol::STANDARD_FLOPPY_SECTOR_SIZE;

    const HDMSS_SECTORS_PER_TRACK: u8 = 23;
    const DMF_SECTORS_PER_TRACK: u8 = 21;
    const HD_SECTORS_PER_TRACK: u8 = 18;
    const MD_SECTORS_PER_TRACK: u8 = 15;
    const LD_SECTORS_PER_TRACK: u8 = 9;

    const fn calculate_image_size(sectors_per_track: u8) -> u64 {
        sectors_per_track as u64
            * STANDARD_FLOPPY_SECTOR_SIZE as u64
            * FLOPPY_TOTAL_CYLINDERS as u64
            * 2
    }

    const HDMSS_FLOPY_IMAGE_SIZE: u64 = calculate_image_size(HDMSS_SECTORS_PER_TRACK);
    const DMF_FLOPPY_IMAGE_SIZE: u64 = calculate_image_size(DMF_SECTORS_PER_TRACK);
    const HD_FLOPPY_IMAGE_SIZE: u64 = calculate_image_size(HD_SECTORS_PER_TRACK);
    const MD_FLOPPY_IMAGE_SIZE: u64 = calculate_image_size(MD_SECTORS_PER_TRACK);
    const LD_FLOPPY_IMAGE_SIZE: u64 = calculate_image_size(LD_SECTORS_PER_TRACK);
    const LDSS_FLOPPY_IMAGE_SIZE: u64 = calculate_image_size(LD_SECTORS_PER_TRACK) / 2;

    pub enum FloppyImageType {
        /// Low-density disks, single sided (360Kb)
        LowDensitySingleSided,
        /// Low-density disks (720Kb)
        LowDensity,
        /// Medium-density disks (1.2Mb)
        MediumDensity,
        /// High-density disks (1.44MB)
        HighDensity,
        /// DMF (distribution media format) disks (1.68Mb)
        Dmf,
        /// High-density Multiple Sector Size (MSS) used by eXtended
        /// Distribution Format (XDF) (1.72Mb)
        HighDensityMss,
    }

    impl FloppyImageType {
        pub fn sectors(&self) -> u8 {
            match self {
                FloppyImageType::LowDensity => LD_SECTORS_PER_TRACK,
                FloppyImageType::HighDensity => HD_SECTORS_PER_TRACK,
                FloppyImageType::Dmf => DMF_SECTORS_PER_TRACK,
                FloppyImageType::LowDensitySingleSided => LD_SECTORS_PER_TRACK,
                FloppyImageType::MediumDensity => MD_SECTORS_PER_TRACK,
                FloppyImageType::HighDensityMss => HDMSS_SECTORS_PER_TRACK,
            }
        }

        pub fn from_file_size(file_size: u64) -> Option<Self> {
            let res = match file_size {
                HD_FLOPPY_IMAGE_SIZE => FloppyImageType::HighDensity,
                DMF_FLOPPY_IMAGE_SIZE => FloppyImageType::Dmf,
                LD_FLOPPY_IMAGE_SIZE => FloppyImageType::LowDensity,
                MD_FLOPPY_IMAGE_SIZE => FloppyImageType::MediumDensity,
                LDSS_FLOPPY_IMAGE_SIZE => FloppyImageType::LowDensitySingleSided,
                HDMSS_FLOPY_IMAGE_SIZE => FloppyImageType::HighDensityMss,
                _ => return None,
            };
            Some(res)
        }
    }
}

mod protocol {
    use bitfield_struct::bitfield;
    use inspect::Inspect;
    use open_enum::open_enum;

    pub const FIFO_SIZE: usize = 16;

    pub const INVALID_COMMAND_STATUS: u8 = 0x80; // returned by e.g., SENSE_INTERRUPT_STATUS on err

    pub const STANDARD_FLOPPY_SECTOR_SIZE: usize = 512;
    pub const FLOPPY_TOTAL_CYLINDERS: u8 = 80;

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct InputRegister {
        #[bits(2)]
        pub drive_select: u8,
        #[bits(1)]
        pub head: u8,
        #[bits(5)]
        unused2: u8,
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct StatusRegister0 {
        #[bits(2)]
        pub drive_select: u8,
        #[bits(1)]
        pub head: u8,
        #[bits(2)]
        unused: u8,
        pub seek_end: bool,
        pub abnormal_termination: bool,
        pub invalid_command: bool,
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct StatusRegister1 {
        pub missing_address: bool,
        pub write_protected: bool,
        pub no_data: bool,
        #[bits(5)]
        unused: u8,
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct StatusRegister2 {
        pub missing_address: bool,
        pub bad_cylinder: bool,
        #[bits(6)]
        unused: u8,
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct StatusRegister3 {
        #[bits(2)]
        pub drive_select: u8,
        #[bits(1)]
        pub head: u8,
        pub unused1: bool, // This bit is always 1
        pub track0: bool,
        pub unused2: bool, // This bit is always 1
        pub write_protected: bool,
        pub unused3: bool, // This bit is always 0
    }

    open_enum! {
        #[derive(Default)]
        pub enum RegisterOffset: u16 {
            STATUS_A = 0, // Read-only
            STATUS_B = 1, // Read-only
            DIGITAL_OUTPUT = 2,
            TAPE_DRIVE = 3, // Obsolete
            MAIN_STATUS = 4, // Read-only
            DATA_RATE = 4, // Write-only
            DATA = 5,
            DIGITAL_INPUT = 7,// Read-only
            CONFIG_CONTROL = 7, // Write-only
        }
    }

    /// Floppy DOR - digital output register (read/write)
    // Drive Select bits is [1:0]
    // Reset bits is        [2]
    // Not DMA Gate bit is  [3]
    // Motor enable bits is [7:4]. Each bit for EN0, EN1, ..., EN4
    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct DigitalOutputRegister {
        // A good item to note are the drive activation (drive select and motor enable) values:
        // DOR value= 0x1C for drive= 0,
        // DOR value= 0x2D for drive= 1,
        // DOR value= 0x4E for drive= 2,
        // DOR value= 0x8F for drive= 3,
        #[bits(2)]
        pub _drive_select: u8,

        // effectively, `not reset` 1 is true, 0 is false (meaning resetting)
        pub controller_enabled: bool,

        // bit high only in PC-AT and Model 30 modes
        pub dma_enabled: bool,

        // This is really 4 separate bools, but for our convenience we treat
        // it as a large number (one-hot encoding).
        #[bits(4)]
        pub motors_active: u8,
    }

    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub enum DataDirection {
        /// Write to guest memory. Also to indicate the
        /// direction of a data transfer (0 indicates a
        /// write is required -- an inward FIFO
        /// direction).
        Write = 0,
        /// Read from guest memory.  Also to indicate the
        /// direction of a data transfer (1 indicates a
        /// read is required -- an outward FIFO
        /// direction).
        Read = 1,
    }

    impl DataDirection {
        pub fn as_bool(self) -> bool {
            match self {
                Self::Write => false,
                Self::Read => true,
            }
        }
    }

    /// Floppy MSR - main status register (read-only)
    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct MainStatusRegister {
        // This is really 4 separate bools, but for our convenience we treat
        // it as a large number. E.g. one-hot encoded for DRV0, ..., DRV3
        #[bits(4)]
        pub active_drives: u8,
        /// Indicates if the controller is currently executing a command
        pub busy: bool,
        /// Non DMA mode is not supported
        pub non_dma_mode: bool,
        /// Data input/output (1 - output data to CPU (read), 0 - receive data from CPU (write)).
        /// Holds no meaning if main_request is not set.
        pub data_direction: bool, // DataDirection
        /// Indicates whether controller is ready to receive or send
        /// data or commands via the data registers
        pub main_request: bool,
    }

    /// Floppy DIR - digital input register (read-only)
    // e.g., return current data-rate set via ConfigControl
    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct DigitalInputRegister {
        // in PC-AT, all bits except for msb always tristated
        #[bits(7)]
        pub tristated: u8,

        #[bits(1)]
        pub disk_change: bool,
    }

    open_enum! {
        #[derive(Default)]
        // #[inspect(debug)]
        /// RECALIBRATE, SEEK, RELATIVE SEEK generate interrupts but do not clear
        /// the signal themselves. The rest don't forget to clear if applicable.
        pub enum FloppyCommand: u8 {
            // high nibble may be 6, C, or E, based on bit values
            // for MT, MFM, and SK
            // READ_DATA = 0x06,
            // READ_DEL_DATA = 0x0C,
            // WRITE_DATA = 0x05,
            // WRITE_DEL_DATA = 0x09,
            // READ_TRACK = 0x02,
            VERIFY = 0x16,
            VERIFY2 = 0xF6,
            /// Just checks if controller is newer/enhanced type, or old type.
            /// Return value of 0x90 indicates enhanced type.
            VERSION = 0x10,
            FORMAT_TRACK = 0x4D,
            FORMAT_DOUBLE_DENSITY_MODE = 0xCD,
            // SCAN_EQUAL = 0x11,
            SCAN_EQUAL_ALL = 0xD1,
            SCAN_EQUAL = 0xF1,
            SCAN_LOW_OR_EQUAL = 0x19,
            SCAN_HIGH_OR_EQUAL = 0x1D,
            /// Recalibrate command moves the read/write head back to position on
            /// track 0. On physical floppy disk, there is a TRACK0 pin that goes
            /// high when head reaches track 0. If disk has more than something
            /// like 80 tracks, recalibrate would be needed to be called multiple
            /// times (command simply does e.g., 79 steps via stepper motor pulse,
            /// checking if each track is track 0)
            ///
            /// SENSE_INTERRUPT_STATUS must immediately follow, due to RECALIBRATE
            /// not having result phase of its own in original design to lower
            /// interrupt signal.
            RECALIBRATE = 0x07,
            /// Will clear interrupt signal, and determine what raised the
            /// interrupt. Returns 0x80 if command issued when there are no
            /// active interrupts.
            ///
            /// Must be called directly after RECALIBRATE and either type of SEEK.
            SENSE_INTERRUPT_STATUS = 0x08,
            /// Provide the Stepping Rate Time (SRT) to be used to set the rate at
            /// which step pulses are issued to move between tracks during a SEEK
            /// or RECALIBRATE. Also sets initial values for Head Unload Timer
            /// (HUT), and Head Load Time (HLT). HUT defines time from end of
            /// execution to head unload state, and HLT defines time between signal
            /// for R/W operation raised, and operation begin.
            SPECIFY = 0x03,
            /// Simply returns drive state information. Directly proceeds to result
            /// phase (e.g., no execution phase).
            SENSE_DRIVE_STATUS = 0x04,
            DRIVE_SPECIFICATION_COMMAND = 0x8E,
            /// SEEK command moves the read/write head from track to track. Using
            /// correct technicalities, the terms `track` and `cylinder` are some-
            /// what synonymous. Consider a physical floppy disk -- it is a disk
            /// with two sides. Each side is called a head. The concentric rings
            /// that are on each head are called tracks. Each head has e.g., a
            /// track 18. Together these two track 18s form a cylinder. But, if
            /// we are to only use one head of the disk, then cylinder and track
            /// are the same thing. SEEK effectively moves the read/write head
            /// from the PCN (present cylinder number) to the NCN (new / desired
            /// cylinder number). Here, the words cylinder and track mean the same.
            ///
            /// SENSE_INTERRUPT_STATUS must immediately follow, due to SEEK not
            /// having result phase of its own in original design to lower interrupt
            /// signal.
            SEEK = 0x0F,
            /// Enables various special features. Don't need by default, probably :)
            /// E.g., Disable FIFO, disable polling
            CONFIGURE = 0x13,
            /// Similar to SEEK, except instead of providing NCN to move R/W head
            /// to, provide an RCN (relative cylinder number), to move n tracks
            /// out/in (specified by a direction bit 0/1 DIR) from PCN.
            RELATIVE_SEEK_IN = 0xCF,
            RELATIVE_SEEK_OUT = 0x8F,
            /// Debug reasons
            DUMP_REGISTERS = 0x0E,
            READ_ID = 0x4A, // 4 for double density mode
            /// Perpendicular Recording Mode classically is support for orienting
            /// the the magnetic bits vertically instead of horizontally, thereby
            /// being able to pack more data bits for the same area. Toggling
            /// this mode in theory determines whether or not to interface with a
            /// perpendicular recoding floppy drive. A 1 Mbps datarate is needed,
            /// and all other commands here will function the same regardless.
            PERP288_MODE = 0x12,
            /// Set LOCK bit to 0.
            ///
            /// If LOCK bit is 1, then software resets by DOR/DSR will have no
            /// effect any parameter values set by CONFIGURE. Hardware reset will
            /// override and reset parameters.
            UNLOCK_FIFO_FUNCTIONS = 0x14,
            /// Set LOCK bit to 1.
            LOCK_FIFO_FUNCTIONS = 0x94,
            /// Only purpose is really for problem reporting.
            PART_ID = 0x18,
            POWERDOWN_MODE = 0x17,
            OPTION = 0x33,
            SAVE = 0x2E,
            RESTORE = 0x4E,
            FORMAT_AND_WRITE = 0xAD,

            EXIT_STANDBY_MODE = 0x34,
            GOTO_STANDBY_MODE = 0x35,
            HARD_RESET = 0x36,
            READ_TRACK = 0x42,
            SEEK_AND_WRITE = 0x45,
            SEEK_AND_READ = 0x46,
            ALT_SEEK_AND_READ = 0x66,
            WRITE_DATA = 0xC5,
            READ_NORMAL_DEL_DATA = 0xC6,
            WRITE_DEL_DATA = 0xC9,
            READ_DEL_DATA = 0xCC,
            WRITE_NORMAL_DATA = 0xE5, // Nonstandard command used by BeOS
            READ_NORMAL_DATA = 0xE6,

            INVALID = 0x00,
        }
    }

    impl FloppyCommand {
        // Floppy commands are written one byte at a time to the DATA register. The
        // first byte specifies the issued command. The remaining bytes are used as
        // inputs for the command. AKA, number of parameters for particular command
        pub fn input_bytes_needed(&self) -> usize {
            // Add one to account for the command byte itself
            1 + match *self {
                Self::READ_DEL_DATA => 8,
                Self::WRITE_DATA => 8,
                Self::WRITE_DEL_DATA => 8,
                Self::READ_TRACK => 8,
                Self::VERIFY => 8,
                Self::VERSION => 0,
                Self::FORMAT_TRACK => 5,
                Self::FORMAT_DOUBLE_DENSITY_MODE => 5,
                Self::SCAN_EQUAL_ALL => 8,
                Self::SCAN_EQUAL => 8,
                Self::SCAN_LOW_OR_EQUAL => 8,
                Self::SCAN_HIGH_OR_EQUAL => 8,
                Self::RECALIBRATE => 1,
                Self::SENSE_INTERRUPT_STATUS => 0,
                Self::SPECIFY => 2,
                Self::SENSE_DRIVE_STATUS => 1,
                Self::DRIVE_SPECIFICATION_COMMAND => 6,
                Self::SEEK => 2,
                Self::CONFIGURE => 3,
                Self::RELATIVE_SEEK_IN => 2,
                Self::RELATIVE_SEEK_OUT => 2,
                Self::DUMP_REGISTERS => 0,
                Self::READ_ID => 1,
                Self::PERP288_MODE => 1,
                Self::UNLOCK_FIFO_FUNCTIONS => 0,
                Self::LOCK_FIFO_FUNCTIONS => 0,
                Self::PART_ID => 0,
                Self::POWERDOWN_MODE => 1,
                Self::OPTION => 1,
                Self::SAVE => 0,
                Self::RESTORE => 16,
                Self::FORMAT_AND_WRITE => 5,

                // Self::EXIT_STANDBY_MODE =>,
                // Self::GOTO_STANDBY_MODE =>,
                // Self::HARD_RESET =>,
                // Self::READ_TRACK =>,
                Self::SEEK_AND_WRITE => 8,
                Self::SEEK_AND_READ => 8,
                Self::ALT_SEEK_AND_READ => 8,
                Self::READ_NORMAL_DEL_DATA => 8,
                Self::WRITE_NORMAL_DATA => 8,
                Self::READ_NORMAL_DATA => 8,

                // Self::INVALID => ..,
                _ => 0,
            }
        }

        pub fn result_bytes_expected(&self) -> usize {
            match *self {
                Self::READ_DEL_DATA => 7,
                Self::WRITE_DATA => 7,
                Self::WRITE_DEL_DATA => 7,
                Self::READ_TRACK => 7,
                Self::VERIFY => 7,
                Self::VERSION => 1,
                Self::FORMAT_TRACK => 7,
                Self::FORMAT_DOUBLE_DENSITY_MODE => 7,
                Self::SCAN_EQUAL_ALL => 7,
                Self::SCAN_EQUAL => 7,
                Self::SCAN_LOW_OR_EQUAL => 7,
                Self::SCAN_HIGH_OR_EQUAL => 7,
                Self::RECALIBRATE => 2,
                Self::SENSE_INTERRUPT_STATUS => 2,
                Self::SPECIFY => 0,
                Self::SENSE_DRIVE_STATUS => 1,
                Self::DRIVE_SPECIFICATION_COMMAND => 0,
                Self::SEEK => 2, // TODO: 0?
                Self::CONFIGURE => 0,
                Self::RELATIVE_SEEK_IN => 2,  // TODO: 0?
                Self::RELATIVE_SEEK_OUT => 2, // TODO: 0?
                Self::DUMP_REGISTERS => 10,
                Self::READ_ID => 7,
                Self::PERP288_MODE => 0,
                Self::UNLOCK_FIFO_FUNCTIONS => 1,
                Self::LOCK_FIFO_FUNCTIONS => 1,
                Self::PART_ID => 1,
                Self::POWERDOWN_MODE => 1,
                Self::OPTION => 1,
                Self::SAVE => 16,
                Self::RESTORE => 0,
                Self::FORMAT_AND_WRITE => 7,

                // Self::EXIT_STANDBY_MODE =>,
                // Self::GOTO_STANDBY_MODE =>,
                // Self::HARD_RESET =>,
                Self::SEEK_AND_WRITE => 7,
                Self::SEEK_AND_READ => 7,
                Self::ALT_SEEK_AND_READ => 7,
                Self::READ_NORMAL_DEL_DATA => 7,
                Self::WRITE_NORMAL_DATA => 7,
                Self::READ_NORMAL_DATA => 7,

                Self::INVALID => 1,
                _ => 0,
            }
        }
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct SpecifyParam1 {
        #[bits(4)]
        pub head_unload_timer: u8,
        #[bits(4)]
        pub step_rate_time: u8,
    }

    #[derive(Inspect)]
    #[bitfield(u8)]
    pub struct SpecifyParam2 {
        #[bits(7)]
        pub head_load_timer: u8,
        pub dma_disabled: bool,
    }
}

const MAX_CMD_BUFFER_BYTES: usize = 64 * 1024;

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
            memory: GuestMemory::new("floppy_buffer", self.buffer.clone()),
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

struct Io(Pin<Box<dyn Send + Future<Output = Result<(), disk_backend::DiskError>>>>);

impl ChangeDeviceState for FloppyDiskController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.reset(false);
    }
}

impl ChipsetDevice for FloppyDiskController {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for FloppyDiskController {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(io) = self.io.as_mut() {
            if let Poll::Ready(result) = io.0.as_mut().poll(cx) {
                self.io = None;
                self.handle_io_completion(result);
            }
        }
        self.waker = Some(cx.waker().clone());
    }
}

impl PortIoIntercept for FloppyDiskController {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let offset = RegisterOffset(io_port % 0x10);
        data[0] = match offset {
            // This port is completely unsupported by latest floppy controllers.
            RegisterOffset::STATUS_A => 0xFF,
            // Also unsupported but return 0xFC to indicate no tape drives present.
            RegisterOffset::STATUS_B => 0xFC,
            // Do nothing. This port is obsolete.
            RegisterOffset::TAPE_DRIVE => 0xFF,
            RegisterOffset::DIGITAL_OUTPUT => self.state.digital_output.into(),
            RegisterOffset::MAIN_STATUS => {
                // Indicate data register is ready for reading/writing.
                // manifests as 0x80 (or something else with msb high)
                if self.state.digital_output.controller_enabled() {
                    self.state.main_status.into()
                } else {
                    0
                }
            }
            RegisterOffset::DATA => {
                // If there are more bytes left to read then read them out now.
                let active_drive = self.state.main_status.active_drives();
                let io_direction = self.state.main_status.data_direction();
                tracing::trace!(?active_drive, ?io_direction, "DATA io read state");

                if let Some(result) = self.state.output_bytes.pop() {
                    self.state.main_status.set_active_drives(0);
                    if self.state.output_bytes.is_empty() {
                        // Reverse direction, now ready to receive a new command
                        self.state.main_status = (self.state.main_status)
                            .with_non_dma_mode(false)
                            .with_busy(false)
                            .with_main_request(true)
                            .with_data_direction(protocol::DataDirection::Write.as_bool());
                    }
                    result
                } else {
                    INVALID_COMMAND_STATUS
                }
            }

            // This port returns a value in the high bit if a floppy is
            // missing or has changed since the last command.
            RegisterOffset::DIGITAL_INPUT => {
                // The bottom seven bits are tristated, and always read as
                // ones on a real floppy controller (in PC-AT mode).
                let val = protocol::DigitalInputRegister::new()
                    .with_tristated(0x7f)
                    .with_disk_change(
                        self.state.digital_output.motors_active() != 0
                            && (!self.state.internals.floppy_present
                                || self.state.internals.floppy_changed),
                    );

                val.into()
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        tracing::trace!(?offset, ?data, "floppy pio read");

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let data = data[0];
        let offset = RegisterOffset(io_port % 0x10);
        tracing::trace!(?offset, ?data, "floppy pio write");
        match offset {
            RegisterOffset::STATUS_A | RegisterOffset::STATUS_B => {
                tracelimit::warn_ratelimited!(
                    ?data,
                    ?offset,
                    "write to read-only floppy status register"
                );
            }
            RegisterOffset::TAPE_DRIVE => {
                tracing::debug!(?data, "write to obsolete tape drive register");
            } // Do nothing. This port is obsolete.
            RegisterOffset::CONFIG_CONTROL => {
                // This controls the data transfer rate which is not
                // interesting to us. We will just ignore it.
                tracing::debug!(?data, "write to control register");
            }
            RegisterOffset::DATA_RATE => {
                const FLOPPY_DSR_DISK_RESET_MASK: u8 = 0x80; // DSR = Data-rate Select Register ("software" reset)

                if self.state.digital_output.controller_enabled()
                    && (data & FLOPPY_DSR_DISK_RESET_MASK) != 0
                {
                    self.reset(true);
                    self.state.sense_output = Some(SenseOutput::ResetCounter { count: 4 });
                    // Always trigger a reset interrupt, even though DMA will be disabled
                    self.raise_interrupt(true);
                    tracing::trace!("DSR wr: Un-resetting - asserting floppy interrupt");
                }
            }
            RegisterOffset::DIGITAL_OUTPUT => {
                let new_digital_output = protocol::DigitalOutputRegister::from(data);
                // state written to DOR contains NOT RESET, and controller enabled is
                // the positive logic relation. Negating controller enabled then gives
                // a high reset signal (which was originally transmitted as active low).
                // This means that a 0x00 byte disabled all motors and initiates a reset.
                // And then 0x04 byte turns off the reset flag. 0x08 will enable
                // interrupts, etc.
                let was_reset = !self.state.digital_output.controller_enabled();
                let is_reset = !new_digital_output.controller_enabled();
                let interrupts_were_enabled = self.state.digital_output.dma_enabled();
                let interrupts_enabled = new_digital_output.dma_enabled();
                self.state.digital_output = new_digital_output;

                if was_reset && !is_reset {
                    tracing::trace!("DOR wr: Un-resetting - asserting floppy interrupt");
                    self.state.sense_output = Some(SenseOutput::ResetCounter { count: 4 });
                    // Always trigger a reset interrupt, regardless of DMA configuration
                    self.raise_interrupt(true);
                } else if is_reset {
                    tracing::debug!("DOR wr: Software reset on fdc");
                    self.reset(true);
                } else {
                    if !interrupts_were_enabled && interrupts_enabled {
                        tracing::trace!("Re-enabling floppy interrupts");
                        self.raise_interrupt(false);
                    } else if interrupts_were_enabled && !interrupts_enabled {
                        tracing::trace!("Disabling floppy interrupts");
                        self.lower_interrupt();
                    }
                }
            }
            RegisterOffset::DATA => self.handle_data_write(data),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
    }
}

#[derive(Clone, Inspect)]
struct FloppyState {
    digital_output: protocol::DigitalOutputRegister,
    main_status: protocol::MainStatusRegister,

    // Used for command input
    #[inspect(bytes)]
    input_bytes: ArrayVec<u8, { protocol::FIFO_SIZE }>,

    // Used for output status/results
    #[inspect(bytes)]
    output_bytes: ArrayVec<u8, { protocol::FIFO_SIZE }>,

    // Needed for async Read/Write/Format
    #[inspect(skip)]
    pending_command: FloppyCommand,

    // scd: [u8; 2],
    head_unload_timer: u8,
    step_rate_time: u8,
    head_load_timer: u8,
    dma_disabled: bool,

    sense_output: Option<SenseOutput>,

    internals: FloppyStateInternals,

    // HACK: Our DSDT always reports that only 1 drive is available.
    // If this changes in the future proper drive selection and indexing will
    // need to be implemented here.
    position: ReadWriteHeadLocation,
    end_of_track: u8,

    // Needed for save/restore
    interrupt_level: bool,
}

#[derive(Clone, Inspect, Debug, Default, Copy)]
struct FloppyStateInternals {
    floppy_changed: bool,
    floppy_present: bool,
    media_write_protected: bool,
    io_pending: bool,

    num_bytes_rd: u32,
    num_bytes_wr: u32,
    sectors_per_track: u8,
    start_sector_pos: u32,
    sector_cache_start_logical: u32,
    sector_cache_end_logical: u32,
}

#[derive(Inspect, Debug, Clone, Copy)]
struct ReadWriteHeadLocation {
    cylinder: u8,
    head: u8,
    sector: u8,
}

impl ReadWriteHeadLocation {
    fn new() -> Self {
        Self {
            cylinder: 0,
            head: 0,
            sector: 0,
        }
    }

    /// Convert from a logical block address lba to a cylinder, head, sector chs indexing scheme
    /// Typical cylinder is 0-79, head is 0-1, sector is 1-18 all inclusive
    /// See https://wiki.osdev.org/Floppy_Disk_Controller#CHS
    fn chs_to_lba(&self, sectors_per_track: u8) -> u32 {
        (self.cylinder as u32 * 2 + self.head as u32) * sectors_per_track as u32
            + (self.sector as u32 - 1)
    }
}

#[derive(Clone, Inspect, Debug)]
#[inspect(external_tag)]
enum SenseOutput {
    /// BIOS expects the controller to interrupt four times, one for
    /// each possible drive connected to controller. Even though
    /// right now per DSDT, we only can have one drive
    ResetCounter { count: u8 },
    /// Effectively denotes interrupt cause, as part of drive state
    Value { value: protocol::StatusRegister0 },
}

impl FloppyState {
    fn new(sectors_per_track: u8, read_only: bool) -> Self {
        Self {
            digital_output: protocol::DigitalOutputRegister::new(),
            main_status: protocol::MainStatusRegister::new(),
            position: ReadWriteHeadLocation::new(),
            end_of_track: 0,

            input_bytes: ArrayVec::new(),
            output_bytes: ArrayVec::new(),

            head_unload_timer: 0,
            step_rate_time: 0,
            head_load_timer: 0,
            dma_disabled: false,
            sense_output: None,
            interrupt_level: false,

            internals: FloppyStateInternals::new(sectors_per_track, read_only),

            pending_command: FloppyCommand::INVALID,
        }
    }
}

impl FloppyStateInternals {
    fn new(sectors_per_track: u8, read_only: bool) -> Self {
        // TODO: this is bogus, but works fine given that we
        // don't support multi disks / hot add/remove
        let floppy_present = sectors_per_track != 0;

        Self {
            floppy_changed: false,
            floppy_present,
            media_write_protected: read_only,
            io_pending: false,

            num_bytes_rd: 0,
            num_bytes_wr: 0,
            sectors_per_track,
            start_sector_pos: 0,
            sector_cache_start_logical: 0,
            sector_cache_end_logical: 0,
        }
    }
}

#[derive(Inspect)]
struct FloppyRt {
    interrupt: LineInterrupt,
    pio_base: Box<dyn ControlPortIoIntercept>,
    pio_control: Box<dyn ControlPortIoIntercept>,
}

/// 82077AA Floppy disk controller
#[derive(InspectMut)]
pub struct FloppyDiskController {
    guest_memory: GuestMemory,

    // Runtime glue
    rt: FloppyRt,

    // Volatile state
    state: FloppyState,

    // backend
    disk_drive: DriveRibbon,

    #[inspect(skip)]
    dma: Box<dyn IsaDmaChannel>,

    #[inspect(skip)]
    command_buffer: CommandBuffer,

    #[inspect(with = "Option::is_some")]
    io: Option<Io>,
    #[inspect(skip)]
    waker: Option<Waker>,
}

/// Floppy disk drive configuration
#[derive(Inspect)]
#[inspect(external_tag)]
pub enum DriveRibbon {
    /// No drives connected
    None,
    /// Single drive connected
    Single(#[inspect(rename = "media")] FloppyMedia),
    // TODO: consider supporting multiple disks per controller?
    // real hardware can support up to 4 per controller...
}

/// Floppy disk backing media
#[derive(Clone)]
pub struct FloppyMedia(Arc<dyn SimpleDisk>);

impl FloppyMedia {
    /// Create a new floppy disk media from a backing `SimpleDisk`
    pub fn new(disk: Arc<dyn SimpleDisk>) -> Self {
        FloppyMedia(disk)
    }
}

impl Inspect for FloppyMedia {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.field("drive_type", "DriveType::Floppy")
            .field("backend_type", self.0.disk_type())
            .field("backend", &self.0)
            .field("sector_size", self.0.sector_size())
            .field("sector_count", self.0.sector_count());
    }
}

/// Error returned by `DriveRibbon::from_vec` when too many drives are provided.
#[derive(Debug, Error)]
#[error("too many drives")]
pub struct TooManyDrives;

impl DriveRibbon {
    /// Create a new `DriveRibbon` from a vector of `FloppyMedia`.
    pub fn from_vec(drives: Vec<FloppyMedia>) -> Result<Self, TooManyDrives> {
        match drives.as_slice() {
            [] => Ok(Self::None),
            [d] => Ok(Self::Single(d.clone())),
            _ => Err(TooManyDrives),
        }
    }
}

/// Errors returned by `FloppyDiskController::new`.
#[derive(Debug, Error)]
pub enum NewFloppyDiskControllerError {
    /// The disk is not a standard size.
    #[error("disk is non-standard size: {0} bytes")]
    NonStandardDisk(u64),
}

impl FloppyDiskController {
    /// Create a new floppy disk controller.
    pub fn new(
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        register_pio: &mut dyn RegisterPortIoIntercept,
        pio_base_addr: u16,
        disk_drive: DriveRibbon,
        dma: Box<dyn IsaDmaChannel>,
    ) -> Result<Self, NewFloppyDiskControllerError> {
        let mut pio_base = register_pio.new_io_region("base", 6);
        let mut pio_control = register_pio.new_io_region("control", 1);

        pio_base.map(pio_base_addr);
        // take note of the 1-byte "hole" in this register space!
        // it is important, as it turns out that IDE controllers claim this port for themselves!
        pio_control.map(pio_base_addr + RegisterOffset::DIGITAL_INPUT.0);

        Ok(Self {
            guest_memory,
            rt: FloppyRt {
                interrupt,
                pio_base,
                pio_control,
            },
            state: FloppyState::new(
                {
                    match &disk_drive {
                        DriveRibbon::None => {
                            // TODO: this is bogus, but works fine given that we
                            // don't support multi disks / hot add/remove
                            0
                        }
                        DriveRibbon::Single(disk) => {
                            let FloppyMedia(disk) = disk;
                            let file_size = disk.sector_count() * disk.sector_size() as u64;

                            let image_type = FloppyImageType::from_file_size(file_size)
                                .ok_or(NewFloppyDiskControllerError::NonStandardDisk(file_size))?;
                            image_type.sectors()
                        }
                    }
                },
                match &disk_drive {
                    DriveRibbon::Single(disk) => disk.0.is_read_only(),
                    DriveRibbon::None => false,
                },
            ),
            disk_drive,
            dma,
            command_buffer: CommandBuffer::new(),
            io: None,
            waker: None,
        })
    }

    /// Sets the asynchronous IO to be polled in `poll_device`.
    fn set_io<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Arc<dyn SimpleDisk>) -> Fut,
        Fut: 'static + Future<Output = Result<(), disk_backend::DiskError>> + Send,
    {
        let DriveRibbon::Single(disk) = &self.disk_drive else {
            panic!();
        };

        let FloppyMedia(disk) = disk;

        let fut = (f)(disk.clone());
        assert!(self.io.is_none());
        self.io = Some(Io(Box::pin(fut)));
        // Ensure poll_device gets called again.
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn handle_io_completion(&mut self, result: Result<(), disk_backend::DiskError>) {
        let command = self.state.pending_command;
        tracing::trace!(?command, ?result, "io completion");

        let result = match command {
            FloppyCommand::READ_NORMAL_DATA
            | FloppyCommand::READ_NORMAL_DEL_DATA
            | FloppyCommand::READ_DEL_DATA
            | FloppyCommand::SEEK_AND_READ
            | FloppyCommand::ALT_SEEK_AND_READ
            | FloppyCommand::READ_TRACK => match result {
                Ok(()) => self.read_complete(),
                Err(err) => Err(err),
            },
            FloppyCommand::WRITE_NORMAL_DATA
            | FloppyCommand::WRITE_DATA
            | FloppyCommand::WRITE_DEL_DATA
            | FloppyCommand::SEEK_AND_WRITE => match result {
                Ok(()) => self.write_complete(),
                Err(err) => Err(err),
            },
            FloppyCommand::FORMAT_TRACK | FloppyCommand::FORMAT_DOUBLE_DENSITY_MODE => match result
            {
                Ok(()) => self.write_zeros_complete(),
                Err(err) => Err(err),
            },
            _ => {
                tracelimit::error_ratelimited!(?command, "unexpected command!");
                return;
            }
        };

        if let Err(err) = result {
            let wo_error = matches!(err, disk_backend::DiskError::ReadOnly);
            self.set_output_status(true, wo_error, true);
        }

        self.state.pending_command = FloppyCommand::INVALID;
        self.complete_command(true);
    }

    // This function is called when we are done reading from a floppy drive image
    // asynchronously or if the data was already in the cache.
    fn read_complete(&mut self) -> Result<(), disk_backend::DiskError> {
        // TODO: we should be checking if the DMA channel is OK before firing
        // off a storage backend request...
        let buffer = match self.dma.request(IsaDmaDirection::Write) {
            Some(r) => r,
            None => {
                tracelimit::error_ratelimited!("request_dma for read failed");
                return Err(disk_backend::DiskError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "request_dma for read failed",
                )));
            }
        };

        let size = (((buffer.size + STANDARD_FLOPPY_SECTOR_SIZE - 1) / STANDARD_FLOPPY_SECTOR_SIZE)
            * STANDARD_FLOPPY_SECTOR_SIZE) as u32;

        let buffer_ptr = &self.command_buffer.buffer[0..size as usize][..size as usize];

        let res = self
            .guest_memory
            .write_from_atomic(buffer.address, buffer_ptr);

        self.dma.complete();

        if let Err(err) = res {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dma transfer failed"
            );
            return Err(disk_backend::DiskError::MemoryAccess(err.into()));
        }

        self.set_output_status(false, false, false);

        Ok(())
    }

    fn write_complete(&mut self) -> Result<(), disk_backend::DiskError> {
        self.set_output_status(false, false, false);
        Ok(())
    }

    fn write_zeros_complete(&mut self) -> Result<(), disk_backend::DiskError> {
        self.set_output_status(false, false, true);
        Ok(())
    }

    /// Return the offset of `addr` from the region's base address.
    ///
    /// Returns `None` if the provided `addr` is outside of the memory
    /// region, or the region is currently unmapped.
    pub fn offset_of(&self, addr: u16) -> Option<u16> {
        self.rt.pio_base.offset_of(addr).or_else(|| {
            self.rt
                .pio_control
                .offset_of(addr)
                .map(|_| RegisterOffset::DIGITAL_INPUT.0)
        })
    }

    fn raise_interrupt(&mut self, is_reset: bool) {
        if self.state.digital_output.dma_enabled() || is_reset {
            self.rt.interrupt.set_level(true);
            self.state.interrupt_level = true;
        }
    }

    fn lower_interrupt(&mut self) {
        self.rt.interrupt.set_level(false);
        self.state.interrupt_level = false;
    }

    // e.g., a reset in the DOR register (meaning bit 2..3 is low)
    // will deassert irq, reset all state info like cur cylinder,
    // but will preserve contents of DOR register itself
    fn reset(&mut self, preserve_digital_output: bool) {
        self.lower_interrupt();
        self.state = FloppyState {
            digital_output: if preserve_digital_output {
                self.state.digital_output
            } else {
                protocol::DigitalOutputRegister::new()
            },
            ..FloppyState::new(
                self.state.internals.sectors_per_track,
                self.state.internals.media_write_protected,
            )
        };

        // At the end of a reset we always do want to set RQM bit in MSR high,
        // so this is fine
        self.state.main_status = protocol::MainStatusRegister::new().with_main_request(true);

        tracing::trace!(
            preserve_digital_output,
            "controller reset - deasserting floppy interrupt"
        );
    }

    fn parse_input_for_readwrite(&mut self) {
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        if input.drive_select() != 0 {
            tracelimit::warn_ratelimited!(
                "Drive selected as outside of what is supported in data read"
            );
        }

        let head = input.head();

        self.state.position.head = head;
        self.state.position.cylinder = self.state.input_bytes[2];
        if self.state.position.cylinder > FLOPPY_TOTAL_CYLINDERS {
            tracelimit::warn_ratelimited!(?self.state.position.cylinder, "Floppy seek to cylinder > 80");
        }
        self.state.position.sector = self.state.input_bytes[4];
        self.state.end_of_track = self.state.input_bytes[6];
        if self.state.input_bytes[5] != 2 || self.state.input_bytes[8] != 0xFF {
            tracelimit::warn_ratelimited!(?self.state.input_bytes, "non-standard floppy read command parameters for PC floppy format");
        }
    }

    fn get_sense_output(&mut self) -> &mut protocol::StatusRegister0 {
        match self.state.sense_output {
            Some(SenseOutput::Value { ref mut value }) => value,
            _ => {
                self.state.sense_output = Some(SenseOutput::Value {
                    value: protocol::StatusRegister0::new(),
                });

                match self.state.sense_output {
                    Some(SenseOutput::Value { ref mut value }) => value,
                    _ => panic!(),
                }
            }
        }
    }

    fn handle_sense_interrupt_status(&mut self) {
        match self.state.sense_output {
            Some(SenseOutput::ResetCounter { ref mut count }) => {
                // If the controller was just reset, it needs to send four
                // consecutive interrupts - one for each possible drive. The
                // bottom two bits of the ST0 (passed back as the first output
                // parameter) should increase from 0 to 3.
                if *count > 0 {
                    let out = protocol::StatusRegister0::from(4 - *count)
                        .with_invalid_command(true)
                        .with_abnormal_termination(true);
                    self.state.sense_output = if (*count - 1) == 0 {
                        None
                    } else {
                        Some(SenseOutput::ResetCounter { count: *count - 1 })
                    };
                    self.state.output_bytes.push(self.state.position.cylinder);
                    self.state.output_bytes.push(out.into());
                } else {
                    tracelimit::error_ratelimited!(
                        "SENSE_INTERRUPT_STATUS called with ResetCount stage 0. p lease fix me"
                    );
                    self.state.output_bytes.push(INVALID_COMMAND_STATUS);
                    self.state.output_bytes.push(INVALID_COMMAND_STATUS);
                }
            }
            Some(SenseOutput::Value { value }) => {
                self.state.output_bytes.push(self.state.position.cylinder);
                self.state.output_bytes.push(value.into());

                self.state.sense_output = None;
            }
            _ => {
                self.state.output_bytes.push(INVALID_COMMAND_STATUS);
                self.state.output_bytes.push(INVALID_COMMAND_STATUS);
            }
        }

        tracing::trace!("sense interrupt status cmd - deasserting floppy interrupt");

        self.lower_interrupt();

        self.state.main_status = (self.state.main_status)
            .with_data_direction(protocol::DataDirection::Write.as_bool())
            .with_non_dma_mode(false)
            .with_busy(false)
            .with_main_request(true);
    }

    fn handle_sense_drive_status(&mut self) {
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        let drive: u8 = input.drive_select();
        if drive != 0 {
            tracelimit::warn_ratelimited!(
                ?drive,
                "Floppy drive number out of range from DSDT enforcement"
            );
        }

        let head: u8 = input.head();
        self.state.position.head = head;

        let output = protocol::StatusRegister3::new()
            .with_drive_select(drive)
            .with_head(head)
            .with_unused1(true)
            .with_track0(self.state.position.cylinder == 0)
            .with_unused2(true)
            .with_write_protected(self.state.internals.media_write_protected);

        self.state.output_bytes.push(output.into());
        self.get_sense_output().set_seek_end(true);
    }

    fn set_output_status(&mut self, rw_error: bool, wo_error: bool, end_seek: bool) {
        if !self.state.output_bytes.is_empty() {
            tracelimit::warn_ratelimited!("output_setup_long called with non-empty output_bytes");
        }
        self.state.output_bytes.push(0x2); // sector size code for standard size of 512 bytes
        self.state.output_bytes.push(1);

        self.state.output_bytes.push(self.state.position.head);
        self.state.output_bytes.push(self.state.position.cylinder);
        self.state.output_bytes.push(
            protocol::StatusRegister2::new()
                .with_missing_address(rw_error)
                .with_bad_cylinder(rw_error)
                .into(),
        );

        self.state.output_bytes.push(
            protocol::StatusRegister1::new()
                .with_no_data(rw_error)
                .with_missing_address(rw_error)
                .with_write_protected(wo_error)
                .into(),
        );

        self.state.output_bytes.push({
            let drive = 0; // again, we only support one drive, but could be changed in future
            let out = protocol::StatusRegister0::new()
                .with_drive_select(drive)
                .with_head(self.state.position.head)
                .with_abnormal_termination(rw_error || wo_error)
                .with_seek_end(end_seek);

            out.into()
        });
    }

    fn complete_command(&mut self, request_interrupt: bool) {
        let has_output = !self.state.output_bytes.is_empty();
        self.state.main_status.set_busy(has_output);
        self.state.main_status.set_non_dma_mode(false);

        let dma_type = if has_output {
            protocol::DataDirection::Read
        } else {
            protocol::DataDirection::Write
        };
        self.state
            .main_status
            .set_data_direction(dma_type.as_bool());
        self.state.main_status.set_main_request(true);

        if request_interrupt {
            self.raise_interrupt(false);
        }
    }

    // Output bytes should be in reverse order of the output in the 82077AA spec.
    // This is because the output bytes are popped off the end of the vector.
    // E.g., 7 byte output for READ_ID is ST0, ST1, ST2, C, H, R, N.
    // So output_bytes[0] is N, output_bytes[6] is ST0.
    fn handle_data_write(&mut self, data: u8) {
        // technically proper byte flow would pend on whether rqm bit for main request was enabled
        if !self.state.digital_output.controller_enabled() {
            // Do not handle commands if we're in a reset state.
            return;
        }

        self.state.input_bytes.push(data);
        let command = FloppyCommand(self.state.input_bytes[0]);

        // we want this to be below update of input buffer so that we
        // don't otherwise misreport what the command byte is
        // side effect is multiple trace lines of one command issue
        tracing::trace!(
            ?data,
            ?self.state.input_bytes,
            "floppy byte (cmd or param)"
        );

        self.handle_command(command);
    }

    fn handle_command(&mut self, command: FloppyCommand) {
        if !self.state.output_bytes.is_empty() {
            tracelimit::warn_ratelimited!(output_bytes = ?self.state.output_bytes, "Floppy data register write with bytes still pending");
        }
        self.state.output_bytes.clear();

        if self.state.input_bytes.len() < command.input_bytes_needed() {
            tracing::debug!(
                ?command,
                bytes_needed = ?command.input_bytes_needed(),
                bytes_received = ?self.state.input_bytes.len(),
                "floppy command missing (or waiting for) parameters"
            );

            // Command is still waiting for more bytes
            self.state.main_status.set_busy(true);
            return;
        }

        tracing::trace!(
            ?command,
            input_bytes = ?self.state.input_bytes,
            "executing floppy command"
        );

        // The controller appears to help along poorly written software
        // which does not correctly clear the INT signal by issuing a
        // sense-interrupt-status command. If we see a command come
        // through which is not a sense-interrupt-status and there
        // is already an interrupt pending, we will deassert the INT signal.
        if self.state.interrupt_level && command != FloppyCommand::SENSE_INTERRUPT_STATUS {
            tracing::trace!(?command, "Floppy interrupt level was high before command execution. Now de-asserting interrupt");
            self.lower_interrupt();
            self.state.main_status.set_active_drives(0);
        }

        let mut complete_command = true;
        let mut request_interrupt = false;

        match command {
            FloppyCommand::READ_NORMAL_DATA
            | FloppyCommand::READ_NORMAL_DEL_DATA
            | FloppyCommand::READ_DEL_DATA
            | FloppyCommand::SEEK_AND_READ
            | FloppyCommand::ALT_SEEK_AND_READ => {
                let success = self.handle_read();
                request_interrupt = !success;
                complete_command = !success;
            }
            FloppyCommand::WRITE_NORMAL_DATA
            | FloppyCommand::WRITE_DATA
            | FloppyCommand::WRITE_DEL_DATA
            | FloppyCommand::SEEK_AND_WRITE => {
                let success = self.handle_write();
                request_interrupt = !success;
                complete_command = !success;
            }
            FloppyCommand::READ_TRACK => {
                // Set the starting cylinder to 0
                self.state.input_bytes[2] = 0;
                let success = self.handle_read();
                request_interrupt = !success;
                complete_command = !success;
            }
            FloppyCommand::VERSION => {
                // magic number returned by 82077AA controllers
                self.state.output_bytes.push(0x90);
            }
            FloppyCommand::FORMAT_TRACK | FloppyCommand::FORMAT_DOUBLE_DENSITY_MODE => {
                let success = self.format();
                request_interrupt = !success;
                complete_command = !success;
            }
            FloppyCommand::SEEK => {
                self.handle_seek();
                request_interrupt = true;
            }
            FloppyCommand::RECALIBRATE => {
                self.handle_recalibrate();
                request_interrupt = true;
            }
            FloppyCommand::SENSE_INTERRUPT_STATUS => {
                self.handle_sense_interrupt_status();
            }
            FloppyCommand::SPECIFY => self.handle_specify(),
            FloppyCommand::SENSE_DRIVE_STATUS => self.handle_sense_drive_status(),
            FloppyCommand::DUMP_REGISTERS => self.handle_dump_registers(),
            FloppyCommand::READ_ID => {
                self.read_id();
                request_interrupt = true;
            }

            // These commands lock out or unlock software resets. Ignore the lock command but respond as if we care.
            // Pass back lock/unlock bit in bit 4.
            FloppyCommand::UNLOCK_FIFO_FUNCTIONS => {
                self.state.output_bytes.push(0);
            }
            FloppyCommand::LOCK_FIFO_FUNCTIONS => {
                self.state.output_bytes.push(0x10);
            }
            FloppyCommand::PART_ID => {
                self.state.output_bytes.push(0x01);
            }
            FloppyCommand::CONFIGURE | FloppyCommand::PERP288_MODE => {
                // Ignore the data bytes. No response, no interrupt.
                tracing::debug!(?command, "command ignored");
            }
            _ => {
                tracelimit::error_ratelimited!(?command, "unimplemented/unsupported command");
                self.state.output_bytes.push(INVALID_COMMAND_STATUS);
            }
        }

        // Finished processing command, so no longer need input
        self.state.input_bytes.clear();

        if !self.state.output_bytes.is_empty() {
            if self.state.output_bytes.len() != command.result_bytes_expected() {
                tracelimit::warn_ratelimited!(?command, output_bytes = ?self.state.output_bytes, "command output size doesn't match expected");
            } else {
                tracing::trace!(
                    ?command,
                    output_bytes = ?self.state.output_bytes,
                    "floppy command output"
                );
            }
        }

        self.state.pending_command = if complete_command {
            self.complete_command(request_interrupt);
            FloppyCommand::INVALID
        } else {
            command
        };

        tracing::trace!(
            main_status = ?self.state.main_status,
            digital_output = ?self.state.digital_output,
            sense_output = ?self.state.sense_output,
            dma_disabled = ?self.state.dma_disabled,
            cylinder = ?self.state.position.cylinder,
            head = ?self.state.position.head,
            sector = ?self.state.position.sector,
            interrupt_level = ?self.state.interrupt_level,
            "floppy state"
        );

        tracing::trace!("floppy command completed");
    }

    fn handle_read(&mut self) -> bool {
        // clear floppy changed flag
        self.state.internals.floppy_changed = false;

        // set interrupt cause
        self.get_sense_output().set_seek_end(true);

        // clear RQM
        self.state.main_status.set_main_request(false);

        // per section 4.2.1 of 82077AA spec, can operate with or
        // without DMA. We want DMA disabled currently because
        // DMA implementation is a stub. self.state.dma_disabled
        // is hard-coded to true for now via FloppyCommand::SPECIFY
        if self.state.dma_disabled {
            tracelimit::warn_ratelimited!("non-dma mode is not supported");
            self.state.main_status.set_non_dma_mode(true);
        }

        // mark drive as busy. this should? always set lsb
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        let busy_drive = input.drive_select();
        self.state
            .main_status
            .set_active_drives(self.state.main_status.active_drives() | (1 << busy_drive));

        self.state.main_status.set_busy(true);

        self.parse_input_for_readwrite();

        let error = !self.read_data();
        if error {
            self.state.internals.io_pending = false;
            self.set_output_status(error, false, error);
        }
        !error
    }

    fn read_data(&mut self) -> bool {
        if !self.state.internals.floppy_present {
            tracelimit::error_ratelimited!("read attempted, but floppy not present");
            return false;
        }

        if self.state.position.sector == 0
            || self.state.position.sector > self.state.end_of_track
            || self.state.position.sector > self.state.internals.sectors_per_track
        {
            tracelimit::error_ratelimited!(
                position = ?self.state.position,
                end_of_track = self.state.end_of_track,
                sectors_per_track = self.state.internals.sectors_per_track,
                "invalid read position"
            );
            return false;
        }

        if self.state.position.cylinder > FLOPPY_TOTAL_CYLINDERS {
            tracelimit::error_ratelimited!(sector = ?self.state.position.sector, "bad sector in floppy read");
            return false;
        }

        self.state.internals.io_pending = true;
        let size_hint = self.dma.check_transfer_size() as usize;

        // now to read the next sector from the floppy
        let lba = (self.state.position).chs_to_lba(self.state.internals.sectors_per_track) as u64;

        let size = {
            let num = (((size_hint + STANDARD_FLOPPY_SECTOR_SIZE - 1)
                / STANDARD_FLOPPY_SECTOR_SIZE)
                * STANDARD_FLOPPY_SECTOR_SIZE) as u32;
            if num < STANDARD_FLOPPY_SECTOR_SIZE as u32 {
                STANDARD_FLOPPY_SECTOR_SIZE as u32
            } else {
                num
            }
        };

        let command_buffer = self.command_buffer.access();

        tracing::trace!(lba, size, "starting disk read");
        self.set_io(|disk| async move {
            let buffers = command_buffer.buffers(0, size as usize, true);
            disk.read_vectored(&buffers, lba).await
        });

        true
    }

    fn handle_write(&mut self) -> bool {
        self.state.internals.floppy_changed = false;

        // set interrupt cause
        self.get_sense_output().set_seek_end(true);

        // clear RQM
        self.state.main_status.set_main_request(false);

        // per section 4.2.1 of 82077AA spec, can operate with or
        // without DMA. We want DMA disabled currently because
        // DMA implementation is a stub. self.state.dma_disabled
        // is hard-coded to true for now via FloppyCommand::SPECIFY
        if self.state.dma_disabled {
            tracelimit::warn_ratelimited!("non-dma mode is not supported");
            self.state.main_status.set_non_dma_mode(true);
        }

        // mark drive as busy. this should? always set lsb
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        let busy_drive = input.drive_select();
        self.state
            .main_status
            .set_active_drives(self.state.main_status.active_drives() | (1 << busy_drive));

        self.state.main_status.set_busy(true);

        self.parse_input_for_readwrite();

        let wo_error = self.state.internals.media_write_protected;
        let error = if wo_error { true } else { !self.write_data() };

        if error {
            self.set_output_status(error, wo_error, error);
        }
        !error
    }

    fn write_data(&mut self) -> bool {
        if !self.state.internals.floppy_present {
            tracelimit::error_ratelimited!("write attempted, but floppy not present");
            return false;
        }

        if self.state.position.sector == 0
            || self.state.position.sector > self.state.end_of_track
            || self.state.position.sector > self.state.internals.sectors_per_track
        {
            tracelimit::error_ratelimited!(
                position = ?self.state.position,
                end_of_track = self.state.end_of_track,
                sectors_per_track = self.state.internals.sectors_per_track,
                "invalid write position"
            );
            return false;
        }

        let lba = (self.state.position).chs_to_lba(self.state.internals.sectors_per_track) as u64;
        let buffer = match self.dma.request(IsaDmaDirection::Read) {
            Some(r) => r,
            None => {
                tracelimit::error_ratelimited!("request_dma for write failed");
                return false;
            }
        };

        let size = ((buffer.size + STANDARD_FLOPPY_SECTOR_SIZE - 1) / STANDARD_FLOPPY_SECTOR_SIZE)
            * STANDARD_FLOPPY_SECTOR_SIZE;

        let command_buffer = self.command_buffer.access();

        let buffer_ptr = &self.command_buffer.buffer[0..size as usize][..size as usize];
        let r = self.guest_memory.read_to_atomic(buffer.address, buffer_ptr);

        self.dma.complete();

        if let Err(err) = r {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dma transfer failed"
            );

            return false;
        }

        let DriveRibbon::Single(disk) = &self.disk_drive else {
            tracelimit::error_ratelimited!("No disk");
            return false;
        };

        let FloppyMedia(disk) = disk;

        if disk.is_read_only() {
            tracelimit::error_ratelimited!("Read only");
            return false;
        }

        self.set_io(|disk| async move {
            let buffers = command_buffer.buffers(0, size as usize, false);
            let result = disk.write_vectored(&buffers, lba, false).await;
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "write failed"
                );
                return Err(err);
            }
            let result = disk.sync_cache().await;
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "flush failed"
                );
                return Err(err);
            }

            result
        });

        true
    }

    fn write_zeros(&mut self) -> bool {
        let DriveRibbon::Single(disk) = &self.disk_drive else {
            tracelimit::error_ratelimited!("No disk");
            return false;
        };

        let FloppyMedia(disk) = disk;

        if disk.is_read_only() {
            tracelimit::error_ratelimited!("Read only");
            return false;
        }

        let buffer = match self.dma.request(IsaDmaDirection::Read) {
            Some(r) => r,
            None => {
                tracelimit::error_ratelimited!("request_dma for format failed");
                return false;
            }
        };

        let size = (((buffer.size + STANDARD_FLOPPY_SECTOR_SIZE - 1) / STANDARD_FLOPPY_SECTOR_SIZE)
            * STANDARD_FLOPPY_SECTOR_SIZE) as u32;

        let command_buffer = self.command_buffer.access();

        let buffer_ptr = &self.command_buffer.buffer[0..size as usize][..size as usize];
        let r = self.guest_memory.read_to_atomic(buffer.address, buffer_ptr);

        self.dma.complete();

        if let Err(err) = r {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "dma transfer failed"
            );

            return false;
        }

        let Some(cylinder) = buffer_ptr.first() else {
            tracelimit::error_ratelimited!("failed to get(0)");
            return false;
        };

        let cylinder = cylinder.load(Ordering::Relaxed) as u64;

        let Some(head) = buffer_ptr.get(1) else {
            tracelimit::error_ratelimited!("failed to get(1)");
            return false;
        };

        let head = head.load(Ordering::Relaxed) as u64;

        let size = STANDARD_FLOPPY_SECTOR_SIZE * self.state.internals.sectors_per_track as usize;
        let buffers = command_buffer.buffers(0, size, false);

        let res = buffers.guest_memory().zero_range(&buffers.range());
        if let Err(err) = res {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "zero_range failed"
            );
            return false;
        }

        let lba = (cylinder * 2 + head) * self.state.internals.sectors_per_track as u64;

        tracing::trace!(?cylinder, ?head, ?lba, ?buffer_ptr, "Format: ");

        self.set_io(|disk| async move {
            let buffers = command_buffer.buffers(0, size, false);
            let result = disk.write_vectored(&buffers, lba, false).await;
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "write failed"
                );
                return Err(err);
            }
            let result = disk.sync_cache().await;
            if let Err(err) = result {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "flush failed"
                );
                return Err(err);
            }
            result
        });

        true
    }

    fn format(&mut self) -> bool {
        let wo_err_occurred = self.state.internals.media_write_protected;
        let error = if wo_err_occurred {
            true
        } else {
            self.state.main_status.set_busy(true);
            !self.write_zeros()
        };

        if error {
            self.set_output_status(error, wo_err_occurred, true);
        }
        !error
    }

    fn handle_seek(&mut self) {
        self.state.internals.floppy_changed = false;

        self.state.position.sector = 0;

        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        self.state.position.head = input.head();

        self.state.position.cylinder = if self.state.input_bytes[2] >= FLOPPY_TOTAL_CYLINDERS {
            tracelimit::warn_ratelimited!(?self.state.position.cylinder, "Floppy seek to cylinder > 80");
            0
        } else {
            self.state.input_bytes[2] // this is the new cylinder number
        };

        self.recalibrate();
    }

    fn handle_recalibrate(&mut self) {
        self.state.position.cylinder = 0;
        self.recalibrate();
    }

    fn recalibrate(&mut self) {
        if let Some(SenseOutput::ResetCounter { .. }) = self.state.sense_output {
            self.state.sense_output = None;
        }

        // We don't have any hardware, e.g., read/write head, that needs
        // to move, so just immediately signal completion. These commands
        // can interrupt a reset sequence, most can't.
        // Also, both arms cause reset stage set to 0 implicitly
        let head = self.state.position.head;
        self.get_sense_output().set_seek_end(true);
        self.get_sense_output().set_head(head);

        // Set the appropriate disk to active
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        let busy_drive = input.drive_select();
        self.state
            .main_status
            .set_active_drives(self.state.main_status.active_drives() | (1 << busy_drive));
        if busy_drive > 0 {
            tracelimit::warn_ratelimited!(
                ?busy_drive,
                "Floppy seek to drive outside of what is supported"
            );
        }
    }

    fn handle_specify(&mut self) {
        let param1 = protocol::SpecifyParam1::from(self.state.input_bytes[1]);
        let param2 = protocol::SpecifyParam2::from(self.state.input_bytes[2]);

        self.state.head_unload_timer = param1.head_unload_timer();
        self.state.step_rate_time = param1.step_rate_time();
        self.state.head_load_timer = param2.head_load_timer();
        self.state.dma_disabled = param2.dma_disabled();
    }

    fn handle_dump_registers(&mut self) {
        self.state.output_bytes.push(self.state.position.cylinder);
        self.state.output_bytes.push(0); // drive 1 cur cylinder (PCN), drive disabled -> 0
        self.state.output_bytes.push(0); // drive 2 PCN. drive disabled, so default 0
        self.state.output_bytes.push(0); // drive 3 PCN. drive disabled, so default 0

        self.state.output_bytes.push(
            protocol::SpecifyParam1::new()
                .with_head_unload_timer(self.state.head_unload_timer)
                .with_step_rate_time(self.state.step_rate_time)
                .into(),
        );
        self.state.output_bytes.push(
            protocol::SpecifyParam2::new()
                .with_head_load_timer(self.state.head_load_timer)
                .with_dma_disabled(self.state.dma_disabled)
                .into(),
        );

        // TODO: Sector per track should not be 0, if disk is inserted / formatted
        self.state.output_bytes.push(0); // SC (Number of Sectors, per track). aka EOT (end of track/number of final sector)
        self.state.output_bytes.push(0); // various flags dealing with PERP288
        self.state.output_bytes.push(0); // configure info (never set?)

        // TODO: write precomp enum and setting
        self.state.output_bytes.push(0); // write precomp start track no. (never set?)
    }

    fn read_id(&mut self) {
        // handle input
        let input = protocol::InputRegister::from(self.state.input_bytes[1]);
        self.state.position.head = input.head();

        // handle output
        self.set_output_status(false, false, false);
        let head = self.state.position.head;
        self.get_sense_output().set_head(head);
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.floppy")]
        pub struct SavedState {
            #[mesh(1)]
            pub digital_output: u8,
            #[mesh(2)]
            pub main_status: u8,
            #[mesh(3)]
            pub input_bytes: Vec<u8>,
            #[mesh(4)]
            pub output_bytes: Vec<u8>,
            #[mesh(5)]
            pub head_unload_timer: u8,
            #[mesh(6)]
            pub step_rate_time: u8,
            #[mesh(7)]
            pub head_load_timer: u8,
            #[mesh(8)]
            pub dma_disabled: bool,
            #[mesh(9)]
            pub interrupt_output: Option<SavedInterruptOutput>,
            #[mesh(10)]
            pub interrupt_level: bool,

            #[mesh(11)]
            pub end_of_track: u8,
            // Below fields are for future-proofing:
            // Unused today as we only support one drive.
            #[mesh(12)]
            pub drive: u8,
            // Only cylinder of the first floppy is used today.
            #[mesh(13)]
            pub floppies: [SavedFloppyState; 1],
            #[mesh(14)]
            pub pending_command: u8,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.floppy")]
        pub struct SavedFloppyState {
            #[mesh(1)]
            pub cylinder: u8,
            #[mesh(2)]
            pub head: u8,
            #[mesh(3)]
            pub sector: u8,
            #[mesh(4)]
            pub internals: SavedFloppyStateInternals,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.floppy")]
        pub enum SavedInterruptOutput {
            #[mesh(1)]
            ResetCounter {
                #[mesh(1)]
                count: u8,
            },
            #[mesh(2)]
            Value {
                #[mesh(1)]
                value: u8,
            },
        }

        impl From<SavedInterruptOutput> for super::SenseOutput {
            fn from(value: SavedInterruptOutput) -> Self {
                match value {
                    SavedInterruptOutput::ResetCounter { count } => {
                        super::SenseOutput::ResetCounter { count }
                    }
                    SavedInterruptOutput::Value { value } => super::SenseOutput::Value {
                        value: super::protocol::StatusRegister0::from(value),
                    },
                }
            }
        }

        impl From<super::SenseOutput> for SavedInterruptOutput {
            fn from(value: super::SenseOutput) -> Self {
                match value {
                    super::SenseOutput::ResetCounter { count } => {
                        SavedInterruptOutput::ResetCounter { count }
                    }
                    super::SenseOutput::Value { value } => SavedInterruptOutput::Value {
                        value: u8::from(value),
                    },
                }
            }
        }

        #[derive(Protobuf, Clone, Copy)]
        #[mesh(package = "chipset.floppy")]
        pub struct SavedFloppyStateInternals {
            #[mesh(1)]
            floppy_changed: bool,
            #[mesh(2)]
            floppy_present: bool,
            #[mesh(3)]
            media_write_protected: bool,
            #[mesh(4)]
            io_pending: bool,

            #[mesh(5)]
            num_bytes_rd: u32,
            #[mesh(6)]
            num_bytes_wr: u32,
            #[mesh(7)]
            sectors_per_track: u8,
            #[mesh(8)]
            start_sector_pos: u32,
            #[mesh(9)]
            sector_cache_start_logical: u32,
            #[mesh(10)]
            sector_cache_end_logical: u32,
        }

        impl From<super::FloppyStateInternals> for SavedFloppyStateInternals {
            fn from(value: super::FloppyStateInternals) -> Self {
                let super::FloppyStateInternals {
                    floppy_changed,
                    floppy_present,
                    media_write_protected,
                    io_pending,
                    num_bytes_rd,
                    num_bytes_wr,
                    sectors_per_track,
                    start_sector_pos,
                    sector_cache_start_logical,
                    sector_cache_end_logical,
                } = value;

                Self {
                    floppy_changed,
                    floppy_present,
                    media_write_protected,
                    io_pending,
                    num_bytes_rd,
                    num_bytes_wr,
                    sectors_per_track,
                    start_sector_pos,
                    sector_cache_start_logical,
                    sector_cache_end_logical,
                }
            }
        }

        impl From<SavedFloppyStateInternals> for super::FloppyStateInternals {
            fn from(value: SavedFloppyStateInternals) -> Self {
                let SavedFloppyStateInternals {
                    floppy_changed,
                    floppy_present,
                    media_write_protected,
                    io_pending,
                    num_bytes_rd,
                    num_bytes_wr,
                    sectors_per_track,
                    start_sector_pos,
                    sector_cache_start_logical,
                    sector_cache_end_logical,
                } = value;

                Self {
                    floppy_changed,
                    floppy_present,
                    media_write_protected,
                    io_pending,
                    num_bytes_rd,
                    num_bytes_wr,
                    sectors_per_track,
                    start_sector_pos,
                    sector_cache_start_logical,
                    sector_cache_end_logical,
                }
            }
        }
    }

    impl SaveRestore for FloppyDiskController {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let FloppyState {
                digital_output,
                main_status,
                ref input_bytes,
                ref output_bytes,
                head_unload_timer,
                step_rate_time,
                head_load_timer,
                dma_disabled,
                sense_output: ref interrupt_output,
                interrupt_level,
                position,
                internals,
                end_of_track,
                pending_command,
            } = self.state;

            let saved_state = state::SavedState {
                digital_output: digital_output.into(),
                main_status: main_status.into(),
                input_bytes: input_bytes.to_vec(),
                output_bytes: output_bytes.to_vec(),
                head_unload_timer,
                step_rate_time,
                head_load_timer,
                dma_disabled,
                interrupt_output: interrupt_output.clone().map(|x| x.into()),
                interrupt_level,
                end_of_track,
                drive: 0,
                floppies: [state::SavedFloppyState {
                    cylinder: position.cylinder,
                    head: position.head,
                    sector: position.sector,
                    internals: internals.into(),
                }],
                pending_command: pending_command.0,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                digital_output,
                main_status,
                input_bytes,
                output_bytes,
                head_unload_timer,
                step_rate_time,
                head_load_timer,
                dma_disabled,
                interrupt_output,
                interrupt_level,
                end_of_track,
                drive: _,
                floppies,
                pending_command,
            } = state;

            self.state = FloppyState {
                digital_output: digital_output.into(),
                main_status: main_status.into(),
                input_bytes: input_bytes.as_slice().try_into().map_err(
                    |e: arrayvec::CapacityError| RestoreError::InvalidSavedState(e.into()),
                )?,
                output_bytes: output_bytes.as_slice().try_into().map_err(
                    |e: arrayvec::CapacityError| RestoreError::InvalidSavedState(e.into()),
                )?,
                head_unload_timer,
                step_rate_time,
                head_load_timer,
                dma_disabled,
                sense_output: interrupt_output.map(|x| x.into()),
                interrupt_level,
                end_of_track,
                position: ReadWriteHeadLocation {
                    cylinder: floppies[0].cylinder,
                    head: floppies[0].head,
                    sector: floppies[0].sector,
                },
                internals: floppies[0].internals.into(),
                pending_command: FloppyCommand(pending_command),
            };

            self.rt.interrupt.set_level(interrupt_level);

            Ok(())
        }
    }
}
