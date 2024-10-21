// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::gdb::arch::x86::X86_64_QEMU;
use crate::gdb::targets::VmTarget;
use gdbstub::target;
use gdbstub::target::TargetError;
use gdbstub::target::TargetResult;

/// Copy all bytes of `data` to `buf`.
/// Return the size of data copied.
fn copy_to_buf(data: &[u8], buf: &mut [u8]) -> usize {
    let len = buf.len().min(data.len());
    buf[..len].copy_from_slice(&data[..len]);
    len
}

/// Copy a range of `data` (start at `offset` with a size of `length`) to `buf`.
/// Return the size of data copied. Returns 0 if `offset >= buf.len()`.
///
/// Mainly used by qXfer:_object_:read commands.
fn copy_range_to_buf(data: &[u8], offset: u64, length: usize, buf: &mut [u8]) -> usize {
    let offset = offset as usize;
    if offset > data.len() {
        return 0;
    }

    let start = offset;
    let end = (offset + length).min(data.len());
    copy_to_buf(&data[start..end], buf)
}

impl target::ext::target_description_xml_override::TargetDescriptionXmlOverride
    for VmTarget<'_, X86_64_QEMU>
{
    fn target_description_xml(
        &self,
        annex: &[u8],
        offset: u64,
        length: usize,
        buf: &mut [u8],
    ) -> TargetResult<usize, Self> {
        let xml = match annex {
            b"target.xml" => TARGET_XML.trim(),
            // pulled from QEMU
            b"i386-64bit.xml" => include_str!("./i386-64bit.xml").trim(),
            _ => return Err(TargetError::NonFatal),
        };

        Ok(copy_range_to_buf(
            xml.trim().as_bytes(),
            offset,
            length,
            buf,
        ))
    }
}

/// ExdiGdbSrv doesn't parse XML with newlines in it.
const TARGET_XML: &str = r#"
<?xml version="1.0"?><!DOCTYPE target SYSTEM "gdb-target.dtd"><target><architecture>i386:x86-64</architecture><xi:include href="i386-64bit.xml"/></target>
"#;
