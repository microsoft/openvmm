// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stub Intel 82077AA Floppy Disk Controller, implementing a minimal subset of
//! functionality required to boot using the Microsoft PCAT BIOS.
//!
//! It will unconditionally report that no floppy drives are present.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

use arrayvec::ArrayVec;
use bitfield_struct::bitfield;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

const FIFO_SIZE: usize = 16;
const INVALID_COMMAND_STATUS: u8 = 0x80;
const FLOPPY_DSR_DISK_RESET_MASK: u8 = 0x80;
const ENHANCED_CONTROLLER_VERSION: u8 = 0x90;
const FLOPPY_STATUS0_MASK: u8 = 0xC0;
const FLOPPY_STATUS0_SEEK_END: u8 = 0x20;
const NO_TAPE_DRIVES_PRESENT: u8 = 0xFC;

open_enum! {
    #[derive(Default)]
    enum RegisterOffset: u16 {
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

/// Floppy DOR - digital output register
#[derive(Inspect)]
#[bitfield(u8)]
pub struct DigitalOutput {
    #[bits(2)]
    _drive_select: u8,
    controller_enabled: bool,
    dma_enabled: bool,
    // This is really 4 separate bools, but for our convenience we treat
    // it as a large number.
    #[bits(4)]
    motors_active: u8,
}

/// Floppy main status register
#[derive(Inspect)]
#[bitfield(u8)]
pub struct MainStatus {
    // This is really 4 separate bools, but for our convenience we treat
    // it as a large number.
    #[bits(4)]
    active_drives: u8,
    /// Indicates if the controller is currently executing a command
    busy: bool,
    _non_dma_mode: bool,
    /// Data input/output (1 - output data to CPU, 0 - receive data from CPU).
    /// Holds no meaning if main_request is not set.
    data_direction: bool,
    /// Indicates whether controller is ready to receive or send
    /// data or commands via the data registers
    main_request: bool,
}

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
     enum FloppyCommand: u8 {
        SPECIFY = 0x3,
        SENSE_DRIVE_STATUS = 0x4,
        RECALIBRATE = 0x7,
        SENSE_INTERRUPT_STATUS = 0x8,
        DUMP_REGISTERS = 0xE,
        SEEK = 0xF,
        VERSION = 0x10,
        PERP288_MODE = 0x12,
        CONFIGURE = 0x13,
        UNLOCK_FIFO_FUNCTIONS = 0x14,
        PART_ID = 0x18,
        LOCK_FIFO_FUNCTIONS = 0x94,
    }
}

impl FloppyCommand {
    // Floppy commands are written one byte at a time to the DATA register. The
    // first byte specifies the issued command. The remaining bytes are used as
    // inputs for the command.
    fn input_bytes_needed(&self) -> usize {
        // Add one to account for the command byte itself
        1 + match *self {
            Self::SPECIFY => 2,
            Self::SENSE_DRIVE_STATUS => 1,
            Self::RECALIBRATE => 1,
            Self::SENSE_INTERRUPT_STATUS => 0,
            Self::DUMP_REGISTERS => 0,
            Self::SEEK => 2,
            Self::VERSION => 0,
            Self::PERP288_MODE => 1,
            Self::CONFIGURE => 3,
            Self::UNLOCK_FIFO_FUNCTIONS => 0,
            Self::PART_ID => 0,
            Self::LOCK_FIFO_FUNCTIONS => 0,
            _ => 0,
        }
    }
}

impl ChangeDeviceState for StubFloppyDiskController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.reset(false);
    }
}

impl ChipsetDevice for StubFloppyDiskController {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

// Must implement this trait so this can "slot-in" where the real floppy
// controller would be
impl PollDevice for StubFloppyDiskController {
    fn poll_device(&mut self, _cx: &mut std::task::Context<'_>) {}
}

impl PortIoIntercept for StubFloppyDiskController {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let mut io_result = IoResult::Ok;
        let offset = RegisterOffset(io_port % 0x10);

