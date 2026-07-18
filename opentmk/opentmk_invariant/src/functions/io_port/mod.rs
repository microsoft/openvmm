// UNSAFETY: This module contains unsafe code because we are doing raw I/O port via within a fuzzer
// context
#![expect(unsafe_code)]

use crate::functions::{FuzzFunctionVariable, VerifyFuzzVariables};
#[allow(unused_imports)]
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
        // Do a sanity check first on whether if the memory address is valid
        mem.try_write_mem(value, &[0])?;

        // Perform and write inb result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inb(port)
        };
        mem.write_mem(value, &[res]);
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
        // Do a sanity check first on whether if the memory address is valid
        mem.try_write_mem(value, &[0; 2])?;

        // Perform and write inb result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inw(port)
        };
        mem.write_mem(value, &res.to_le_bytes());
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
        // Do a sanity check first on whether if the memory address is valid
        mem.try_write_mem(value, &[0; 4])?;

        // Perform and write inb result
        let res = unsafe {
            // SAFETY: this is called within a fuzzer context. We assume all unsafe risks
            arch::io::inl(port)
        };
        mem.write_mem(value, &res.to_le_bytes());
    }
    Ok(FuzzFunctionVariable::Void)
}
