// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel 8042 controller for PS/2 keyboard and mouse input.

#![warn(missing_docs)]

mod ps2keyboard;
mod ps2mouse;
pub mod resolver;
mod spec;

use self::ps2keyboard::Ps2Keyboard;
use self::ps2mouse::Ps2Mouse;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use input_core::InputSource;
use input_core::KeyboardData;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use spec::CommandFlag;
use spec::ControllerCommand;
use spec::KeyboardStatus;
use spec::OutputPort;
use std::task::Context;
use std::task::Waker;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

/// An Intel 8042 keyboard/mouse controller.
#[derive(InspectMut)]
pub struct I8042Device {
    // Runtime glue
    #[inspect(skip)]
    trigger_reset: Box<dyn Fn() + Send + Sync>,
    keyboard_interrupt: LineInterrupt,
    mouse_interrupt: LineInterrupt,

    // Sub-emulators
    keyboard: Ps2Keyboard,
    mouse: Ps2Mouse,

    // Runtime book-keeping
    #[inspect(skip)]
    waker: Option<Waker>,

    // Volatile state
    state: I8042State,
}

#[derive(Inspect, Clone)]
struct I8042State {
    command_flag: CommandFlag,
    #[inspect(flatten)]
    data_port_target: DataPortTarget,
    output_buffer: u8,
    output_buffer_state: OutputBufferState,
    a20_gate: bool,
    memory: [u8; 32],
}

#[derive(Inspect, Copy, Clone, PartialEq, Eq)]
enum OutputBufferState {
    Empty,
    Controller,
    Keyboard,
    Mouse,
}

#[derive(Copy, Clone, Inspect)]
#[inspect(tag = "data_port_target")]
enum DataPortTarget {
    Keyboard,
    Mouse,
    Controller(#[inspect(rename = "target_command")] ControllerCommand),
}

impl I8042State {
    fn new() -> Self {
        Self {
            command_flag: CommandFlag::new()
                .with_allow_keyboard_interrupts(true)
                .with_allow_mouse_interrupts(true)
                .with_keyboard_self_test(true)
                .with_enable_scan_code(true),
            memory: [0; 32],
            data_port_target: DataPortTarget::Keyboard,
            output_buffer: 0,
            output_buffer_state: OutputBufferState::Empty,
            a20_gate: true,
        }
    }
}

open_enum! {
     enum ControllerPort: u16 {
        DATA = 0x60,
        COMMAND = 0x64,
    }
}

impl I8042Device {
    /// Returns a new controller with an attached PS/2 keyboard.
    ///
    /// Calls `reset` to reset the VM on guest request.
    pub async fn new(
        reset: Box<dyn Fn() + Send + Sync>,
        keyboard_interrupt: LineInterrupt,
        mouse_interrupt: LineInterrupt,
        mut keyboard_input: Box<dyn InputSource<KeyboardData>>,
    ) -> Self {
        // Activate the input immediately.
        keyboard_input.set_active(true).await;
        I8042Device {
            trigger_reset: reset,
            keyboard_interrupt,
            mouse_interrupt,
            state: I8042State::new(),
            keyboard: Ps2Keyboard::new(keyboard_input),
            mouse: Ps2Mouse::new(),
            waker: None,
        }
    }
}

impl ChangeDeviceState for I8042Device {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            trigger_reset: _,
            keyboard_interrupt: _,
            mouse_interrupt: _,
            keyboard,
            mouse,
            waker: _,
            state,
        } = self;

        *state = I8042State::new();
        keyboard.reset();
        mouse.reset();

        self.sync_interrupts();
    }
}

impl ChipsetDevice for I8042Device {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for I8042Device {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.keyboard.poll(cx);
        self.load_device_output();
        self.waker = Some(cx.waker().clone());
    }
}

