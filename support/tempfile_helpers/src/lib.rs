// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for using the tempfile crate more effectively.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;
use std::path::PathBuf;

/// Runs the provided closure with a randomly-generated temporary file path.
/// If the closure returns an error indicating that the path is already in use,
/// a new path is generated and the closure is run again. This is intended to be
/// used for the creation of file-like resources, such as Unix domain sockets.
///
/// It is up to the closure to ensure that the file does not exist and that such
/// a check is atomic. Otherwise, a time-of-check to time-of-use bug could be
/// introduced. See [`tempfile::Builder::make`] for more information.
///
/// Note that the returned file will not have any automatic cleanup behavior.
/// While it will be located in a temporary directory, it will not be deleted
/// on drop. If you need automatic cleanup consider using the `tempfile` crate
/// directly.
pub fn with_temp_path<T>(f: impl Fn(&Path) -> std::io::Result<T>) -> std::io::Result<(T, PathBuf)> {
    tempfile::Builder::new()
        .make(f)
        .and_then(|tfile| tfile.keep().map_err(|e| e.error))
}
