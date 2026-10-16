// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows security API wrappers.

use std::ffi::c_void;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::windows::prelude::*;
use std::ptr::NonNull;
use std::ptr::null_mut;
use std::str::FromStr;
use widestring::U16CStr;
use widestring::U16CString;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::DeriveCapabilitySidsFromName;
use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::LABEL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::Security::SACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

const MAX_SUBAUTHORITY_COUNT: usize = 15;

/// A Windows SID.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Sid<T: ?Sized = [u32]> {
    revision: u8,
    sub_authority_count: u8,
    identifier_authority: [u8; 6],
    sub_authorities: T,
}

/// A SID that can contain the maximum number of subauthorities.
pub type MaximumSid = Sid<[u32; MAX_SUBAUTHORITY_COUNT]>;

impl<const N: usize> Sid<[u32; N]> {
    /// Creates a new SID.
    pub fn new(identifier_authority: [u8; 6], sub_authorities: [u32; N]) -> Self {
        assert!(N <= MAX_SUBAUTHORITY_COUNT);
        Self {
            revision: 1,
            sub_authority_count: N as u8,
            identifier_authority,
            sub_authorities,
        }
    }
}

impl<T: ?Sized> Debug for Sid<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&self.to_string_sid())
    }
}

impl<T: ?Sized> Sid<T> {
    /// Returns a `PSID` pointer for use with Win32 APIs.
    pub fn as_ptr(&self) -> PSID {
        std::ptr::from_ref(self) as PSID
    }

    /// Constructs the string representation of a SID.
    pub fn to_string_sid(&self) -> String {
        // SAFETY: calling Win32 APIs according to doc.
        unsafe {
            let mut s16 = null_mut();
            if ConvertSidToStringSidW(self.as_ptr(), &mut s16) == 0 {
                panic!(
                    "ConvertSidToStringSidW failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            let s = U16CStr::from_ptr_str(s16).to_string().unwrap();
            LocalFree(s16.cast());
            s
        }
    }
}

impl AsRef<Sid> for Sid {
    fn as_ref(&self) -> &Sid {
        self
    }
}

impl<const N: usize> AsRef<Sid> for Sid<[u32; N]> {
    fn as_ref(&self) -> &Sid {
        self
    }
}

impl From<&Sid> for MaximumSid {
    fn from(sid: &Sid) -> Self {
        let n = sid.sub_authority_count;
        assert!(n <= 15);
        let mut this = Self {
            revision: 1,
            sub_authority_count: n,
            identifier_authority: sid.identifier_authority,
            sub_authorities: [0; 15],
        };
        this.sub_authorities[..n.into()].copy_from_slice(&sid.sub_authorities[..n.into()]);
        this
    }
}

/// A SID that has been allocated with LocalAlloc.
///
// This is mostly useful for interacting with Win32 APIs that allocate SIDs.
#[repr(transparent)]
pub struct LocalSid(NonNull<Sid>);

impl Debug for LocalSid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.as_ref(), f)
    }
}

impl LocalSid {
    pub fn from_capability_name(name: &str) -> std::io::Result<Self> {
        // SAFETY: calling Win32 APIs according to doc.
        unsafe {
            let mut group_count = 0;
            let mut count = 0;
            let mut group_sid_array = null_mut();
            let mut sid_array = null_mut();
            if DeriveCapabilitySidsFromName(
                U16CString::from_str(name).unwrap().as_ptr(),
                &mut group_sid_array,
                &mut group_count,
                &mut sid_array,
                &mut count,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }

            // Free all the group SIDs (unused).
            let group_sids = std::slice::from_raw_parts_mut(group_sid_array, group_count as usize);
            for sid in group_sids {
                LocalFree(*sid);
            }
            LocalFree(group_sid_array.cast());

            // Take just the first SID (there should really never be more than one).
            let sids = std::slice::from_raw_parts_mut(sid_array, count as usize);
            let cap_sid = Self::from_raw_sid(sids[0]);
            for sid in &sids[1..] {
                LocalFree(*sid);
            }
            LocalFree(sid_array.cast());
            Ok(cap_sid)
        }
    }

    /// Takes ownership of a `PSID` pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `sid` has been allocated with `LocalAlloc`
    /// and is exclusively owned.
    pub unsafe fn from_raw_sid(sid: PSID) -> Self {
        unsafe {
            let auth_count = (*(sid as *const Sid<[u32; 0]>)).sub_authority_count;
            let slice = std::slice::from_raw_parts_mut(sid, auth_count.into());
            std::mem::transmute(slice)
        }
    }
}

impl AsRef<Sid> for LocalSid {
    fn as_ref(&self) -> &Sid {
        self
    }
}

impl Deref for LocalSid {
    type Target = Sid;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the pointer is guaranteed to be valid.
        unsafe { &*self.0.as_ptr() }
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        // SAFETY: the pointer is guaranteed to be valid.
        unsafe {
            LocalFree(self.as_ptr());
        }
    }
}

