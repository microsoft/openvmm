// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolves and validates VMDK descriptor and extent paths.
//!
//! Checks reject unsupported path forms, symbolic links, and Windows reparse
//! points before canonicalization. Neither descriptor nor extent input may
//! contain `.` or `..` components.

use crate::OpenError;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(windows)]
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
#[cfg(windows)]
use std::path::Prefix;

// Win32 metadata bit indicating that a file or directory is a reparse point.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Validates a descriptor or extent path, resolves relative input against `base`,
/// and returns its canonical form. Absolute input does not use `base`.
pub(super) fn resolve_path(base: &Path, path: &Path) -> Result<PathBuf, OpenError> {
    // Reject empty input rather than treating it as the base directory.
    if path.as_os_str().is_empty() {
        return Err(OpenError::UnsafePath {
            path: path.to_owned(),
            reason: "path must not be empty",
        });
    }

    // Reject dot components before normalization can remove them.
    if path
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|component| matches!(component, "." | ".."))
    {
        return Err(OpenError::UnsafePath {
            path: path.to_owned(),
            reason: "'.' and '..' path components are not supported",
        });
    }

    // Reject unsupported Windows path forms before resolving relative paths.
    #[cfg(windows)]
    validate_windows_path(path).map_err(|reason| OpenError::UnsafePath {
        path: path.to_owned(),
        reason,
    })?;

    // Anchor relative input to the base; absolute input replaces it.
    // Make the result absolute without following filesystem links.
    let path = base.join(path);
    let path = std::path::absolute(&path).map_err(|source| OpenError::ResolvePath {
        path: path.clone(),
        source,
    })?;

    // Reject links before canonicalization can hide them.
    reject_link_components(&path)?;

    // Use a canonical path for relative-source resolution and source-file reuse.
    let path = fs_err::canonicalize(&path).map_err(|source| OpenError::ResolvePath {
        path: path.clone(),
        source,
    })?;

    Ok(path)
}

/// Accepts ordinary relative or drive-absolute paths, rejecting explicit
/// network paths, device paths, and alternate streams.
#[cfg(windows)]
fn validate_windows_path(path: &Path) -> Result<(), &'static str> {
    // Extract any drive, network, device, or verbatim prefix.
    let prefix = path.components().find_map(|component| match component {
        Component::Prefix(prefix) => Some(prefix.kind()),
        _ => None,
    });

    match prefix {
        // Accept `C:\file`, but reject drive-relative `C:file`.
        Some(Prefix::Disk(_)) => {
            if !path.has_root() {
                return Err("drive-relative paths are not supported");
            }
        }
        // UNC paths access a network share rather than a local drive.
        Some(Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _)) => {
            return Err("network paths are not supported");
        }
        // Reject paths that can address devices or bypass Win32 normalization.
        Some(Prefix::DeviceNS(_) | Prefix::Verbatim(_) | Prefix::VerbatimDisk(_)) => {
            return Err("device and verbatim paths are not supported");
        }
        // `\file` has a root but inherits the current drive.
        None if path.has_root() => return Err("root-relative paths are not supported"),
        // No prefix or root means an ordinary relative path.
        None => {}
    }

    // Prefix and root components were handled above. Check only file and
    // directory names here.
    for path_component in path.components() {
        let Component::Normal(name) = path_component else {
            continue;
        };

        let name = name.to_string_lossy();
        // A colon within a component selects an alternate data stream.
        if name.contains(':') {
            return Err("alternate data streams are not supported");
        }
    }

    Ok(())
}

/// Checks an absolute path from root to file so a linked directory is rejected
/// before any entries beneath it are inspected.
fn reject_link_components(path: &Path) -> Result<(), OpenError> {
    let ancestors = path.ancestors().collect::<Vec<_>>();
    for path in ancestors.into_iter().rev() {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|source| OpenError::ResolvePath {
                path: path.to_owned(),
                source,
            })?;

        if is_link_or_reparse_point(&metadata) {
            return Err(OpenError::UnsafePath {
                path: path.to_owned(),
                reason: "symbolic links and reparse points are not supported",
            });
        }
    }

    Ok(())
}

/// Returns whether metadata describes a link or Windows reparse point.
#[cfg(unix)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Returns whether metadata describes a link or Windows reparse point.
#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    // Verifies that relative input uses the supplied base, absolute input does
    // not, and periods within ordinary names remain valid.
    #[test]
    fn resolves_relative_and_absolute_paths() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["source.bin", ".source.bin", "source..bin"] {
            let path = directory.path().join(name);
            std::fs::write(&path, [0; 512]).unwrap();

            assert_eq!(
                resolve_path(directory.path(), Path::new(name)).unwrap(),
                fs_err::canonicalize(&path).unwrap()
            );
        }

        let path = directory.path().join("source.bin");
        assert_eq!(
            resolve_path(Path::new("unused"), &path).unwrap(),
            fs_err::canonicalize(path).unwrap()
        );
    }

    // Verifies that empty input is not interpreted as the supplied base path.
    #[test]
    fn rejects_empty_path() {
        assert!(matches!(
            resolve_path(Path::new("."), Path::new("")),
            Err(OpenError::UnsafePath { path, reason })
                if path.as_os_str().is_empty() && reason == "path must not be empty"
        ));
    }

    // Verifies that current-directory and parent-directory components are
    // rejected with either Unix or Windows separators.
    #[test]
    fn rejects_dot_components() {
        for path in [
            ".",
            "..",
            "./disk",
            r".\disk",
            "../disk",
            r"..\disk",
            "data/./disk",
            r"data\.\disk",
            "data/../disk",
            r"data\..\disk",
            "data/..\\disk",
            "data/.",
            r"data\..",
        ] {
            assert!(
                matches!(
                    resolve_path(Path::new("."), Path::new(path)),
                    Err(OpenError::UnsafePath {
                        reason: "'.' and '..' path components are not supported",
                        ..
                    })
                ),
                "path unexpectedly accepted: {path}"
            );
        }
    }

    // Verifies the accepted local forms and each rejected Windows path class.
    #[cfg(windows)]
    #[test]
    fn validates_windows_path_forms() {
        for path in [
            r"directory\disk.vmdk",
            r"C:\directory\disk.vmdk",
            r"directory.\disk.vmdk",
            "disk.vmdk ",
        ] {
            assert_eq!(validate_windows_path(Path::new(path)), Ok(()), "{path}");
        }

        for (path, reason) in [
            (r"C:disk.vmdk", "drive-relative paths are not supported"),
            (
                r"\\server\share\disk.vmdk",
                "network paths are not supported",
            ),
            (
                r"\\?\UNC\server\share\disk.vmdk",
                "network paths are not supported",
            ),
            (
                r"\\.\PhysicalDrive0",
                "device and verbatim paths are not supported",
            ),
            (
                r"\\?\C:\disk.vmdk",
                "device and verbatim paths are not supported",
            ),
            (r"\disk.vmdk", "root-relative paths are not supported"),
            (
                "disk.vmdk:stream",
                "alternate data streams are not supported",
            ),
        ] {
            assert_eq!(
                validate_windows_path(Path::new(path)),
                Err(reason),
                "{path}"
            );
        }
    }
}
