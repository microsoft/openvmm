// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(unix)]
use super::unix as sys;
#[cfg(windows)]
use super::windows as sys;
use std::borrow::Cow;
use std::path::Path;
use std::path::PathBuf;

/// Extensions to `PathBuf` to convert Unix-style paths to a native path.
///
/// # Windows
///
/// Paths are converted to native by switching the separators from / to \\, and by escaping
/// characters that are not legal in NTFS file names (such as \\, :, and others) by mapping
/// them into a private unicode range (0xf000).
///
/// This is primarily useful for relative paths which will be passed to the methods of `LxVolume`.
/// While you can translate absolute paths, the result will have no drive letter.
///
/// # Unix
///
/// Paths are already native so no conversion is performed.
pub trait PathBufExt {
    /// Creates a `PathBuf` by converting a Unix-style path to its native representation.
    fn from_lx(path: impl AsRef<lx::LxStr>) -> lx::Result<PathBuf>;

    /// Extends `self` with `path`, first converting it from a Unix-style path to its native
    /// representation.
    fn push_lx(&mut self, path: impl AsRef<lx::LxStr>) -> lx::Result<()>;
}

impl PathBufExt for PathBuf {
    fn from_lx(path: impl AsRef<lx::LxStr>) -> lx::Result<PathBuf> {
        Ok(Self::from(Path::from_lx(&path)?))
    }

    fn push_lx(&mut self, path: impl AsRef<lx::LxStr>) -> lx::Result<()> {
        self.push(Path::from_lx(&path)?);
        Ok(())
    }
}

/// Extensions to `Path` to convert Unix-style paths to a native path, using the same rules as
/// `PathBuf`.
pub trait PathExt {
    /// Creates a `Path` by converting a Unix-style path to its native representation, avoiding
    /// allocation if unnecessary.
    ///
    /// # Windows
    ///
    /// This function does not allocate (returns a `Cow::Borrowed`) if the path contains no
    /// separators and no characters that need to be escaped. Otherwise, it allocates a `PathBuf`
    /// and returns a `Cow::Owned`.
    ///
    /// # Unix
    ///
    /// This function never allocates and always returns a `Cow::Borrowed`.
    fn from_lx(path: &(impl AsRef<lx::LxStr> + ?Sized)) -> lx::Result<Cow<'_, Self>>
    where
        Self: ToOwned;
}

impl PathExt for Path {
    fn from_lx(path: &(impl AsRef<lx::LxStr> + ?Sized)) -> lx::Result<Cow<'_, Self>> {
        sys::path::path_from_lx(path.as_ref().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn from_lx() {
        let path = Path::from_lx("test").unwrap();
        println!("{:?}", path);
        assert!(matches!(path, Cow::Borrowed(_)));
        assert_eq!(path.to_str(), Some("test"));

        let path = Path::from_lx("foo:bar").unwrap();
        println!("{:?}", path);
        if cfg!(windows) {
            assert!(matches!(path, Cow::Owned(_)));
            assert_eq!(path.to_str(), Some("foo\u{f03a}bar"));
        } else {
            assert!(matches!(path, Cow::Borrowed(_)));
            assert_eq!(path.to_str(), Some("foo:bar"));
        }

        let path = Path::from_lx("dir/file").unwrap();
        println!("{:?}", path);
        if cfg!(windows) {
            assert!(matches!(path, Cow::Owned(_)));
            assert_eq!(path.to_str(), Some("dir\\file"));
        } else {
            assert!(matches!(path, Cow::Borrowed(_)));
            assert_eq!(path.to_str(), Some("dir/file"));
        }

        let path = PathBuf::from_lx("dir/foo:bar").unwrap();
        if cfg!(windows) {
            assert_eq!(path.to_str(), Some("dir\\foo\u{f03a}bar"));
        } else {
            assert_eq!(path.to_str(), Some("dir/foo:bar"));
        }
    }

    #[test]
    fn push() {
        let mut path = PathBuf::new();
        path.push_lx("dir/subdir").unwrap();
        if cfg!(windows) {
            assert_eq!(path.to_str(), Some("dir\\subdir"));
        } else {
            assert_eq!(path.to_str(), Some("dir/subdir"));
        }

        path.push_lx("foo:bar").unwrap();
        if cfg!(windows) {
            assert_eq!(path.to_str(), Some("dir\\subdir\\foo\u{f03a}bar"));
        } else {
            assert_eq!(path.to_str(), Some("dir/subdir/foo:bar"));
        }
    }
}
