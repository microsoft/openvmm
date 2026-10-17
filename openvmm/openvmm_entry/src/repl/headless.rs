// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Headless operation of the REPL, for runs driven by another process instead
//! of an interactive console.
//!
//! With a stdin that is not a terminal, such as a pipe, the stdio thread reads
//! it without switching the console to raw mode.

use std::io;
use std::io::IsTerminal;

/// Enables raw console mode for the console input of the stdio thread, unless
/// stdin is not a terminal. Returns whether raw mode was enabled.
pub(super) fn enable_raw_mode(stdin: &io::Stdin) -> io::Result<bool> {
    let terminal = stdin.is_terminal();
    if terminal {
        crossterm::terminal::enable_raw_mode()?;
    }
    Ok(terminal)
}

/// Disables raw console mode if [`enable_raw_mode`] enabled it.
pub(super) fn disable_raw_mode(enabled: bool) -> io::Result<()> {
    if enabled {
        crossterm::terminal::disable_raw_mode()?;
    }
    Ok(())
}
