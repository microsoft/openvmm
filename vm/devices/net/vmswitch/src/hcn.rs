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
    parse_network_ids(&json)
}

/// Parse the JSON array of GUID strings returned by `HcnEnumerateNetworks`,
/// e.g. `["e2af8db9-d4e4-42ff-a695-85a71b928dd0", ...]`.
fn parse_network_ids(json: &str) -> Result<Vec<Guid>, Error> {
    fn parse_err(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
        Error {
            operation: "parse",
            err: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
        }
    }
    let ids: Vec<String> = serde_json::from_str(json).map_err(parse_err)?;
    ids.iter()
        .map(|id| id.parse::<Guid>().map_err(parse_err))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_network_ids;
    use guid::Guid;

    #[test]
    fn parse_plain_array() {
        let json =
            r#"["e2af8db9-d4e4-42ff-a695-85a71b928dd0","b807c2d2-0db8-401a-994a-840d4c0769b1"]"#;
        let guids = parse_network_ids(json).unwrap();
        assert_eq!(
            guids,
            vec![
                "e2af8db9-d4e4-42ff-a695-85a71b928dd0"
                    .parse::<Guid>()
                    .unwrap(),
                "b807c2d2-0db8-401a-994a-840d4c0769b1"
                    .parse::<Guid>()
                    .unwrap(),
            ]
        );
    }

    #[test]
    fn parse_braced_guids() {
        let json = r#"["{e2af8db9-d4e4-42ff-a695-85a71b928dd0}"]"#;
        let guids = parse_network_ids(json).unwrap();
        assert_eq!(
            guids,
            vec![
                "e2af8db9-d4e4-42ff-a695-85a71b928dd0"
                    .parse::<Guid>()
                    .unwrap()
            ]
        );
    }

    #[test]
    fn parse_empty_array() {
        assert!(parse_network_ids("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_invalid_json_fails() {
        assert!(parse_network_ids("not json").is_err());
    }
}
