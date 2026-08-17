// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: This module contains unsafe code because we are doing raw I/O port via within a fuzzer
// context
#![expect(unsafe_code)]

use crate::functions::{FuzzFunctionVariable, VerifyFuzzVariables};
#[cfg_attr(not(target_os = "uefi"), expect(unused_imports))]
use crate::prelude::*;

use inv_decoder::SafeMemoryMap;

use opentmk_core::arch;

fn validate_ioport(port: u16) -> Option<u16> {
    if (0x3E8..0x3F0).contains(&port) || (0x2E8..0x300).contains(&port) {
        Some(port)
    } else {
        None
    }
}

pub fn write_ioport_u8(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as u8;

    if let Some(port) = port {
        unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::outb(port, value);
        }
    }
    Ok(FuzzFunctionVariable::Void)
}

pub fn read_ioport_u8(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as usize;
    if let Some(port) = port {
        // Perform and write inb result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inb(port)
        };

        if let Err(e) = mem.try_write_mem(value, &[res]) {
            log::warn!("Failed to write inb output: {e}")
        }
    }
    Ok(FuzzFunctionVariable::Void)
}

pub fn write_ioport_u16(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as u16;

    if let Some(port) = port {
        unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::outw(port, value);
        }
    }

    Ok(FuzzFunctionVariable::Void)
}

pub fn read_ioport_u16(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as usize;
    if let Some(port) = port {
        // Perform and write inh result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inw(port)
        };

        if let Err(e) = mem.try_write_mem(value, &res.to_le_bytes()) {
            log::warn!("Failed to write inh output: {e}")
        }
    }
    Ok(FuzzFunctionVariable::Void)
}

pub fn write_ioport_u32(
    _mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as u32;

    if let Some(port) = port {
        unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::outl(port, value);
        }
    }

    Ok(FuzzFunctionVariable::Void)
}

pub fn read_ioport_u32(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    let [port, value] = vars.verify_num_params::<2>()?;
    let port = validate_ioport(port.expect_int("Port")? as u16);
    let value = value.expect_int("Value")? as usize;
    if let Some(port) = port {
        // Perform and write inl result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inl(port)
        };

        if let Err(e) = mem.try_write_mem(value, &res.to_le_bytes()) {
            log::warn!("Failed to write inl output: {e}")
        }
    }
    Ok(FuzzFunctionVariable::Void)
}
