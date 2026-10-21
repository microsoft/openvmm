// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The sandbox policy vocabulary: [`Profile`] and its widening [`Builder`],
//! plus the additive-only [`Restrictions`] ratchet consumed by
//! [`tighten`](crate::tighten).
//!
//! # Default-deny by construction
//!
//! A policy is always built from [`Profile::deny_all`], which denies every
//! ambient authority. The [`Builder`] can only *grant* — no method removes a
//! denial — so a profile is only ever as permissive as its most permissive
//! call. A worker whose profile adds nothing can reach nothing but the
//! handles the control process explicitly kept inheritable.
//!
//! # Widening vs. narrowing
//!
//! [`Builder`] widens a default-deny base at policy-authoring time.
//! [`Restrictions`] is the complementary, additive-only *narrowing* spec: it
//! can only ever subtract from an already-applied profile, so a non-monotonic
//! ratchet is unrepresentable rather than a runtime error.
//!
//! # Platform neutrality
//!
//! The vocabulary is intent-level and platform-neutral. No third-party
//! sandbox type (`landlock::*`, `seccompiler::*`, `caps::*`, `windows_sys::*`)
//! ever crosses this surface. Where a dimension has no analogue on a platform,
//! the backend treats the grant as a no-op rather than an error.

use std::path::Path;
use std::path::PathBuf;

/// An opaque, immutable sandbox policy.
///
/// Built once from [`Profile::deny_all`] in the crate that owns the worker,
/// and linked into whichever side applies it (the worker on Linux; the
/// control process *and* the worker on Windows). A `Profile` is **not** a
/// wire type — policy is linked, not sent.
#[derive(Debug, Clone)]
pub struct Profile {
    pub(crate) inner: ProfileInner,
}

impl Profile {
    /// The base every profile builds on: deny all ambient authority.
    ///
    /// A profile that adds nothing is a worker that can reach nothing but its
    /// explicitly kept-inheritable handles.
    pub fn deny_all() -> Builder {
        Builder {
            inner: ProfileInner::default(),
        }
    }

    /// Reopen a finished profile as a [`Builder`] to derive a new, wider
    /// profile from it.
    ///
    /// The builder starts from a clone of this profile's grants, so the
    /// original is left intact and any number of variants can be derived from a
    /// single base. Because a [`Builder`] only ever widens, the accumulating
    /// dimensions (filesystem and capabilities) can only grow, syscall denials
    /// can only be added, and the scalar dimensions (`name` and `network`) are
    /// replaced by a later call.
    ///
    /// Prefer exposing a reusable base as a `fn() -> Builder` — see
    /// [`profiles`](crate::profiles) — and reach for `edit` only when you
    /// already hold a built [`Profile`], e.g. one handed to you by another
    /// crate.
    ///
    /// ```
    /// use sandbox::Profile;
    ///
    /// let base = Profile::deny_all().name("base").read("/usr/lib").build();
    /// let worker = base.edit().name("worker").read_write("/run/worker").build();
    /// assert_eq!(worker.name(), "worker");
    /// ```
    pub fn edit(&self) -> Builder {
        Builder {
            inner: self.inner.clone(),
        }
    }

    /// The human-readable name of the profile, used only in diagnostics.
    pub fn name(&self) -> &'static str {
        self.inner.name
    }
}

/// Widening-only policy builder.
///
/// Every method *grants*; none can relax a denial. Because the base is
/// [`Profile::deny_all`] and nothing subtracts from it, the default-deny
/// invariant holds by construction. Post-`apply` *narrowing* is a separate
/// concern with its own additive-only type — see [`Restrictions`].
#[derive(Debug, Clone)]
pub struct Builder {
    inner: ProfileInner,
}

impl Builder {
    /// Attach a human-readable name used in tracing and error messages. Not
    /// consulted by the sandbox itself.
    pub fn name(mut self, name: &'static str) -> Self {
        self.inner.name = name;
        self
    }

    /// Grant read (and execute) access to `path`.
    ///
    /// Read grants include execute so that shared libraries and interpreters
    /// under system directories (e.g. `/lib`, `/usr`) remain loadable. Use
    /// [`Builder::read_write`] for data the worker must modify; write grants
    /// are mounted non-executable.
    pub fn read(mut self, path: impl AsRef<Path>) -> Self {
        self.push_fs(path.as_ref().to_path_buf(), Access::ReadExec);
        self
    }

    /// Grant read and write access to `path`. The path is mounted
    /// non-executable, so a worker cannot execute data it can write.
    pub fn read_write(mut self, path: impl AsRef<Path>) -> Self {
        self.push_fs(path.as_ref().to_path_buf(), Access::ReadWrite);
        self
    }

    /// Grant network reachability up to `scope`.
    ///
    /// The default (from [`Profile::deny_all`]) is [`Network::None`] — an
    /// empty network namespace with no peer. [`Network::Loopback`] enables
    /// loopback inside that namespace, while [`Network::Unrestricted`] keeps
    /// the worker in the caller's network namespace.
    pub fn network(mut self, scope: Network) -> Self {
        self.inner.network = scope;
        self
    }

