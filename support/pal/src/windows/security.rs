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
use winapi::shared::minwindef::BOOL;
use winapi::shared::sddl::ConvertSecurityDescriptorToStringSecurityDescriptorW;
use winapi::shared::sddl::ConvertSidToStringSidW;
use winapi::shared::sddl::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use winapi::shared::sddl::SDDL_REVISION_1;
use winapi::um::securitybaseapi::DeriveCapabilitySidsFromName;
use winapi::um::winbase::LocalFree;
use winapi::um::winnt::DACL_SECURITY_INFORMATION;
use winapi::um::winnt::GROUP_SECURITY_INFORMATION;
use winapi::um::winnt::HANDLE;
use winapi::um::winnt::LABEL_SECURITY_INFORMATION;
use winapi::um::winnt::LPSECURITY_CAPABILITIES;
use winapi::um::winnt::OWNER_SECURITY_INFORMATION;
use winapi::um::winnt::PHANDLE;
use winapi::um::winnt::PSECURITY_DESCRIPTOR;
use winapi::um::winnt::PSID;
use winapi::um::winnt::SACL_SECURITY_INFORMATION;
use winapi::um::winnt::SE_GROUP_ENABLED;
use winapi::um::winnt::SECURITY_CAPABILITIES;
use winapi::um::winnt::SID_AND_ATTRIBUTES;

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
                SDDL_REVISION_1.into(),
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
                SDDL_REVISION_1.into(),
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

#[link(name = "api-ms-win-security-base-private-l1-1-1")]
unsafe extern "C" {
    fn CreateAppContainerToken(
        token: HANDLE,
        caps: LPSECURITY_CAPABILITIES,
        new_token: PHANDLE,
    ) -> BOOL;
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
        .map(|c| SidAndAttributes::new(c.as_ref(), SE_GROUP_ENABLED))
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
