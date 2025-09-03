// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A `CommandMatch` builder

use nvme_resources::fault::CommandMatch;
use nvme_spec::Command;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// A builder that can be used to generate `CommandMatch` patterns.
/// Usage:
/// Match to any admin command with cid == 10
/// ```rust
/// # use nvme_spec::Cdw0;
/// # use nvme_test::command_match::CommandMatchBuilder;
/// CommandMatchBuilder::new().match_cdw0(
///     Cdw0::new().with_cid(10).into(),
///     Cdw0::new().with_cid(u16::MAX).into(),  // Mask all the cid bits for an exact match on cid(10)
/// )
/// .build();
/// ```
///
/// Match to any admin command with opcode == 0x01
/// ```rust
/// # use nvme_test::command_match::CommandMatchBuilder;
/// CommandMatchBuilder::new().match_cdw0_opcode(0x01).build();
/// ```
pub struct CommandMatchBuilder {
    command: Command,
    mask: Command,
}

impl CommandMatchBuilder {
    /// Generates a matcher for every command
    pub fn new() -> Self {
        CommandMatchBuilder {
            command: Command::new_zeroed(),
            mask: Command::new_zeroed(),
        }
    }

    /// Configure to match to an opcode. See struct docs for usage
    pub fn match_cdw0_opcode(&mut self, opcode: u8) -> &mut Self {
        self.command.cdw0 = self.command.cdw0.with_opcode(opcode);
        self.mask.cdw0 = self.mask.cdw0.with_opcode(u8::MAX);
        self
    }

    /// Configure to match a cdw0 pattern. Mask specifies which bits to match on.
    /// See struct docs for usage
    pub fn match_cdw0(&mut self, cdw0: u32, mask: u32) -> &mut Self {
        self.command.cdw0 = cdw0.into();
        self.mask.cdw0 = mask.into();
        self
    }

    /// Configure to match a cdw10 pattern. Mask specifies which bits to match on.
    /// See struct docs for usage
    pub fn match_cdw10(&mut self, cdw10: u32, mask: u32) -> &mut Self {
        self.command.cdw10 = cdw10;
        self.mask.cdw10 = mask;
        self
    }

    /// Returns a `CommandMatch` corresponding to the builder configuration
    pub fn build(&self) -> CommandMatch {
        CommandMatch {
            command: self.command,
            mask: self
                .mask
                .as_bytes()
                .try_into()
                .expect("mask should be 64 bytes"),
        }
    }
}

/// Given a CommandMatch and a Command, return whether the command matches the pattern
pub fn match_command_pattern(match_pattern: &CommandMatch, command: &Command) -> bool {
    let command_lhs = match_pattern.command.as_bytes();
    let mask_bytes = &match_pattern.mask;

    let command_rhs = command.as_bytes();

    !command_lhs
        .iter()
        .zip(command_rhs.iter())
        .zip(mask_bytes.iter())
        .any(|((lhs, rhs), mask)| ((lhs ^ rhs) & mask) != 0)
}