impl PortIoIntercept for I8042Device {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        data[0] = match ControllerPort(io_port) {
            ControllerPort::DATA => {
                // Read a piece of data from the output buffer
                self.read_output_byte()
            }
            ControllerPort::COMMAND => {
                // Read the current keyboard status.
                let data = KeyboardStatus::new()
                    .with_output_buffer_full(
                        self.state.output_buffer_state != OutputBufferState::Empty,
                    )
                    .with_input_buffer_full(false)
                    .with_keyboard_self_test(true)
                    .with_input_buffer_for_controller(matches!(
                        self.state.data_port_target,
                        DataPortTarget::Controller(_)
                    ))
                    .with_keyboard_unlocked(true)
                    .with_output_buffer_for_mouse(
                        self.state.output_buffer_state == OutputBufferState::Mouse,
                    );

                data.into()
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        // Populate the next output byte if appropriate.
        self.check_devices_for_output();
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        let &[data] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        match ControllerPort(io_port) {
            ControllerPort::DATA => {
                match std::mem::replace(&mut self.state.data_port_target, DataPortTarget::Keyboard)
                {
                    DataPortTarget::Keyboard => {
                        self.state.command_flag.set_disable_keyboard(false);
                        self.keyboard.input(data);
                    }
                    DataPortTarget::Mouse => {
                        self.state.command_flag.set_disable_mouse(false);
                        self.mouse.input(data);
                    }
                    DataPortTarget::Controller(command) => {
                        self.handle_command(command, Some(data));
                    }
                }

                // Check to see if the keyboard or mouse
                // have more input for the controller.
                self.check_devices_for_output();
            }
            ControllerPort::COMMAND => {
                // Next data port write defaults to the keyboard.
                self.state.data_port_target = DataPortTarget::Keyboard;
                let command = ControllerCommand(data);
                if self.handle_command(command, None).is_none() {
                    self.state.data_port_target = DataPortTarget::Controller(command);
                }

                // Check to see if the keyboard or mouse
                // have more input for the controller.
                self.check_devices_for_output();
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, std::ops::RangeInclusive<u16>)] {
        &[
            ("data", ControllerPort::DATA.0..=ControllerPort::DATA.0),
            (
                "command",
                ControllerPort::COMMAND.0..=ControllerPort::COMMAND.0,
            ),
        ]
    }
}

impl I8042Device {
    fn sync_interrupts(&mut self) {
        self.keyboard_interrupt.set_level(
            self.state.command_flag.allow_keyboard_interrupts()
                && self.state.output_buffer_state == OutputBufferState::Keyboard,
        );
        self.mouse_interrupt.set_level(
            self.state.command_flag.allow_mouse_interrupts()
                && self.state.output_buffer_state == OutputBufferState::Mouse,
        );
    }

    fn write_output_byte(&mut self, state: OutputBufferState, data: u8) {
        self.state.output_buffer = data;
        self.state.output_buffer_state = state;
        self.sync_interrupts();
    }

    fn read_output_byte(&mut self) -> u8 {
        self.state.output_buffer_state = OutputBufferState::Empty;
        self.sync_interrupts();
        self.state.output_buffer
    }

