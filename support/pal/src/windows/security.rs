// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows security API wrappers.

use std::ffi::c_void;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::ops::Deref;
use std::os::windows::prelude::*;
use std::path::Path;
use std::ptr::NonNull;
use std::ptr::null_mut;
use std::str::FromStr;
use widestring::U16CStr;
use widestring::U16CString;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::DeriveCapabilitySidsFromName;
use windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION;
use windows_sys::Win32::Security::LABEL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::Security::SACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CREATE_NEW;
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;

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

/// Creates a directory with `security_descriptor` applied atomically.
pub fn create_directory_with_security(
    path: &Path,
    security_descriptor: &SecurityDescriptor,
) -> std::io::Result<()> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, error))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: the NUL-terminated path and security descriptor remain valid for
    // the duration of the documented Win32 call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Exclusively creates a regular file with `security_descriptor` applied atomically.
pub fn create_file_with_security(
    path: &Path,
    security_descriptor: &SecurityDescriptor,
) -> std::io::Result<std::fs::File> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|error| std::io::Error::new(ErrorKind::InvalidInput, error))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: arguments satisfy CreateFileW's contracts. Ownership of a valid
    // returned handle is transferred exactly once into `File`.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` is valid and uniquely owned after successful CreateFileW.
    Ok(unsafe { std::fs::File::from_raw_handle(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io;
    use std::io::Read;
    use std::io::Seek;
    use std::io::Write;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use test_with_tracing::test;
    use widestring::U16CStr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::GetFileSecurityW;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let mut id = [0; 16];
                getrandom::fill(&mut id).unwrap();
                let path = std::env::temp_dir().join(format!(
                    "pal-secure-create-{:032x}",
                    u128::from_ne_bytes(id)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("failed to create test directory: {error}"),
                }
            }
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn protected_descriptor() -> LocalSecurityDescriptor {
        "D:P(A;;FA;;;WD)".parse().unwrap()
    }

    fn dacl_sddl(path: &Path) -> io::Result<String> {
        let path = U16CString::from_os_str(path.as_os_str())
            .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error))?;
        let mut required = 0;
        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut required,
            );
        }
        if required == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = vec![0_u8; required as usize];
        if unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut sddl = null_mut();
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_ptr().cast_mut().cast(),
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut sddl,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sddl_string = unsafe { U16CStr::from_ptr_str(sddl).to_string_lossy() };
        unsafe {
            LocalFree(sddl.cast());
        }
        Ok(sddl_string)
    }

    #[test]
    fn applies_protected_dacl_to_files_and_directories() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let expected = descriptor.to_sddl().unwrap();
        let directory = root.join("directory");
        let file = root.join("file");

        create_directory_with_security(&directory, &descriptor).unwrap();
        create_file_with_security(&file, &descriptor).unwrap();

        assert_eq!(dacl_sddl(&directory).unwrap(), expected);
        assert_eq!(dacl_sddl(&file).unwrap(), expected);
    }

    #[test]
    fn existing_objects_are_not_modified() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let replacement: LocalSecurityDescriptor = "D:P(A;;FRFW;;;WD)".parse().unwrap();
        let directory = root.join("directory");
        let marker = directory.join("marker");
        let file = root.join("file");

        create_directory_with_security(&directory, &descriptor).unwrap();
        fs::write(&marker, b"directory contents").unwrap();
        fs::write(&file, b"existing file contents").unwrap();
        let directory_dacl = dacl_sddl(&directory).unwrap();
        let file_dacl = dacl_sddl(&file).unwrap();

        let directory_error = create_directory_with_security(&directory, &replacement).unwrap_err();
        let file_error = create_file_with_security(&file, &replacement).unwrap_err();

        assert_eq!(directory_error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(file_error.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&marker).unwrap(), b"directory contents");
        assert_eq!(fs::read(&file).unwrap(), b"existing file contents");
        assert_eq!(dacl_sddl(&directory).unwrap(), directory_dacl);
        assert_eq!(dacl_sddl(&file).unwrap(), file_dacl);
    }

    #[test]
    fn created_file_is_read_write_and_fully_shared() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let path = root.join("file");
        let renamed = root.join("renamed");
        let mut file = create_file_with_security(&path, &descriptor).unwrap();

        file.write_all(b"contents").unwrap();
        file.rewind().unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "contents");

        let reader = OpenOptions::new().read(true).open(&path).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        fs::rename(&path, &renamed).unwrap();
        drop((reader, writer));

        file.write_all(b" appended").unwrap();
        drop(file);
        assert_eq!(fs::read(&renamed).unwrap(), b"contents appended");
    }

    #[test]
    fn rejects_paths_with_interior_nul() {
        let root = TestDirectory::new();
        let descriptor = protected_descriptor();
        let path = root.join(OsString::from_wide(&[
            b'n' as u16,
            0,
            b'u' as u16,
            b'l' as u16,
        ]));

        assert_eq!(
            create_directory_with_security(&path, &descriptor)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            create_file_with_security(&path, &descriptor)
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }
}