        data[0] = match offset {
            // This port is completely unsupported by latest floppy controllers.
            RegisterOffset::STATUS_A => 0xFF,
            // Also unsupported but return 0xFC to indicate no tape drives present.
            RegisterOffset::STATUS_B => NO_TAPE_DRIVES_PRESENT,
            // Do nothing. This port is obsolete.
            RegisterOffset::TAPE_DRIVE => 0xFF,
            // Now the ports that actually do something.
            RegisterOffset::DIGITAL_OUTPUT => self.state.digital_output.0,
            RegisterOffset::MAIN_STATUS => {
                // Indicate data register is ready for reading/writing.
                if self.state.digital_output.controller_enabled() {
                    self.state.main_status.0
                } else {
                    0
                }
            }
            RegisterOffset::DATA => {
                // If there are more bytes left to read then read them out now.
                if let Some(result) = self.state.output_bytes.pop() {
                    self.state.main_status.set_active_drives(0);
                    if self.state.output_bytes.is_empty() {
                        // Reverse direction, now ready to receive a new command
                        self.state.main_status.set_data_direction(false);
                        self.state.main_status.set_busy(false);
                    }
                    result
                } else {
                    INVALID_COMMAND_STATUS
                }
            }
            RegisterOffset::DIGITAL_INPUT => {
                // The bottom seven bits are tristated, and always read as
                // ones on a real floppy controller.
                if self.state.digital_output.motors_active() != 0 {
                    0xff
                } else {
                    0x7f
                }
            }
            _ => {
                io_result = IoResult::Err(IoError::InvalidRegister);
                0
            }
        };

        tracing::trace!(?io_port, ?offset, ?data, "io port read");
        io_result
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let data = data[0];
        let offset = RegisterOffset(io_port % 0x10);
        tracing::trace!(?io_port, ?offset, ?data, "io port write");

