// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guid::Guid;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::ptr::null_mut;
use thiserror::Error;
use widestring::U16CStr;
use windows_sys::Win32::System::Com::CoTaskMemFree;

pal::delayload!("computenetwork.dll" {
    fn HcnOpenNetwork(id: &Guid, network: &mut *mut c_void, error_record: *mut *mut u16) -> i32;
    fn HcnCloseNetwork(network: NonNull<c_void>) -> i32;
    fn HcnEnumerateNetworks(query: *const u16, networks: &mut *mut u16, error_record: *mut *mut u16) -> i32;
});

#[derive(Debug, Error)]
#[error("HCN {0} failed", operation)]
pub struct Error {
    operation: &'static str,
    #[source]
    err: std::io::Error,
}

fn chk(operation: &'static str, result: i32) -> Result<i32, Error> {
    if result >= 0 {
        Ok(result)
    } else {
        Err(Error {
            operation,
            err: std::io::Error::from_raw_os_error(result),
        })
    }
}

pub struct Network(NonNull<c_void>);

impl Network {
    pub fn open(id: &Guid) -> Result<Self, Error> {
        let mut network = null_mut();
        chk("open", unsafe {
            HcnOpenNetwork(id, &mut network, null_mut())
        })?;
        Ok(Self(
            NonNull::new(network).expect("HcnOpenNetwork returned null network"),
        ))
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        if let Err(e) = chk("close", unsafe { HcnCloseNetwork(self.0) }) {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "failed to close HCN network"
            );
        }
    }
}

/// The well-known GUID of the Hyper-V Default Switch.
///
/// Provisioned automatically when the Hyper-V optional feature is
/// installed; provides a NAT'd network for VMs.
pub const DEFAULT_SWITCH: Guid = guid::guid!("c08cb7b8-9b3c-408e-8e30-5e16a3aeb444");

/// Returns the GUIDs of all HCN networks (vmswitches) currently
/// registered on the host, in the order reported by HCN.
///
/// On a host without Hyper-V installed, or where `computenetwork.dll`
/// cannot be loaded, this returns an error.
pub fn enumerate_networks() -> Result<Vec<Guid>, Error> {
    let mut raw: *mut u16 = null_mut();
    chk("enumerate", unsafe {
        HcnEnumerateNetworks(null_mut(), &mut raw, null_mut())
    })?;
    if raw.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: HcnEnumerateNetworks returns a NUL-terminated UTF-16
    // string allocated via CoTaskMemAlloc. We own the buffer until we
    // free it with CoTaskMemFree below.
    let json = unsafe { U16CStr::from_ptr_str(raw) }.to_string_lossy();
    // SAFETY: per HCN API contract, the returned buffer must be freed
    // with CoTaskMemFree.
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(extract_guids(&json))
}

/// Pull every GUID-shaped substring out of `s`.
///
/// HcnEnumerateNetworks returns a JSON array of GUID strings (with or
/// without surrounding braces). Rather than depend on a JSON parser for
/// this single use, we scan for 36-character GUID patterns directly,
/// which is robust to either format.
fn extract_guids(s: &str) -> Vec<Guid> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 36 <= bytes.len() {
        if let Ok(slice) = std::str::from_utf8(&bytes[i..i + 36]) {
            if let Ok(g) = slice.parse::<Guid>() {
                out.push(g);
                i += 36;
                continue;
            }
        }
        i += 1;
    }
    out
}