    /// Add to the worker's syscall-filtering policy.
    ///
    /// Syscall filtering is opt-in per worker class: the default is
    /// [`Syscalls::Unfiltered`] (no seccomp filter). [`Syscalls::Deny`]
    /// installs a seccomp filter that blocks the crate's built-in set of
    /// dangerous, namespace-escape, and historically CVE-prone syscalls, plus
    /// any additional syscalls named in the list.
    ///
    /// Successive calls compose monotonically: deny lists are unioned while
    /// preserving first-seen order, and [`Syscalls::Unfiltered`] is the
    /// identity. Once a base profile opts into filtering, a derived profile
    /// cannot remove that filter or any of its denials.
    pub fn syscalls(mut self, policy: Syscalls) -> Self {
        if let Syscalls::Deny(additional) = policy {
            match &mut self.inner.syscalls {
                current @ Syscalls::Unfiltered => *current = Syscalls::Deny(additional),
                Syscalls::Deny(existing) => {
                    for name in additional {
                        if !existing.contains(&name) {
                            existing.push(name);
                        }
                    }
                }
            }
        }
        self
    }

    /// Grant a named platform capability (e.g. a Windows LPAC capability).
    ///
    /// Capabilities with no analogue on the current platform are a no-op.
    pub fn capability(mut self, cap: Capability) -> Self {
        self.inner.capabilities.push(cap);
        self
    }

    /// Finalize the policy.
    pub fn build(self) -> Profile {
        Profile { inner: self.inner }
    }

    fn push_fs(&mut self, path: PathBuf, access: Access) {
        self.inner.fs.push(FsGrant { path, access });
    }
}

/// Network reachability granted to the worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Network {
    /// No network at all — an empty network namespace with no peer.
    #[default]
    None,
    /// Loopback only (`127.0.0.0/8`, `::1`) — a network namespace with the
    /// loopback interface brought up and nothing else.
    Loopback,
    /// Preserve the caller's network namespace and ambient connectivity.
    Unrestricted,
}

/// Syscall-filtering policy (Linux; opt-in per worker class).
///
/// The crate uses a **denylist** model rather than an allowlist: a worker's
/// full set of *needed* syscalls cannot be reliably enumerated, so the filter
/// instead blocks a curated set of syscalls that a sandboxed worker never
/// legitimately needs and that are known namespace-escape or kernel-CVE
/// vectors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Syscalls {
    /// Install no seccomp filter. The worker retains the ambient syscall
    /// surface; confinement rests on namespaces, credentials, and
    /// `no_new_privs`.
    #[default]
    Unfiltered,
    /// Install a seccomp filter that always blocks the crate's built-in set of
    /// dangerous syscalls — mount/namespace manipulation, module loading,
    /// `ptrace`, `kexec`, keyring access, and other escape or CVE vectors, plus
    /// argument-level guards on `ioctl`, `clone`, `prctl`, and `socket` — and
    /// additionally blocks every syscall named here. An empty list installs
    /// just the built-in baseline. Unknown names are a hard error at apply
    /// time.
    Deny(Vec<String>),
}

impl Syscalls {
    /// Deny the supplied syscall names in addition to the built-in baseline.
    pub fn deny<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Deny(names.into_iter().map(Into::into).collect())
    }
}

/// An opaque, named platform capability, constructed from crate-provided
/// constants. The consumer never sees the underlying SID or grant.
///
/// Capabilities are Windows LPAC concepts; on other platforms they are a
/// no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability(pub(crate) CapabilityKind);

impl Capability {
    /// The `lpacCom` capability — COM activation under LPAC.
    pub const LPAC_COM: Capability = Capability(CapabilityKind::LpacCom);
    /// The `lpacCryptoServices` capability — cryptographic service access
    /// under LPAC.
    pub const LPAC_CRYPTO_SERVICES: Capability = Capability(CapabilityKind::LpacCryptoServices);
    /// The `registryRead` capability — read access to policy under the
    /// registry.
    pub const REGISTRY_READ: Capability = Capability(CapabilityKind::RegistryRead);

