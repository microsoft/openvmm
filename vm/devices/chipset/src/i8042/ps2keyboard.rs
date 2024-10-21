// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PS/2 keyboard.

use self::spec::Ps2KeyboardCommand;
use self::spec::ACKNOWLEDGE_COMMAND;
use futures::Stream;
use input_core::InputSource;
use input_core::KeyboardData;
use inspect::Inspect;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// PS/2 keyboard definitions.
mod spec {
    use inspect::Inspect;
    use open_enum::open_enum;

    open_enum! {
        #[derive(Inspect)]
        #[inspect(debug)]
        pub enum Ps2KeyboardCommand: u8 {
            TURN_ON_OFF_LE_DS       = 0xED,
            ECHO                    = 0xEE,
            SET_SCAN_CODE_SET       = 0xF0,
            IDENTIFY_KEYBOARD       = 0xF2,
            STANDARD_ENABLE         = 0xF6,
            RESEND                  = 0xFE,
            ENABLE                  = 0xF4,
            STANDARD_DISABLE        = 0xF5,
            SET_REPETITION_RATE     = 0xF3,
            SET3_COMMAND_F7         = 0xF7,
            SET3_COMMAND_F8         = 0xF8,
            SET3_COMMAND_F9         = 0xF9,
            SET3_COMMAND_FA         = 0xFA,
            SET3_COMMAND_FB         = 0xFB,
            SET3_COMMAND_FC         = 0xFC,
            SET3_COMMAND_FD         = 0xFD,
            RESET                   = 0xFF,
        }
    }

    pub const ACKNOWLEDGE_COMMAND: u8 = 0xFA;
}

#[derive(Inspect)]
struct KeyboardState {
    previous_command: Option<Ps2KeyboardCommand>,
    #[inspect(hex)]
    last_output_byte_read: u8,
    #[inspect(binary)]
    led_state: u8,
    #[inspect(bytes)]
    output_buffer: VecDeque<u8>,
}

impl KeyboardState {
    fn new() -> Self {
        Self {
            previous_command: None,
            last_output_byte_read: 0,
            led_state: 0,
            output_buffer: VecDeque::new(),
        }
    }
}

#[derive(Inspect)]
pub struct Ps2Keyboard {
    #[inspect(skip)]
    keyboard_input: Box<dyn InputSource<KeyboardData>>,
    #[inspect(flatten)]
    state: KeyboardState,
}

const KEYBOARD_BUFFER_SIZE: usize = 21;

impl Ps2Keyboard {
    pub fn new(keyboard_input: Box<dyn InputSource<KeyboardData>>) -> Self {
        Self {
            keyboard_input,
            state: KeyboardState::new(),
        }
    }

    pub fn reset(&mut self) {
        self.state = KeyboardState::new();
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) {
        // FUTURE: don't populate the buffer too fast, Hyper-V inserts a 2ms
        // delay between each keystroke.
        while self.state.output_buffer.len() < KEYBOARD_BUFFER_SIZE - 2 {
            if let Poll::Ready(Some(input)) = Pin::new(&mut self.keyboard_input).poll_next(cx) {
                if input.code > 0xff {
                    self.state.output_buffer.push_back((input.code >> 8) as u8);
                }
                self.state
                    .output_buffer
                    .push_back((input.code as u8) | if input.make { 0 } else { 0x80 });
            } else {
                break;
            }
        }
    }

    pub fn output(&mut self) -> Option<u8> {
        let value = self.state.output_buffer.pop_front()?;
        self.state.last_output_byte_read = value;
        Some(value)
    }

    fn push(&mut self, value: u8) {
        if self.state.output_buffer.len() <= KEYBOARD_BUFFER_SIZE {
            self.state.output_buffer.push_back(value);
        } else {
            // Indicate buffer overflow.
            *self.state.output_buffer.back_mut().unwrap() = 0;
        }
    }

    pub fn input(&mut self, input: u8) {
        let (command, data) = if let Some(command) = self.state.previous_command.take() {
            (command, Some(input))
        } else {
            (Ps2KeyboardCommand(input), None)
        };
        if self.command(command, data).is_none() {
            self.state.previous_command = Some(command);
        }
    }

