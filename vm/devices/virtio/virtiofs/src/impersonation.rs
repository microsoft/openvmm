// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serving a file system under a user token on Windows.

// UNSAFETY: Calling Win32 APIs and constructing owned handles from their outputs.
#![expect(unsafe_code)]

use std::io;
use std::marker::PhantomData;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::RawHandle;
use std::ptr;
use windows_sys::Win32::Security;
use windows_sys::Win32::System::Threading::SetThreadToken;

/// Creates an impersonation token, owned by this process, from a token handle
/// value duplicated into this process by the RPC caller.
pub fn duplicate_user_token(token: u64) -> io::Result<OwnedHandle> {
    let token = usize::try_from(token).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    duplicate_impersonation_token(token as RawHandle)
}

fn duplicate_impersonation_token(token: RawHandle) -> io::Result<OwnedHandle> {
    let mut duplicate = ptr::null_mut();
    // SAFETY: DuplicateTokenEx does not take ownership of `token`. On success,
    // `duplicate` is a new handle owned by this process.
    unsafe {
        if Security::DuplicateTokenEx(
            token,
            Security::TOKEN_IMPERSONATE | Security::TOKEN_QUERY | Security::TOKEN_DUPLICATE,
            ptr::null(),
            Security::SecurityImpersonation,
            Security::TokenImpersonation,
            &mut duplicate,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        Ok(OwnedHandle::from_raw_handle(duplicate))
    }
}

/// Impersonates a token on the current thread until dropped.
pub(crate) struct ImpersonationGuard {
    _not_send: PhantomData<*const ()>,
}

impl ImpersonationGuard {
    pub fn new(token: &OwnedHandle) -> io::Result<Self> {
        // SAFETY: A null thread selects the current thread, and `token` is a
        // valid handle for the duration of the call.
        if unsafe { SetThreadToken(ptr::null(), token.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        // SAFETY: Calling Win32 API as documented.
        if unsafe { Security::RevertToSelf() } == 0 {
            panic!(
                "failed to revert virtio-fs impersonation: {}",
                io::Error::last_os_error()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    use windows_sys::Win32::System::Threading::OpenThreadToken;

    fn process_token() -> OwnedHandle {
        let mut process_token = ptr::null_mut();
        let mut token = ptr::null_mut();
        // SAFETY: Calling Win32 APIs as documented.
        unsafe {
            assert_ne!(
                OpenProcessToken(
                    GetCurrentProcess(),
                    Security::TOKEN_DUPLICATE,
                    &mut process_token
                ),
                0
            );
            let process_token = OwnedHandle::from_raw_handle(process_token);
            assert_ne!(
                Security::DuplicateTokenEx(
                    process_token.as_raw_handle(),
                    Security::TOKEN_IMPERSONATE | Security::TOKEN_QUERY | Security::TOKEN_DUPLICATE,
                    ptr::null(),
                    Security::SecurityImpersonation,
                    Security::TokenImpersonation,
                    &mut token,
                ),
                0
            );
            OwnedHandle::from_raw_handle(token)
        }
    }

    fn thread_token() -> io::Result<OwnedHandle> {
        let mut token = ptr::null_mut();
        // SAFETY: Calling Win32 API as documented.
        unsafe {
            if OpenThreadToken(GetCurrentThread(), Security::TOKEN_QUERY, 1, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedHandle::from_raw_handle(token))
        }
    }

    #[test]
    fn rejects_non_token_handle() {
        duplicate_user_token(0).unwrap_err();
    }

    #[test]
    fn impersonates_until_dropped() {
        let process_token = process_token();
        let token_handle = process_token.as_raw_handle() as usize as u64;
        let token = duplicate_user_token(token_handle).unwrap();
        {
            let _guard = ImpersonationGuard::new(&token).unwrap();
            thread_token().unwrap();
        }

        let err = thread_token().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ERROR_NO_TOKEN as i32));
    }
}