        match offset {
            RegisterOffset::STATUS_A | RegisterOffset::STATUS_B => {
                tracelimit::warn_ratelimited!(?offset, "write to read-only floppy status register");
            }
            RegisterOffset::TAPE_DRIVE => {} // Do nothing. This port is obsolete.
            RegisterOffset::CONFIG_CONTROL => {} // ignore writes
            RegisterOffset::DATA_RATE => {
                if self.state.digital_output.controller_enabled()
                    && (data & FLOPPY_DSR_DISK_RESET_MASK) != 0
                {
                    self.reset(true);
                    self.state.sense_output = Some(SenseOutput::ResetCounter { count: 4 });
                    // Always trigger a reset interrupt, even though DMA will be disabled
                    self.raise_interrupt(true);
                    tracing::trace!("Un-resetting - asserting floppy interrupt");
                }
            }
            RegisterOffset::DIGITAL_OUTPUT => {
                let new_digital_output = DigitalOutput::from(data);
                let was_reset = !self.state.digital_output.controller_enabled();
                let is_reset = !new_digital_output.controller_enabled();
                let interrupts_were_enabled = self.state.digital_output.dma_enabled();
                let interrupts_enabled = new_digital_output.dma_enabled();
                self.state.digital_output = new_digital_output;

                if was_reset && !is_reset {
                    tracing::trace!("un-resetting - asserting floppy interrupt");
                    self.state.sense_output = Some(SenseOutput::ResetCounter { count: 4 });
                    // Always trigger a reset interrupt, regardless of DMA configuration
                    self.raise_interrupt(true);
                } else if is_reset {
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
            RegisterOffset::DATA => {
                if !self.state.digital_output.controller_enabled() {
                    // Do not handle commands if we're in a reset state.
                    return IoResult::Ok;
                }

                tracing::trace!(
                    ?data,
                    ?self.state.input_bytes,
                    "floppy command byte"
                );

                self.state.output_bytes.clear();
                self.state.input_bytes.push(data);
                self.state.main_status.set_busy(true);
                let command = FloppyCommand(self.state.input_bytes[0]);
                if self.state.input_bytes.len() < command.input_bytes_needed() {
                    return IoResult::Ok;
                }

                tracing::trace!(
                    ?command,
                    input_bytes = ?self.state.input_bytes,
                    "executing floppy command"
                );

                match command {
                    FloppyCommand::SPECIFY => {
                        // Head timing information is returned as part of the
                        // DUMP REGISTERS command. This command also specifies
                        // whether DMA is enabled but this is ignored for now.
                        self.state.scd = [self.state.input_bytes[1], self.state.input_bytes[2]];
                    }
                    FloppyCommand::SENSE_DRIVE_STATUS => {
                        // The lowest bit specifies the drive number, the next
                        // is the track, the last is the head.
                        // These get reported back in the output.
                        let input_info = self.state.input_bytes[1] & 0b111;
                        let mut result = 0x28 | input_info;
                        if self.state.cur_cylinder == 0 {
                            result |= 0x10;
                        }
                        self.state.output_bytes.push(result);

                        if let Some(SenseOutput::Value { ref mut value }) = self.state.sense_output
                        {
                            *value |= FLOPPY_STATUS0_SEEK_END;
                        }
                    }
                    FloppyCommand::RECALIBRATE | FloppyCommand::SEEK => {
                        self.state.cur_cylinder = if matches!(command, FloppyCommand::SEEK) {
                            self.state.input_bytes[2]
                        } else {
                            0
                        };
                        // We don't have any hardware that needs to move, so just
                        // immediately signal completion. These commands can interrupt
                        // a reset sequence, most can't.
                        match self.state.sense_output {
                            Some(SenseOutput::Value { ref mut value }) => {
                                *value |= FLOPPY_STATUS0_SEEK_END
                            }
                            _ => {
                                self.state.sense_output = Some(SenseOutput::Value {
                                    value: FLOPPY_STATUS0_SEEK_END,
                                })
                            }
                        }
                        // Set the appropriate disk to active
                        self.state.main_status.set_active_drives(
                            self.state.main_status.active_drives()
                                | (1 << (self.state.input_bytes[1] & 0x3)),
                        );

                        self.raise_interrupt(false);
                    }
                    FloppyCommand::SENSE_INTERRUPT_STATUS => {
                        self.state.output_bytes.push(self.state.cur_cylinder);
                        match self.state.sense_output {
                            Some(SenseOutput::ResetCounter { ref mut count }) => {
                                self.state
                                    .output_bytes
                                    .push(FLOPPY_STATUS0_MASK | (4 - *count));
                                *count -= 1;
                                if *count == 0 {
                                    self.state.sense_output = None;
                                }
                            }
                            Some(SenseOutput::Value { value }) => {
                                self.state.output_bytes.push(value);
                                self.state.sense_output = None;
                            }
                            None => {
                                self.state.output_bytes.push(INVALID_COMMAND_STATUS);
                            }
                        }

                        tracing::trace!(
                            "sense interrupt status cmd - deasserting floppy interrupt"
                        );
                        self.lower_interrupt();
                    }
                    FloppyCommand::DUMP_REGISTERS => {
                        self.state.output_bytes.push(self.state.cur_cylinder);
                        self.state.output_bytes.push(0); // drive 1 cur cylinder, drive disabled -> 0
                        self.state.output_bytes.push(0); // unknown hardcoded 0, maybe drive 2?
                        self.state.output_bytes.push(0); // unknown hardcoded 0, maybe drive 3?
                        self.state.output_bytes.push(self.state.scd[0]);
                        self.state.output_bytes.push(self.state.scd[1]);
                        self.state.output_bytes.push(0); // cur floppy sectors per track, no media -> 0
                        self.state.output_bytes.push(0); // unknown hardcoded 0, perpendicular info?
                        self.state.output_bytes.push(0); // configure info (never set?)
                        self.state.output_bytes.push(0); // write precomp (never set?)
                    }
                    FloppyCommand::VERSION => {
                        self.state.output_bytes.push(ENHANCED_CONTROLLER_VERSION);
                    }
                    FloppyCommand::PERP288_MODE => {} // Ignore the data byte. No response, no interrupt.
                    FloppyCommand::CONFIGURE => {} // Ignore the data bytes. No response, no interrupt.
                    FloppyCommand::PART_ID => {
                        self.state.output_bytes.push(0x01);
                    }
                    // These commands lock out or unlock software resets. Ignore the lock command but respond as if we care.
                    // Pass back lock/unlock bit in bit 4.
                    FloppyCommand::UNLOCK_FIFO_FUNCTIONS => {
                        self.state.output_bytes.push(0);
                    }
                    FloppyCommand::LOCK_FIFO_FUNCTIONS => {
                        self.state.output_bytes.push(0x10);
                    }
                    _ => {
                        tracing::debug!(?command, "unimplemented/unsupported command");
                        self.state.output_bytes.push(INVALID_COMMAND_STATUS);
                    }
                }

                self.state.input_bytes.clear();

                if self.state.output_bytes.is_empty() {
                    self.state.main_status.set_busy(false);
                } else {
                    // Sets IO direction to Controller -> Host
                    self.state.main_status.set_data_direction(true);
                }

                // Possibly add PCAT BIOS wait cancellation enlightenment to indicate
                // emulated device activity.
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
    }
}

#[derive(Clone, Inspect)]
struct FloppyState {
    digital_output: DigitalOutput,
    main_status: MainStatus,

    // Used for command input
    #[inspect(bytes)]
    input_bytes: ArrayVec<u8, FIFO_SIZE>,

    // Used for output status/results
    #[inspect(bytes)]
    output_bytes: ArrayVec<u8, FIFO_SIZE>,

    scd: [u8; 2],

    sense_output: Option<SenseOutput>,

    // HACK: Our DSDT always reports that only 1 drive is available.
    // If this changes in the future proper drive selection and indexing will
    // need to be implemented here.
    cur_cylinder: u8,

    // Needed for save/restore
    interrupt_level: bool,
}

#[derive(Clone, Inspect)]
#[inspect(external_tag)]
enum SenseOutput {
    ResetCounter { count: u8 },
    Value { value: u8 },
}

impl FloppyState {
    fn new() -> Self {
        Self {
            digital_output: DigitalOutput::new(),
            main_status: MainStatus::new(),
            cur_cylinder: 0,
            input_bytes: ArrayVec::new(),
            output_bytes: ArrayVec::new(),
            scd: [0; 2],
            sense_output: None,
            interrupt_level: false,
        }
    }
}

#[derive(Inspect)]
struct FloppyRt {
    interrupt: LineInterrupt,
    pio_base: Box<dyn ControlPortIoIntercept>,
    pio_control: Box<dyn ControlPortIoIntercept>,
}

/// Stub implementation of the Intel 82077AA Floppy Disk Controller.
#[derive(InspectMut)]
pub struct StubFloppyDiskController {
    // Runtime glue
    rt: FloppyRt,

    // Volatile state
    state: FloppyState,
}

impl StubFloppyDiskController {
    /// Create a new `StubFloppyDiskController` instance.
    pub fn new(
        interrupt: LineInterrupt,
        register_pio: &mut dyn RegisterPortIoIntercept,
        pio_base_addr: u16,
    ) -> Self {
        let mut pio_base = register_pio.new_io_region("floppy base", 6);
        let mut pio_control = register_pio.new_io_region("floppy control", 1);

        pio_base.map(pio_base_addr);
        // take note of the 1-byte "hole" in this register space!
        // it is important, as it turns out that IDE controllers like to claim this port for themselves!
        pio_control.map(pio_base_addr + RegisterOffset::DIGITAL_INPUT.0);

        Self {
            rt: FloppyRt {
                interrupt,
                pio_base,
                pio_control,
            },
            state: FloppyState::new(),
        }
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

    fn reset(&mut self, preserve_digital_output: bool) {
        self.lower_interrupt();
        self.state = FloppyState {
            digital_output: if preserve_digital_output {
                self.state.digital_output
            } else {
                DigitalOutput::new()
            },
            ..FloppyState::new()
        };

        // Main request will always be true for us as we don't support actually
        // returning any data or delaying interrupts today. If these conditions
        // change then more careful handling of main request may be necessary.
        self.state.main_status.set_main_request(true);

        tracing::trace!(
            preserve_digital_output,
            "controller reset - deasserting floppy interrupt"
        );
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
            pub scd: [u8; 2],
            #[mesh(6)]
            pub interrupt_output: Option<SavedInterruptOutput>,
            #[mesh(7)]
            pub interrupt_level: bool,
            // Below fields are for future-proofing:
            // Unused today as we only support one drive.
            #[mesh(8)]
            pub cur_drive: u8,
            // Only cur_cylinder of the first floppy is used today.
            #[mesh(9)]
            pub floppies: [SavedFloppyState; 4],
        }

        #[derive(Protobuf, Default)]
        #[mesh(package = "chipset.floppy")]
        pub struct SavedFloppyState {
            #[mesh(1)]
            pub cur_cylinder: u8,
            #[mesh(2)]
            pub cur_head: u8,
            #[mesh(3)]
            pub cur_sector: u8,
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
                    SavedInterruptOutput::Value { value } => super::SenseOutput::Value { value },
                }
            }
        }

        impl From<super::SenseOutput> for SavedInterruptOutput {
            fn from(value: super::SenseOutput) -> Self {
                match value {
                    super::SenseOutput::ResetCounter { count } => {
                        SavedInterruptOutput::ResetCounter { count }
                    }
                    super::SenseOutput::Value { value } => SavedInterruptOutput::Value { value },
                }
            }
        }
    }

    impl SaveRestore for StubFloppyDiskController {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let FloppyState {
                digital_output,
                main_status,
                ref input_bytes,
                ref output_bytes,
                scd,
                sense_output: ref interrupt_output,
                interrupt_level,
                cur_cylinder,
            } = self.state;

            let saved_state = state::SavedState {
                digital_output: digital_output.into(),
                main_status: main_status.into(),
                input_bytes: input_bytes.to_vec(),
                output_bytes: output_bytes.to_vec(),
                scd,
                interrupt_output: interrupt_output.clone().map(|x| x.into()),
                interrupt_level,
                cur_drive: 0,
                floppies: [
                    state::SavedFloppyState {
                        cur_cylinder,
                        ..state::SavedFloppyState::default()
                    },
                    state::SavedFloppyState::default(),
                    state::SavedFloppyState::default(),
                    state::SavedFloppyState::default(),
                ],
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                digital_output,
                main_status,
                input_bytes,
                output_bytes,
                scd,
                interrupt_output,
                interrupt_level,
                cur_drive: _,
                floppies,
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
                scd,
                sense_output: interrupt_output.map(|x| x.into()),
                interrupt_level,
                cur_cylinder: floppies[0].cur_cylinder,
            };

            self.rt.interrupt.set_level(interrupt_level);

            Ok(())
        }
    }
}
