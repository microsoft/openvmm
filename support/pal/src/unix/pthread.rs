// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pthread, a basic Linux pthread (pthread_t) wrapper to support send
//! and sync on musl.

use std::io;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Pthread(libc::pthread_t);

// SAFETY: pthread_t is an opaque handle and is safe to share/send between
// threads. But it's a pointer type on musl so does not default to Send+Sync.
unsafe impl Send for Pthread {}
// SAFETY: see above comment.
unsafe impl Sync for Pthread {}

impl Pthread {
    /// Gets a Pthread object initialized with the caller thread.
    pub fn current() -> Self {
        // SAFETY: calling C API as documented, with no special requirements.
        Self(unsafe { libc::pthread_self() })
    }

    /// Sends a signal to Pthread's thread.
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        // SAFETY: calling as documented, with no special requirements.
        unsafe {
            if libc::pthread_kill(self.0, signal) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
