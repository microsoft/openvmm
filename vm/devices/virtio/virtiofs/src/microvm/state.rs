// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM virtio-fs relative path validation.

use std::path::Path;

pub(crate) fn relative_path_encoded_len(path: &Path) -> lx::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str()
            .encode_wide()
            .count()
            .checked_mul(size_of::<u16>())
            .ok_or(lx::Error::E2BIG)
    }
}

pub(crate) fn validate_relative_path(path: &Path, strict: bool) -> lx::Result<()> {
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(lx::Error::EINVAL);
        };
        if component.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let component = component.as_bytes();
            if component.contains(&b'\0')
                || (strict && (component.contains(&b'\\') || component.contains(&b':')))
            {
                return Err(lx::Error::EINVAL);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;

            let units = component.encode_wide().collect::<Vec<_>>();
            if units.contains(&0)
                || (strict && (units.contains(&(b'\\' as u16)) || units.contains(&(b':' as u16))))
            {
                return Err(lx::Error::EINVAL);
            }
        }
    }
    Ok(())
}