    /// Returns `None` if the command needs to wait for more data.
    fn handle_command(
        &mut self,
        command: ControllerCommand,
        command_data: Option<u8>,
    ) -> Option<()> {
        tracing::debug!(?command, command_data, "8042 command");
        match command {
            ControllerCommand::READ_COMMAND_BYTE => {
                self.write_output_byte(
                    OutputBufferState::Controller,
                    self.state.command_flag.into(),
                );
            }
            ControllerCommand::WRITE_COMMAND_BYTE => {
                self.state.command_flag = command_data?.into();
            }
            ControllerCommand::DISABLE_AUX_INTERFACE => {
                self.state.command_flag.set_disable_mouse(true);
            }
            ControllerCommand::ENABLE_AUX_INTERFACE => {
                self.state.command_flag.set_disable_mouse(false);
            }
            ControllerCommand::CHECK_AUX_INTERFACE => {
                self.write_output_byte(OutputBufferState::Controller, 0);
            }
            ControllerCommand::SELF_TEST => {
                self.write_output_byte(OutputBufferState::Controller, 0x55);
            }
            ControllerCommand::CHECK_INTERFACE => {
                self.write_output_byte(OutputBufferState::Controller, 0);
            }
            ControllerCommand::DISABLE_KEYBOARD => {
                self.state.command_flag.set_disable_keyboard(true);
            }
            ControllerCommand::ENABLE_KEYBOARD => {
                self.state.command_flag.set_disable_keyboard(false);
            }
            ControllerCommand::READ_INPUT_PORT => {
                // Specify that the keyboard is not locked
                self.write_output_byte(OutputBufferState::Controller, 0x80);
            }
            ControllerCommand::READ_OUT_INPUT_PORT_LO
            | ControllerCommand::READ_OUT_INPUT_PORT_HI => {
                //
                // Over multiple releases of Hyper-V these have never
                // been implemented and are clearly not needed.
                // Silently ignore these two commands.
                //
            }
            ControllerCommand::READ_OUTPUT_PORT => {
                let output_port = OutputPort::new()
                    .with_reset(true)
                    .with_a20_gate(self.state.a20_gate)
                    .with_aux_clock(true)
                    .with_aux_data(false)
                    .with_keyboard_output_buffered(
                        self.state.output_buffer_state == OutputBufferState::Keyboard,
                    )
                    .with_mouse_output_buffered(
                        self.state.output_buffer_state == OutputBufferState::Mouse,
                    )
                    .with_clock(true)
                    .with_data(false);

                self.write_output_byte(OutputBufferState::Controller, output_port.into());
            }
            ControllerCommand::WRITE_OUTPUT_PORT => {
                let output_port = OutputPort::from(command_data?);
                if output_port.a20_gate() != self.state.a20_gate {
                    tracelimit::warn_ratelimited!(
                        a20_gate = output_port.a20_gate(),
                        "a20 gate changed, not supported"
                    );
                    self.state.a20_gate = output_port.a20_gate();
                }
                if !output_port.reset() {
                    tracing::info!("initiated reset via WRITE_OUTPUT_PORT command");
                    (self.trigger_reset)();
                }
            }
            ControllerCommand::WRITE_OUTPUT_BUFFER => {
                // Write the data to the output buffer, making it look as
                // though the keyboard put it there. This means that there
                // will be an interrupt (IRQ1) requested.
                self.write_output_byte(OutputBufferState::Keyboard, command_data?);
            }
            ControllerCommand::WRITE_AUX_OUTPUT_BUFFER => {
                // Write the data to the output buffer, making it look as
                // though the mouse put it there.
                self.write_output_byte(OutputBufferState::Mouse, command_data?);
            }
            ControllerCommand::WRITE_AUX_DEVICE => {
                // The next byte written to port 60 will go to the mouse instead of the keyboard
                self.state.data_port_target = DataPortTarget::Mouse;
            }
            cmd if (ControllerCommand::PULSE_OUTPUT_F0..=ControllerCommand::PULSE_OUTPUT_FF)
                .contains(&cmd) =>
            {
                if (cmd.0 & 1) == 0 {
                    // If we get this command, the program wants to restart the
                    // machine if bit 0 of the command is clear.
                    tracing::info!("initiated reset via PULSE_OUTPUT_FX command");
                    (self.trigger_reset)();
                } else {
                    // This command (along with commands 0xF0 through 0xFE) strobes the
                    // four output bits on the keyboard controller. Except for 0xFE, all
                    // of these commands are NOPs. This one in particular is used by the
                    // BIOS and various other applications just to check to make sure the
                    // keyboard controller is still responding.
                }
            }
            cmd if (0x20..=0x3f).contains(&cmd.0) => {
                // There are 31 spare bytes of data storage in the keyboard controller
                // that can be addressed and written to by commands 0x61 - 0x7F.
                // When these registers are read using commands 0x21 - 0x3f, they
                // apparently simulate data bytes coming from the keyboard.
                // NetOp (a remote control program) uses this functionality.

                self.write_output_byte(
                    OutputBufferState::Keyboard,
                    self.state.memory[cmd.0 as usize & 0x1f],
                );
            }
            command if (0x60..=0x7f).contains(&command.0) => {
                self.state.memory[command.0 as usize & 0x1f] = command_data?;
            }

            ControllerCommand::UNKNOWN_A1 => {
                self.write_output_byte(OutputBufferState::Controller, 0);
            }
            ControllerCommand::PWD_CHECK => {
                self.write_output_byte(OutputBufferState::Controller, 0xf1);
            }
            cmd => {
                tracelimit::warn_ratelimited!(?cmd, "unsupported keyboard command");
            }
        }
        Some(())
    }

    /// Loads the output buffer with the next device output byte.
    fn load_device_output(&mut self) -> bool {
        if self.state.output_buffer_state != OutputBufferState::Empty {
            return true;
        }

        if !self.state.command_flag.disable_mouse() {
            if let Some(byte) = self.mouse.output() {
                self.write_output_byte(OutputBufferState::Mouse, byte);
                return true;
            }
        }

        if let Some(byte) = self.keyboard.output() {
            self.write_output_byte(OutputBufferState::Keyboard, byte);
            return true;
        }

        false
    }

