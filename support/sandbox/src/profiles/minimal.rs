// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The minimal base profile.

use crate::Builder;
use crate::Network;
use crate::Profile;
use crate::Syscalls;

/// A conservative starting profile that a worker widens for its own needs.
///
/// It grants read + execute on `/bin` and `/lib` (so a dynamically linked
/// binary can start), denies all networking, and installs the seccomp
/// dangerous-syscall baseline ([`Syscalls::Deny`] with no extras) — blocking
/// namespace-escape and kernel-CVE syscalls such as `mount`, `unshare`,
/// `pivot_root`, and `bpf`. Because every profile derives from
/// [`Profile::deny_all`], it also drops all capabilities. A worker typically
/// starts from here and chains further grants before building:
///
/// ```
/// use sandbox::{Network, profiles};
///
/// let profile = profiles::minimal()
///     .name("my_worker")
///     .read_exec("/usr/lib")
///     .network(Network::Loopback)
///     .build();
/// ```
pub fn minimal() -> Builder {
    Profile::deny_all()
        .name("minimal")
        .read_exec("/bin")
        .read_exec("/lib")
        .network(Network::None)
        .syscalls(Syscalls::deny(std::iter::empty::<&str>()))
}
