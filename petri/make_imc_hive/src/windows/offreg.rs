// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline registry DLL wrappers.

// UNSAFETY: needed for the FFI bindings.
#![expect(unsafe_code)]

use std::ffi::OsStr;
use std::ops::Deref;
use std::path::Path;
use std::ptr::null;
use std::ptr::null_mut;
use widestring::U16CString;
use widestring::U16String;
use windows_sys::Wdk::System::OfflineRegistry::ORCloseHive;
use windows_sys::Wdk::System::OfflineRegistry::ORCloseKey;
use windows_sys::Wdk::System::OfflineRegistry::ORCreateHive;
use windows_sys::Wdk::System::OfflineRegistry::ORCreateKey;
use windows_sys::Wdk::System::OfflineRegistry::ORHKEY;
use windows_sys::Wdk::System::OfflineRegistry::ORSaveHive;
use windows_sys::Wdk::System::OfflineRegistry::ORSetValue;
use windows_sys::Win32::System::Registry::REG_DWORD;
use windows_sys::Win32::System::Registry::REG_EXPAND_SZ;
use windows_sys::Win32::System::Registry::REG_MULTI_SZ;
use windows_sys::Win32::System::Registry::REG_SZ;

pub struct Hive(Key);

impl Hive {
    pub fn create() -> std::io::Result<Self> {
        let mut key = null_mut();
        // SAFETY: calling as documented
        unsafe {
            chk(ORCreateHive(&mut key))?;
        }
        Ok(Self(Key(key)))
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let path16 = wide(path)?;

        // SAFETY: calling as documented with owned key and null-terminated
        // path.
        unsafe {
            chk(ORSaveHive((self.0).0, path16.as_ptr(), 6, 1))?;
        }
        Ok(())
    }
}

impl AsRef<Key> for Hive {
    fn as_ref(&self) -> &Key {
        &self.0
    }
}

impl Deref for Hive {
    type Target = Key;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Hive {
    fn drop(&mut self) {
        // SAFETY: calling as documented with owned hive key.
        unsafe {
            let _ = ORCloseHive((self.0).0);
        }
    }
}

pub struct OwnedKey(Key);

impl AsRef<Key> for OwnedKey {
    fn as_ref(&self) -> &Key {
        &self.0
    }
}

impl Deref for OwnedKey {
    type Target = Key;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for OwnedKey {
    fn drop(&mut self) {
        // SAFETY: calling as documented with owned key.
        unsafe {
            let _ = ORCloseKey((self.0).0);
        }
    }
}

pub struct Key(ORHKEY);

impl Key {
    pub fn create_key(&self, name: &str) -> anyhow::Result<OwnedKey> {
        let mut new_key = null_mut();
        let name16 = wide(name)?;
        // SAFETY: calling as documented with owned key and null-terminated
        // path.
        unsafe {
            chk(ORCreateKey(
                self.0,
                name16.as_ptr(),
                null(),
                0,
                null_mut(),
                &mut new_key,
                null_mut(),
            ))?;
        }
        Ok(OwnedKey(Key(new_key)))
    }

    pub fn set_dword(&self, name: &str, dword: u32) -> std::io::Result<()> {
        let name16 = wide(name)?;
        // SAFETY: calling as documented with owned key and null-terminated
        // name.
        unsafe {
            chk(ORSetValue(
                self.0,
                name16.as_ptr(),
                REG_DWORD,
                dword.to_ne_bytes().as_ptr(),
                4,
            ))?;
        }
        Ok(())
    }

    pub fn set_sz(&self, name: &str, value: &str) -> std::io::Result<()> {
        let name16 = wide(name)?;
        let value16 = wide(value)?;
        // SAFETY: calling as documented with owned key and null-terminated
        // name and value.
        unsafe {
            chk(ORSetValue(
                self.0,
                name16.as_ptr(),
                REG_SZ,
                value16.as_ptr().cast(),
                value16.as_slice_with_nul().len() as u32 * 2,
            ))?;
        }
        Ok(())
    }

    pub fn set_expand_sz(&self, name: &str, value: &str) -> std::io::Result<()> {
        let name16 = wide(name)?;
        let value16 = wide(value)?;
        // SAFETY: calling as documented with owned key and null-terminated
        // name and value.
        unsafe {
            chk(ORSetValue(
                self.0,
                name16.as_ptr(),
                REG_EXPAND_SZ,
                value16.as_ptr().cast(),
                value16.as_slice_with_nul().len() as u32 * 2,
            ))?;
        }
        Ok(())
    }

    pub fn set_multi_sz<'a>(
        &self,
        name: &str,
        value: impl IntoIterator<Item = &'a str>,
    ) -> std::io::Result<()> {
        let name16 = wide(name)?;
        let mut value16 = U16String::new();
        for s in value {
            value16.push_slice(wide(s)?.as_slice_with_nul());
        }
        value16.push_char('\0');
        // SAFETY: calling as documented with owned key and null-terminated
        // name and value.
        unsafe {
            chk(ORSetValue(
                self.0,
                name16.as_ptr(),
                REG_MULTI_SZ,
                value16.as_ptr().cast(),
                value16.len() as u32 * 2,
            ))?;
        }
        Ok(())
    }
}

fn wide(s: impl AsRef<OsStr>) -> std::io::Result<U16CString> {
    U16CString::from_os_str(s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

fn chk(err: u32) -> std::io::Result<()> {
    if err != 0 {
        return Err(std::io::Error::from_raw_os_error(err as i32));
    }
    Ok(())
}
