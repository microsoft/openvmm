// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]
#![warn(missing_docs)]
// UNSAFETY: Calls to libc functions to get rlimit info.
// TODO: replace unsafe with calls into the rlimit crate.
#![expect(unsafe_code)]

//! [`Inspect`] support for process rlimits.

use cfg_if::cfg_if;
use inspect::Inspect;
use inspect::Value;
use libc::rlimit;
use std::num::ParseIntError;
use thiserror::Error;

/// An implementation of [`Inspect`] that inspects, and allows updates of,
/// resource limits for a process.
pub struct InspectRlimit(Option<i32>);

impl InspectRlimit {
    /// Returns an inspector for the current process.
    pub fn new() -> Self {
        Self(None)
    }

    /// Returns an inspector for the process with ID `pid`.
    ///
    /// To use this, this process must have `CAP_SYS_RESOURCE` or have various
    /// user and group IDs that match the target process.
    pub fn for_pid(pid: i32) -> Self {
        Self(Some(pid))
    }
}

cfg_if! {
    if #[cfg(target_env = "gnu")] {
        type Resource = libc::__rlimit_resource_t;
    } else {
        type Resource = libc::c_int;
    }
}

impl Inspect for InspectRlimit {
    fn inspect(&self, req: inspect::Request<'_>) {
        let rlimits = {
            use libc::*;
            &[
                ("cpu", RLIMIT_CPU),
                ("fsize", RLIMIT_FSIZE),
                ("data", RLIMIT_DATA),
                ("stack", RLIMIT_STACK),
                ("core", RLIMIT_CORE),
                ("rss", RLIMIT_RSS),
                ("nproc", RLIMIT_NPROC),
                ("nofile", RLIMIT_NOFILE),
                ("memlock", RLIMIT_MEMLOCK),
                ("as", RLIMIT_AS),
                ("locks", RLIMIT_LOCKS),
                ("sigpending", RLIMIT_SIGPENDING),
                ("msgqueue", RLIMIT_MSGQUEUE),
                ("nice", RLIMIT_NICE),
                ("rtprio", RLIMIT_RTPRIO),
                ("rttime", RLIMIT_RTTIME),
            ]
        };

        let mut resp = req.respond();
        for &(name, resource) in rlimits {
            resp.field(
                name,
                RlimitResource {
                    pid: self.0,
                    resource,
                },
            );
        }
    }
}

struct RlimitResource {
    pid: Option<i32>,
    resource: Resource,
}

#[derive(Debug, Error)]
enum RlimitSetError {
    #[error("could not parse new value")]
    Parse(#[source] ParseIntError),
    #[error("failed to set rlimit")]
    Os(#[source] std::io::Error),
}

impl Inspect for RlimitResource {
    fn inspect(&self, req: inspect::Request<'_>) {
        let pid = self.pid.unwrap_or(0);
        let mut rlimit;
        // SAFETY: calling according to syscall documentation.
        let r = unsafe {
            rlimit = std::mem::zeroed();
            libc::prlimit(pid, self.resource, std::ptr::null(), &mut rlimit)
        };
        if r != 0 {
            req.value(std::io::Error::last_os_error().to_string().into());
            return;
        }

        let update = |rlimit: &mut rlimit, sel: fn(&mut rlimit) -> &mut u64, new: Option<&str>| {
            if let Some(new) = new {
                *sel(rlimit) = if new == "unlimited" {
                    !0
                } else {
                    new.parse().map_err(RlimitSetError::Parse)?
                };
                // SAFETY: calling according to syscall documentation.
                let r = unsafe { libc::prlimit(pid, self.resource, rlimit, std::ptr::null_mut()) };
                if r != 0 {
                    return Err(RlimitSetError::Os(std::io::Error::last_os_error()));
                }
            }
            let v = *sel(rlimit);
            let v = if v == !0 {
                Value::from("unlimited")
            } else {
                v.into()
            };
            Ok(v)
        };

        req.respond()
            .field_mut_with("current", |new| {
                update(&mut rlimit, |r| &mut r.rlim_cur, new)
            })
            .field_mut_with("max", |new| update(&mut rlimit, |r| &mut r.rlim_max, new));
    }
}
