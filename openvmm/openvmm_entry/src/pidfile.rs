// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pidfile management with crash-safe cleanup.
//!
//! On Unix, the pidfile is held open with an exclusive `flock` lock for
//! the lifetime of the [`Pidfile`] guard. If the process crashes, the
//! kernel releases the lock, and the stale file can be detected by
//! attempting to acquire the lock. The file is deleted on [`Drop`].
//!
//! On Windows, the pidfile is opened with `FILE_FLAG_DELETE_ON_CLOSE` and
//! shared read access (but not write or delete), so the OS removes it
//! when the handle closes — even after a crash.

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Error returned by [`Pidfile::new`].
#[derive(Debug, thiserror::Error)]
pub enum PidfileError {
    /// The pidfile is already locked by another process.
    #[error("pidfile is locked by another process (pid {existing_pid:?})")]
    Locked {
        /// The PID read from the existing pidfile, if available.
        existing_pid: Option<u32>,
    },
    /// An I/O error occurred.
    #[error("I/O error occurred while handling pidfile: {}", file.display())]
    Io {
        file: PathBuf,
        #[source]
        error: io::Error,
    },
}

/// An open pidfile that holds a lock (Unix) or delete-on-close handle
/// (Windows) for the lifetime of the guard.
#[derive(Debug)]
pub struct Pidfile {
    #[cfg(unix)]
    path: PathBuf,
    _file: std::fs::File,
}

impl Pidfile {
    /// Create a new pidfile at `path` containing the current process ID.
    ///
    /// Returns [`PidfileError::Locked`] if another process already holds
    /// the pidfile.
    pub fn new(path: &Path) -> Result<Self, PidfileError> {
        let pid = std::process::id();
        Self::create(path, pid)
    }

    #[cfg(unix)]
    fn create(path: &Path, pid: u32) -> Result<Self, PidfileError> {
        use std::io::Seek;
        use std::io::Write;

        // Open (or create) without truncating — we need to lock first so
        // we don't destroy a live process's pidfile.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| PidfileError::Io {
                file: path.to_owned(),
                error: e,
            })?;
        // Acquire an exclusive lock. If another instance holds the lock,
        // this returns an error immediately.
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                use std::io::Read;
                let mut contents = String::new();
                let _ = (&file).read_to_string(&mut contents);
                return Err(PidfileError::Locked {
                    existing_pid: contents.trim().parse().ok(),
                });
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(PidfileError::Io {
                    file: path.to_owned(),
                    error: e,
                });
            }
        }
        // Now that we hold the lock, truncate and write the new PID.
        (|| {
            file.set_len(0)?;
            (&file).seek(io::SeekFrom::Start(0))?;
            write!(&file, "{pid}\n")?;
            Ok(())
        })()
        .map_err(|e| PidfileError::Io {
            file: path.to_owned(),
            error: e,
        })?;
        Ok(Self {
            path: path.to_owned(),
            _file: file,
        })
    }

    #[cfg(windows)]
    fn create(path: &Path, pid: u32) -> Result<Self, PidfileError> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_DELETE_ON_CLOSE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        // Open with FILE_SHARE_READ (but not WRITE or DELETE) and
        // FILE_FLAG_DELETE_ON_CLOSE so the OS removes it on handle close.
        // The lack of FILE_SHARE_WRITE means a second create will fail
        // with ERROR_SHARING_VIOLATION, providing mutual exclusion.
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
        {
            Ok(f) => f,
            Err(e)
                if e.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32) =>
            {
                // Another process has the file open. Try to read the existing
                // PID for the error message.
                let existing_pid = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|s| s.trim().parse().ok());
                return Err(PidfileError::Locked { existing_pid });
            }
            Err(e) => {
                return Err(PidfileError::Io {
                    file: path.to_owned(),
                    error: e,
                });
            }
        };

        use std::io::Write;
        write!(&file, "{pid}\n").map_err(|e| PidfileError::Io {
            file: path.to_owned(),
            error: e,
        })?;
        Ok(Self { _file: file.into() })
    }
}

#[cfg(unix)]
impl Drop for Pidfile {
    fn drop(&mut self) {
        // Best-effort removal. The flock is released automatically when
        // the file is closed (which happens when `_file` is dropped after
        // this).
        let _ = std::fs::remove_file(&self.path);
    }
}

// On Windows, FILE_FLAG_DELETE_ON_CLOSE handles removal — no Drop needed.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfile_contains_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        let _pf = Pidfile::new(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, format!("{}\n", std::process::id()));
    }

    #[test]
    fn pidfile_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        {
            let _pf = Pidfile::new(&path).unwrap();
            assert!(path.exists());
        }
        assert!(!path.exists());
    }

    #[test]
    fn double_lock_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pid");
        let _pf = Pidfile::new(&path).unwrap();

        // A second pidfile at the same path should fail because the lock
        // is held.
        let err = Pidfile::new(&path).unwrap_err();
        match err {
            PidfileError::Locked { existing_pid } => {
                assert_eq!(existing_pid, Some(std::process::id()));
            }
            other => panic!("expected PidfileError::Locked, got: {other}"),
        }
    }
}