    /// The stable capability moniker, for diagnostics and for a Windows
    /// consumer to resolve into a capability SID.
    pub fn moniker(self) -> &'static str {
        match self.0 {
            CapabilityKind::LpacCom => "lpacCom",
            CapabilityKind::LpacCryptoServices => "lpacCryptoServices",
            CapabilityKind::RegistryRead => "registryRead",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityKind {
    LpacCom,
    LpacCryptoServices,
    RegistryRead,
}

/// A narrow, additive-only ratchet consumed by [`tighten`](crate::tighten).
///
/// It can express only the primitives that are safe to stack onto an
/// already-applied sandbox: additional syscall denials and a further-restricted
/// path. The one-shot, authority-consuming primitives — namespaces,
/// credentials, and mounts — are absent from this type, so a non-monotonic
/// ratchet is *unrepresentable*.
#[derive(Debug, Clone)]
pub struct Restrictions {
    pub(crate) inner: RestrictionsInner,
}

impl Restrictions {
    /// Start from "no additional restriction"; every method on the returned
    /// builder can only narrow further.
    pub fn none() -> RestrictionsBuilder {
        RestrictionsBuilder {
            inner: RestrictionsInner::default(),
        }
    }
}

/// Narrowing-only ratchet builder. There is deliberately no "unfiltered"
/// escape here — a ratchet only ever subtracts.
#[derive(Debug, Clone)]
pub struct RestrictionsBuilder {
    inner: RestrictionsInner,
}

impl RestrictionsBuilder {
    /// Block additional syscalls beyond those the applied profile already
    /// denies, installed as an additional stacked seccomp filter. The kernel
    /// takes the most restrictive verdict across every installed filter, so a
    /// stacked denylist can only ever narrow the surface further.
    pub fn syscalls(mut self, deny: &'static [&'static str]) -> Self {
        self.inner.syscalls = Some(deny);
        self
    }

    /// Enforce an additional restriction that *removes* access to a path the
    /// applied profile previously permitted. It can never add access.
    pub fn revoke_path(mut self, path: impl AsRef<Path>) -> Self {
        self.inner.revoke_paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Finalize the ratchet.
    pub fn build(self) -> Restrictions {
        Restrictions { inner: self.inner }
    }
}

// ---------------------------------------------------------------------------
// Internal representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ProfileInner {
    pub(crate) name: &'static str,
    pub(crate) fs: Vec<FsGrant>,
    pub(crate) network: Network,
    pub(crate) syscalls: Syscalls,
    pub(crate) capabilities: Vec<Capability>,
}

impl Default for ProfileInner {
    fn default() -> Self {
        Self {
            name: "unnamed",
            fs: Vec::new(),
            network: Network::None,
            syscalls: Syscalls::Unfiltered,
            capabilities: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FsGrant {
    pub(crate) path: PathBuf,
    pub(crate) access: Access,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RestrictionsInner {
    pub(crate) syscalls: Option<&'static [&'static str]>,
    pub(crate) revoke_paths: Vec<PathBuf>,
}

/// Access mode for a filesystem grant. Internal — the public builder exposes
/// only [`Builder::read`] and [`Builder::read_write`], mapped to the mount
/// flags and Landlock rights the backend needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Read + execute. No write.
    ReadExec,
    /// Read + write. No execute.
    ReadWrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_all_defaults_to_no_network_no_seccomp() {
        let profile = Profile::deny_all().build();
        assert_eq!(profile.inner.network, Network::None);
        assert!(matches!(profile.inner.syscalls, Syscalls::Unfiltered));
        assert!(profile.inner.fs.is_empty());
    }

    #[test]
    fn read_grants_are_read_exec() {
        let profile = Profile::deny_all().read("/lib").build();
        assert_eq!(profile.inner.fs.len(), 1);
        assert_eq!(profile.inner.fs[0].access, Access::ReadExec);
    }

    #[test]
    fn network_grant_is_stored() {
        let profile = Profile::deny_all().network(Network::Loopback).build();
        assert_eq!(profile.inner.network, Network::Loopback);

        let profile = Profile::deny_all().network(Network::Unrestricted).build();
        assert_eq!(profile.inner.network, Network::Unrestricted);
    }

    #[test]
    fn syscalls_deny_is_stored() {
        let profile = Profile::deny_all()
            .syscalls(Syscalls::deny(["socket"]))
            .build();
        assert_eq!(
            profile.inner.syscalls,
            Syscalls::Deny(vec!["socket".to_string()])
        );
    }

    #[test]
    fn syscall_denials_are_additive_and_deduplicated() {
        let profile = Profile::deny_all()
            .syscalls(Syscalls::deny(["socket", "kill"]))
            .syscalls(Syscalls::deny(["kill", "tkill"]))
            .build();

        assert_eq!(
            profile.inner.syscalls,
            Syscalls::Deny(vec![
                "socket".to_string(),
                "kill".to_string(),
                "tkill".to_string(),
            ])
        );
    }

    #[test]
    fn unfiltered_does_not_remove_existing_syscall_denials() {
        let profile = Profile::deny_all()
            .syscalls(Syscalls::deny(["socket"]))
            .syscalls(Syscalls::Unfiltered)
            .build();

        assert_eq!(
            profile.inner.syscalls,
            Syscalls::Deny(vec!["socket".to_string()])
        );
    }

    #[test]
    fn edited_profile_adds_to_existing_syscall_denials() {
        let base = Profile::deny_all()
            .syscalls(Syscalls::deny(["socket"]))
            .build();
        let derived = base.edit().syscalls(Syscalls::deny(["kill"])).build();

        assert_eq!(
            derived.inner.syscalls,
            Syscalls::Deny(vec!["socket".to_string(), "kill".to_string()])
        );
    }
}
