// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! User SIDs of local processes, and a fixed-size binary encoding of SIDs.

use super::MAX_SUBAUTHORITY_COUNT;
use super::MaximumSid;
use std::io::ErrorKind;
use std::os::windows::prelude::*;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

impl MaximumSid {
    /// Returns the binary SID length encoded by this value.
    pub fn byte_len(&self) -> usize {
        8 + usize::from(self.sub_authority_count) * size_of::<u32>()
    }

    /// Returns a zero-padded fixed representation and its meaningful length.
    pub fn to_fixed_bytes(self) -> ([u8; 68], u8) {
        let length = self.byte_len();
        assert!(length <= 68);
        let mut bytes = [0; 68];
        // SAFETY: `MaximumSid` is a 68-byte repr(C) value and `length` was
        // derived from its validated subauthority count.
        unsafe {
            bytes[..length].copy_from_slice(std::slice::from_raw_parts(
                std::ptr::from_ref(&self).cast::<u8>(),
                length,
            ));
        }
        (bytes, length as u8)
    }
}

fn process_user_sid_from_handle(process: HANDLE) -> std::io::Result<MaximumSid> {
    // SAFETY: the process handle is valid for TOKEN_QUERY and all output
    // buffers remain live for the documented calls.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);

        let mut required = 0;
        let _ = GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            null_mut(),
            0,
            &mut required,
        );
        if required < size_of::<TOKEN_USER>() as u32 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; required as usize];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let token_user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let length = GetLengthSid(token_user.User.Sid) as usize;
        if !(8..=size_of::<MaximumSid>()).contains(&length) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "process token contains an invalid user SID",
            ));
        }
        let mut sid = MaximumSid::new([0; 6], [0; MAX_SUBAUTHORITY_COUNT]);
        std::ptr::copy_nonoverlapping(
            token_user.User.Sid.cast::<u8>(),
            std::ptr::from_mut(&mut sid).cast::<u8>(),
            length,
        );
        Ok(sid)
    }
}

/// Returns the current process user SID.
pub fn current_process_user_sid() -> std::io::Result<MaximumSid> {
    process_user_sid_from_handle(unsafe { GetCurrentProcess() })
}

/// Returns the user SID for a local process.
pub fn process_user_sid(process_id: u32) -> std::io::Result<MaximumSid> {
    // SAFETY: OpenProcess is called with a query-only access mask and a
    // concrete process identifier supplied by the named-pipe subsystem.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful OpenProcess result is uniquely owned here.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    process_user_sid_from_handle(process.as_raw_handle())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_sid_has_a_bounded_binary_identity() {
        let sid = current_process_user_sid().unwrap();
        let (bytes, length) = sid.to_fixed_bytes();
        assert!((8..=68).contains(&length));
        assert_eq!(bytes[0], 1);
        assert!(sid.to_string_sid().starts_with("S-1-"));
    }
}