/// A Windows security descriptor allocated with `LocalAlloc`.
///
/// Guaranteed to be in self-relative form.
#[derive(Clone)]
pub struct LocalSecurityDescriptor(NonNull<c_void>, usize);

impl Debug for LocalSecurityDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}

impl Deref for LocalSecurityDescriptor {
    type Target = SecurityDescriptor;

    fn deref(&self) -> &Self::Target {
        // SAFETY: the pointer and length are guaranteed to be valid for the
        // lifetime of self.
        unsafe {
            std::mem::transmute(std::slice::from_raw_parts(
                self.0.as_ptr() as *const u8,
                self.1,
            ))
        }
    }
}

impl FromStr for LocalSecurityDescriptor {
    type Err = std::io::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // SAFETY: calling Win32 API according to doc and taking ownership of
        // the allocated buffer.
        unsafe {
            let mut ptr = null_mut();
            let mut len = 0;
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                U16CString::from_str(s)
                    .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?
                    .as_ptr(),
                SDDL_REVISION_1,
                &mut ptr,
                &mut len,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(NonNull::new(ptr).unwrap(), len as usize))
        }
    }
}

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the pointer is guaranteed to be valid and owned.
        unsafe {
            LocalFree(self.0.as_ptr().cast());
        }
    }
}

/// A security descriptor buffer.
///
/// Guaranteed to be in self-relative form.
#[repr(transparent)]
pub struct SecurityDescriptor([u8]);

impl Debug for SecurityDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Ok(sddl) = self.to_sddl() {
            f.pad(&sddl)
        } else {
            write!(f, "{:?}", &self.0)
        }
    }
}

impl SecurityDescriptor {
    /// Converts the security descriptor to SDDL string format.
    pub fn to_sddl(&self) -> std::io::Result<String> {
        // SAFETY: calling Win32 API according to doc.
        unsafe {
            let mut s16 = null_mut();
            if ConvertSecurityDescriptorToStringSecurityDescriptorW(
                self.as_ptr(),
                SDDL_REVISION_1,
                OWNER_SECURITY_INFORMATION
                    | GROUP_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | SACL_SECURITY_INFORMATION
                    | LABEL_SECURITY_INFORMATION,
                &mut s16,
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let s = U16CStr::from_ptr_str(s16).to_string().unwrap();
            LocalFree(s16.cast::<c_void>());
            Ok(s)
        }
    }

    /// Returns a `PSECURITY_DESCRIPTOR` pointer for use with Win32 APIs.
    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0.as_ptr() as _
    }
}

#[link(
    name = "api-ms-win-security-base-private-l1-1-1.dll",
    kind = "raw-dylib",
    modifiers = "+verbatim"
)]
unsafe extern "C" {
    fn CreateAppContainerToken(
        token: HANDLE,
        caps: *mut SECURITY_CAPABILITIES,
        new_token: *mut HANDLE,
    ) -> windows_sys::core::BOOL;
}

#[repr(transparent)]
struct SidAndAttributes<'a>(SID_AND_ATTRIBUTES, PhantomData<&'a Sid>);

impl<'a> SidAndAttributes<'a> {
    pub fn new(sid: &'a Sid, attributes: u32) -> Self {
        Self(
            SID_AND_ATTRIBUTES {
                Sid: sid.as_ptr(),
                Attributes: attributes,
            },
            PhantomData,
        )
    }
}

/// Creates an app container token for `sid` with `capabilities`.
pub fn create_app_container_token<'a, I, T>(
    sid: &Sid,
    capabilities: I,
) -> std::io::Result<OwnedHandle>
where
    I: IntoIterator<Item = &'a T>,
    T: 'a + AsRef<Sid>,
{
    let mut caps_and_attrs: Vec<_> = capabilities
        .into_iter()
        .map(|c| SidAndAttributes::new(c.as_ref(), SE_GROUP_ENABLED as u32))
        .collect();
    let mut caps = SECURITY_CAPABILITIES {
        AppContainerSid: sid.as_ptr(),
        Capabilities: caps_and_attrs.as_mut_ptr().cast(),
        CapabilityCount: caps_and_attrs
            .len()
            .try_into()
            .expect("too many capabilities"),
        Reserved: 0,
    };
    // SAFETY: calling Win32 API according to doc and taking ownership of the
    // handle.
    unsafe {
        let mut new_token = null_mut();
        if CreateAppContainerToken(null_mut(), &mut caps, &mut new_token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(new_token))
    }
}

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
        // TOKEN_USER contains a pointer, so its backing storage must have
        // pointer alignment before it is dereferenced below.
        let mut buffer = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
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
    use test_with_tracing::test;

    #[test]
    fn current_process_sid_has_a_bounded_binary_identity() {
        let sid = current_process_user_sid().unwrap();
        let (bytes, length) = sid.to_fixed_bytes();
        assert!((8..=68).contains(&length));
        assert_eq!(bytes[0], 1);
        assert!(sid.to_string_sid().starts_with("S-1-"));
    }
}