    /// Loads the output buffer with the next device output byte, waking the
    /// controller and devices to be polled if there are no more output bytes.
    fn check_devices_for_output(&mut self) {
        if !self.load_device_output() {
            // Wake the poll function to poll the devices.
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        /// Saved state.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.i8042")]
        pub struct SavedState {
            #[mesh(1)]
            pub controller: SavedControllerState,
            #[mesh(2)]
            pub keyboard: <super::ps2keyboard::Ps2Keyboard as SaveRestore>::SavedState,
            #[mesh(3)]
            pub mouse: <super::ps2mouse::Ps2Mouse as SaveRestore>::SavedState,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.i8042")]
        pub struct SavedControllerState {
            #[mesh(1)]
            pub command_flag: u8,
            #[mesh(2)]
            pub data_port_target: SavedDataPortTarget,
            #[mesh(3)]
            pub output_buffer: u8,
            #[mesh(4)]
            pub output_buffer_state: SavedOutputBufferState,
            #[mesh(5)]
            pub a20_gate: bool,
            #[mesh(6)]
            pub memory: [u8; 32],
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.i8042")]
        pub enum SavedOutputBufferState {
            #[mesh(1)]
            Empty,
            #[mesh(2)]
            Controller,
            #[mesh(3)]
            Keyboard,
            #[mesh(4)]
            Mouse,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.i8042")]
        pub enum SavedDataPortTarget {
            #[mesh(1)]
            Keyboard,
            #[mesh(2)]
            Mouse,
            #[mesh(3)]
            Controller(u8),
        }
    }

    impl SaveRestore for I8042Device {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let I8042State {
                command_flag,
                data_port_target,
                output_buffer,
                output_buffer_state,
                a20_gate,
                memory,
            } = self.state;

            let saved_state = state::SavedState {
                controller: state::SavedControllerState {
                    command_flag: command_flag.into(),
                    data_port_target: match data_port_target {
                        DataPortTarget::Keyboard => state::SavedDataPortTarget::Keyboard,
                        DataPortTarget::Mouse => state::SavedDataPortTarget::Mouse,
                        DataPortTarget::Controller(ControllerCommand(b)) => {
                            state::SavedDataPortTarget::Controller(b)
                        }
                    },
                    output_buffer,
                    output_buffer_state: match output_buffer_state {
                        OutputBufferState::Empty => state::SavedOutputBufferState::Empty,
                        OutputBufferState::Controller => state::SavedOutputBufferState::Controller,
                        OutputBufferState::Keyboard => state::SavedOutputBufferState::Keyboard,
                        OutputBufferState::Mouse => state::SavedOutputBufferState::Mouse,
                    },
                    a20_gate,
                    memory,
                },
                keyboard: self.keyboard.save()?,
                mouse: self.mouse.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                controller,
                keyboard,
                mouse,
            } = state;

            {
                let state::SavedControllerState {
                    command_flag,
                    data_port_target,
                    output_buffer,
                    output_buffer_state,
                    a20_gate,
                    memory,
                } = controller;

                self.state = I8042State {
                    command_flag: CommandFlag::from(command_flag), // no unused bits
                    data_port_target: match data_port_target {
                        state::SavedDataPortTarget::Keyboard => DataPortTarget::Keyboard,
                        state::SavedDataPortTarget::Mouse => DataPortTarget::Mouse,
                        state::SavedDataPortTarget::Controller(b) => {
                            DataPortTarget::Controller(ControllerCommand(b))
                        }
                    },
                    output_buffer,
                    output_buffer_state: match output_buffer_state {
                        state::SavedOutputBufferState::Empty => OutputBufferState::Empty,
                        state::SavedOutputBufferState::Controller => OutputBufferState::Controller,
                        state::SavedOutputBufferState::Keyboard => OutputBufferState::Keyboard,
                        state::SavedOutputBufferState::Mouse => OutputBufferState::Mouse,
                    },
                    a20_gate,
                    memory,
                };
            }

            self.keyboard.restore(keyboard)?;
            self.mouse.restore(mouse)?;

            self.sync_interrupts();

            Ok(())
        }
    }
}