    fn command(&mut self, command: Ps2KeyboardCommand, data: Option<u8>) -> Option<()> {
        tracing::debug!(?command, data, "keyboard command");
        match command {
            Ps2KeyboardCommand::TURN_ON_OFF_LE_DS => {
                self.push(ACKNOWLEDGE_COMMAND);
                let data = data?;
                if data == Ps2KeyboardCommand::TURN_ON_OFF_LE_DS.0 {
                    // Sometimes the BIOS doesn't receive the above ACK in
                    // time. I have tried lots of kludges to get around this
                    // problem, but the best I can come up with at this time
                    // is to detect when it resends the LED command the
                    // second time and just swallow the first one.
                    return None;
                } else {
                    self.state.led_state = data;
                }
            }
            Ps2KeyboardCommand::ECHO => {
                self.push(command.0);
            }
            Ps2KeyboardCommand::SET_SCAN_CODE_SET => {
                self.push(ACKNOWLEDGE_COMMAND);
                let scan_code_set = data?;
                if scan_code_set != 2 {
                    tracelimit::warn_ratelimited!(scan_code_set, "unsupported scan code set");
                }
            }
            Ps2KeyboardCommand::IDENTIFY_KEYBOARD => {
                // FUTURE: in Hyper-V, we delay this by 1ms "so that code in
                // Windows NT/2k/XP that assumes a delay will properly identify
                // our keyboard".
                self.push(ACKNOWLEDGE_COMMAND);
                self.push(0xab);
                self.push(0x41);
            }
            Ps2KeyboardCommand::STANDARD_ENABLE => {
                self.state = KeyboardState::new();
                self.push(ACKNOWLEDGE_COMMAND);
            }

            Ps2KeyboardCommand::ENABLE => {
                self.state.output_buffer.clear();
                self.push(ACKNOWLEDGE_COMMAND);
            }
            Ps2KeyboardCommand::STANDARD_DISABLE => {
                self.state = KeyboardState::new();
                self.push(ACKNOWLEDGE_COMMAND);
            }
            Ps2KeyboardCommand::SET_REPETITION_RATE => {
                // We will use the Host to do auto-repeat keys, so we won't
                // honor the change to the X86 keyboard. Just acknowledge that
                // we received the data byte (without actually doing anything
                // with it).
                self.push(ACKNOWLEDGE_COMMAND);
                let _ = data?;
            }
            Ps2KeyboardCommand::RESEND => {
                self.push(self.state.last_output_byte_read);
            }
            Ps2KeyboardCommand::RESET => {
                self.state = KeyboardState::new();
                self.push(ACKNOWLEDGE_COMMAND);
                self.push(0xaa);
            }
            Ps2KeyboardCommand::SET3_COMMAND_F7
            | Ps2KeyboardCommand::SET3_COMMAND_F8
            | Ps2KeyboardCommand::SET3_COMMAND_F9
            | Ps2KeyboardCommand::SET3_COMMAND_FA => {
                self.push(ACKNOWLEDGE_COMMAND);
            }
            Ps2KeyboardCommand::SET3_COMMAND_FB
            | Ps2KeyboardCommand::SET3_COMMAND_FC
            | Ps2KeyboardCommand::SET3_COMMAND_FD => {
                // Acknowledge parameter byte.
                // Since set3 is not supported, no extra action required.
                self.push(ACKNOWLEDGE_COMMAND);
                let _ = data?;
            }
            command => {
                tracelimit::warn_ratelimited!(?command, "invalid keyboard command");
                self.push(0xfe);
            }
        }
        Some(())
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
        #[mesh(package = "chipset.i8042.keyboard")]
        pub struct SavedState {
            #[mesh(1)]
            pub previous_command: Option<u8>,
            #[mesh(2)]
            pub last_output_byte_read: u8,
            #[mesh(3)]
            pub led_state: u8,
            #[mesh(4)]
            pub output_buffer: Vec<u8>,
        }
    }

    impl SaveRestore for Ps2Keyboard {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let KeyboardState {
                previous_command,
                last_output_byte_read,
                led_state,
                ref output_buffer,
            } = self.state;

            let save_state = state::SavedState {
                previous_command: previous_command.map(|x| x.0),
                last_output_byte_read,
                led_state,
                output_buffer: output_buffer.iter().copied().collect(),
            };

            Ok(save_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                previous_command,
                last_output_byte_read,
                led_state,
                output_buffer,
            } = state;

            self.state = KeyboardState {
                previous_command: previous_command.map(Ps2KeyboardCommand),
                last_output_byte_read,
                led_state,
                output_buffer: output_buffer.into(),
            };

            Ok(())
        }
    }
}
