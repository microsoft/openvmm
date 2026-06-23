// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Handler for the mount request (Linux only).

// UNSAFETY: Required for libc::mount() syscall.
#![expect(unsafe_code)]

use std::ffi::CString;

pub fn handle_mount(request: pipette_protocol::MountRequest) -> anyhow::Result<()> {
    tracing::debug!(
        source = request.source,
        target = request.target,
        fstype = request.fstype,
        flags = request.flags,
        "mount request"
    );

    if request.mkdir_target {
        std::fs::create_dir_all(&request.target)?;
    }

    let source = CString::new(request.source)?;
    let target = CString::new(request.target)?;
    let fstype = CString::new(request.fstype)?;

    // SAFETY: calling libc::mount with valid C strings and null data pointer.
    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            request.flags,
            std::ptr::null(),
        )
    };

    if ret != 0 {
        anyhow::bail!("mount failed: {}", std::io::Error::last_os_error());
    }

    Ok(())
}
