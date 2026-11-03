// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serving a file system under a user token on Windows.

// UNSAFETY: Calling Win32 APIs and constructing owned handles from their outputs.
#![expect(unsafe_code)]

use std::io;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::RawHandle;
use std::ptr;
use windows_sys::Win32::Security;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::VirtioFsSession;
    use fuse::Fuse;
    use fuse::ReplySender;
    use fuse::Request;
    use fuse::SessionInfo;
    use fuse::protocol::*;
    use pal::windows::security::RevertToSelfGuard;
    use std::os::windows::io::AsRawHandle;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use windows_sys::Win32::Foundation::ERROR_NO_TOKEN;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    use windows_sys::Win32::System::Threading::OpenThreadToken;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

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
            let _guard = RevertToSelfGuard::impersonate(&token).unwrap();
            thread_token().unwrap();
        }

        let err = thread_token().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ERROR_NO_TOKEN as i32));
    }

    fn restricted_token() -> OwnedHandle {
        let token = process_token();
        let mut world_sid = [0u32; Security::SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
        let mut sid_size = size_of_val(&world_sid) as u32;
        let mut restricted = ptr::null_mut();
        // SAFETY: The SID buffer is aligned and large enough for a well-known
        // SID. All inputs remain valid until the new token handle is returned.
        unsafe {
            assert_ne!(
                Security::CreateWellKnownSid(
                    Security::WinWorldSid,
                    ptr::null_mut(),
                    world_sid.as_mut_ptr().cast(),
                    &mut sid_size,
                ),
                0
            );
            let sid = Security::SID_AND_ATTRIBUTES {
                Sid: world_sid.as_mut_ptr().cast(),
                Attributes: 0,
            };
            assert_ne!(
                Security::CreateRestrictedToken(
                    token.as_raw_handle(),
                    Security::DISABLE_MAX_PRIVILEGE,
                    0,
                    ptr::null(),
                    0,
                    ptr::null(),
                    1,
                    &sid,
                    &mut restricted,
                ),
                0
            );
            OwnedHandle::from_raw_handle(restricted)
        }
    }

    fn token_id(token: &OwnedHandle) -> (u32, i32) {
        let mut stats = Security::TOKEN_STATISTICS::default();
        let mut size = 0;
        // SAFETY: The token is valid and the output buffer matches the queried
        // information class.
        unsafe {
            assert_ne!(
                Security::GetTokenInformation(
                    token.as_raw_handle(),
                    Security::TokenStatistics,
                    ptr::from_mut(&mut stats).cast(),
                    size_of_val(&stats) as u32,
                    &mut size,
                ),
                0
            );
        }
        (stats.TokenId.LowPart, stats.TokenId.HighPart)
    }

    struct TokenCheckingFs {
        token_id: (u32, i32),
        calls: Arc<AtomicUsize>,
    }

    impl TokenCheckingFs {
        fn check_identity(&self) {
            let token = thread_token().unwrap();
            assert_eq!(token_id(&token), self.token_id);
            // SAFETY: The handle refers to a valid token.
            let restricted = unsafe { Security::IsTokenRestricted(token.as_raw_handle()) };
            assert_ne!(restricted, 0);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Fuse for TokenCheckingFs {
        fn init(&self, _info: &mut SessionInfo) {
            self.check_identity();
        }

        fn get_attr(&self, _request: &Request, _flags: u32, _fh: u64) -> lx::Result<fuse_attr_out> {
            self.check_identity();
            Err(lx::Error::EACCES)
        }
    }

    #[derive(Default)]
    struct TestSender(Vec<u8>);

    impl ReplySender for TestSender {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            self.0 = bufs.iter().flat_map(|buf| buf.iter().copied()).collect();
            Ok(())
        }
    }

    #[test]
    fn session_dispatch_impersonates_restricted_token_and_reverts() {
        assert_eq!(
            thread_token().unwrap_err().raw_os_error(),
            Some(ERROR_NO_TOKEN as i32)
        );
        let token = restricted_token();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = VirtioFsSession::new(TokenCheckingFs {
            token_id: token_id(&token),
            calls: calls.clone(),
        })
        .with_user_token(Some(token));
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            ..FromZeros::new_zeroed()
        };
        let getattr = fuse_getattr_in::new_zeroed();
        for (opcode, args, expected_error) in [
            (FUSE_INIT, init.as_bytes(), 0),
            (FUSE_GETATTR, getattr.as_bytes(), -lx::Error::EACCES.value()),
        ] {
            let header = fuse_in_header {
                len: (size_of::<fuse_in_header>() + args.len()) as u32,
                opcode,
                unique: 1,
                nodeid: 1,
                ..FromZeros::new_zeroed()
            };
            let mut request = header.as_bytes().to_vec();
            request.extend_from_slice(args);
            let mut sender = TestSender::default();
            session.dispatch(Request::new(request.as_slice()).unwrap(), &mut sender, None);
            let (reply, _) = fuse_out_header::read_from_prefix(&sender.0).unwrap();
            assert_eq!(reply.error, expected_error);
            assert_eq!(
                thread_token().unwrap_err().raw_os_error(),
                Some(ERROR_NO_TOKEN as i32)
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
