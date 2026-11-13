// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Control-side preparation: the [`prepare`](crate::prepare) stage and the
//! plain-data [`SandboxProcessConfig`] it returns.
//!
//! # Why data, not a builder mutation
//!
//! Rather than mutating a platform process builder, this crate computes a
//! [`SandboxProcessConfig`] and hands it back to the caller, which passes it
//! intact to the platform process builder. That keeps `support/sandbox` free
//! of any dependency on `mesh` or `pal`: the sandbox decides *what* the
//! child's launch environment must be; PAL decides *how* to realize it.
//!
//! # Handle hygiene is the caller's job
//!
//! There is no grant envelope and no in-crate FD sweep. [`SandboxProcessConfig`]
//! reports exactly which handles must remain inheritable in
//! [`SandboxProcessConfig::inherit_handles`]; the platform process builder is
//! responsible for retaining those handles and its configured standard I/O,
//! while closing every other descriptor in the pre-exec child context.

use crate::Error;
use crate::profile::Profile;

/// A child-visible, platform-native handle value: a target file-descriptor
/// number on Unix, or an inherited `HANDLE` value on Windows.
///
/// Passing a `RawHandle` records intent to retain that child-visible value; it
/// does not transfer ownership of any parent-side source handle. The owner
/// must keep the source handle alive until the child exists.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct RawHandle(pub u64);

/// Per-spawn identity for the worker.
///
/// On Unix, the control process's host UID/GID are retained; custom host
/// identities are not supported. On Linux, namespace UID/GID 0 map to the
/// spawning process's effective UID/GID.
///
/// On Windows, the AppContainer moniker is reflected in the Windows launch
/// preparation. The AppContainer field is only available on Windows.
///
/// Custom host UID/GID inputs cannot be supplied:
///
/// ```compile_fail
/// let identity = sandbox::Identity {
///     uid: Some(0),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// The Windows AppContainer moniker the worker should run under.
    #[cfg(windows)]
    pub app_container: Option<String>,
}

/// The result of [`prepare`](crate::prepare): everything the platform process
/// builder needs to apply before the spawn.
///
/// It is deliberately inert plain data — no file descriptors are touched, no
/// process APIs are called. The caller passes the value intact to the platform
/// process builder.
#[derive(Debug, Clone, Default)]
pub struct SandboxProcessConfig {
    /// Linux namespace creation and current-user mapping requirements.
    #[cfg(target_os = "linux")]
    pub linux: LinuxPreparation,
    /// Windows LPAC construction data. The caller turns this into an
    /// AppContainer SID, a capability SID array, and a `STARTUPINFOEX`
    /// attribute list.
    #[cfg(windows)]
    pub windows: WindowsPreparation,
    /// The handles that must remain inheritable across the spawn. Every
    /// descriptor *not* listed here, or configured as standard I/O by the
    /// process builder, must be closed before the child is reached.
    pub inherit_handles: Vec<RawHandle>,
}

/// Linux-specific launch requirements for a [`SandboxProcessConfig`].
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub struct LinuxPreparation {
    /// Linux `CLONE_NEW*` flags to pass to the operation that creates the
    /// child. Zero when the debug sandbox escape hatch is active.
    ///
    /// Namespace creation must happen as part of cloning the child so the
    /// worker starts inside its namespaces. The platform process builder also
    /// establishes the child user namespace's uid/gid mappings before exec.
    /// Confined Linux launches require user, mount, and IPC namespaces, plus
    /// a network namespace unless the profile allows unrestricted networking.
    pub clone_flags: u64,
    /// Map user and group ID 0 in the Linux user namespace to the spawning
    /// process's effective user and group IDs.
    pub map_current_user: bool,
}

/// The Windows-specific half of a [`SandboxProcessConfig`]: the data a caller
/// needs to build an LPAC token and launch the worker into it.
///
/// This crate does not link `windows-sys` or call any Win32 API; it only
/// describes *what* the LPAC configuration must be. The caller performs
/// `DeriveAppContainerSid`, builds the `SID_AND_ATTRIBUTES[]` from the
/// capability monikers, and sets the mitigation flags below on the
/// `STARTUPINFOEX` / `SetProcessMitigationPolicy` surface.
#[cfg(windows)]
#[derive(Debug, Clone, Default)]
pub struct WindowsPreparation {
    /// The AppContainer moniker to derive the container SID from. `None` means
    /// no explicit container (inherit — dev path).
    pub app_container: Option<String>,
    /// The LPAC capability monikers to grant (resolved to capability SIDs by
    /// the caller). Empty by default: LPAC grants nothing it is not told to.
    pub capabilities: Vec<&'static str>,
    /// Opt out of `ALL_APPLICATION_PACKAGES` — the bit that makes this LPAC
    /// rather than a plain AppContainer. Always `true`; a false value would
    /// silently weaken the sandbox.
    pub lpac_opt_out: bool,
    /// Apply `ProcessImageLoadPolicy(NoRemoteImages)` — deny loading images
    /// from remote sources.
    pub no_remote_images: bool,
    /// Apply `ProcessSystemCallDisablePolicy` (Win32k lockdown) — no worker
    /// needs the GUI subsystem.
    pub win32k_lockdown: bool,
    /// Apply `ProcessChildProcessPolicy` — forbid the worker from spawning
    /// children.
    pub disallow_child_processes: bool,
}

/// STAGE 1 — control process, immediately before the spawn.
///
/// Computes the launch-time configuration for a confined worker and returns it
/// as plain data. It does **not** mutate any process builder, touch any file
/// descriptor, or call any process-creation API — the caller passes the
/// returned [`SandboxProcessConfig`] to its platform process builder.
///
/// * `profile` — the worker's linked policy. On Linux only its launch-relevant
///   bits are consulted here (the worker self-applies the rest in
///   [`apply`](crate::apply)); on Windows its entire LPAC portion is reflected
///   into the Windows launch preparation.
/// * `identity` — the per-spawn identity. On Windows, its AppContainer
///   configuration is reflected in the Windows launch preparation. Unix
///   launches retain the spawning process's host UID/GID.
/// * `handles` — the handles to keep inheritable. They are echoed into
///   [`SandboxProcessConfig::inherit_handles`]; the platform process builder
///   closes every other descriptor except its configured standard I/O.
///
/// # Example
///
/// ```
/// use sandbox::{Identity, Profile, RawHandle};
///
/// let profile = Profile::deny_all().build();
/// let handles = [RawHandle(3)];
/// let preparation = sandbox::prepare(&profile, &Identity::default(), &handles)?;
/// # Ok::<(), sandbox::Error>(())
/// ```
///
/// This only records the child-visible descriptor number. The process builder
/// must map an owned source descriptor to that number and apply the preparation
/// before spawning; the worker must still call [`apply`](crate::apply).
pub fn prepare(
    profile: &Profile,
    identity: &Identity,
    handles: &[RawHandle],
) -> Result<SandboxProcessConfig, Error> {
    #[cfg(not(windows))]
    let _ = identity;
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = profile;

    Ok(SandboxProcessConfig {
        #[cfg(target_os = "linux")]
        linux: if crate::sandbox_disabled() {
            LinuxPreparation::default()
        } else {
            LinuxPreparation {
                clone_flags: linux_clone_flags(profile),
                map_current_user: true,
            }
        },
        #[cfg(windows)]
        windows: windows_preparation(profile, identity),
        inherit_handles: handles.to_vec(),
    })
}

#[cfg(target_os = "linux")]
fn linux_clone_flags(profile: &Profile) -> u64 {
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC;
    if profile.inner.network != crate::Network::Unrestricted {
        flags |= libc::CLONE_NEWNET;
    }
    flags as u64
}

/// Reflect the profile's Windows-relevant policy into LPAC construction data.
#[cfg(windows)]
fn windows_preparation(profile: &Profile, identity: &Identity) -> WindowsPreparation {
    let capabilities = profile
        .inner
        .capabilities
        .iter()
        .map(|c| c.moniker())
        .collect();

    // Network policy is not yet reflected into LPAC capability SIDs. The
    // Windows worker-side implementation remains unsupported.

    WindowsPreparation {
        app_container: identity.app_container.clone(),
        capabilities,
        lpac_opt_out: true,
        no_remote_images: true,
        win32k_lockdown: true,
        disallow_child_processes: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::profile::Capability;

    #[test]
    fn prepare_preserves_handles_and_self_maps() {
        let profile = Profile::deny_all().build();
        let handles = [RawHandle(3), RawHandle(7)];

        let prep = prepare(&profile, &Identity::default(), &handles).unwrap();

        assert_eq!(prep.inherit_handles, handles);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                prep.linux.clone_flags,
                (libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC | libc::CLONE_NEWNET)
                    as u64
            );
            assert!(prep.linux.map_current_user);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn filesystem_grants_preserve_default_namespaces() {
        let profile = Profile::deny_all().read_exec("/usr/lib").build();

        assert_eq!(
            linux_clone_flags(&profile),
            (libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS | libc::CLONE_NEWIPC)
                as u64
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrestricted_network_omits_network_namespace() {
        let profile = Profile::deny_all()
            .network(crate::Network::Unrestricted)
            .build();

        assert_eq!(
            linux_clone_flags(&profile),
            (libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC) as u64
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrestricted_network_keeps_filesystem_namespace() {
        let profile = Profile::deny_all()
            .network(crate::Network::Unrestricted)
            .read_exec("/usr/lib")
            .build();

        assert_eq!(
            linux_clone_flags(&profile),
            (libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWIPC) as u64
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_preparation_reflects_capabilities_and_hardening() {
        let profile = Profile::deny_all()
            .capability(Capability::LPAC_COM)
            .capability(Capability::REGISTRY_READ)
            .build();
        let identity = Identity {
            app_container: Some("worker.net".to_string()),
        };

        let win = prepare(&profile, &identity, &[]).unwrap().windows;

        assert_eq!(win.app_container.as_deref(), Some("worker.net"));
        assert_eq!(win.capabilities, vec!["lpacCom", "registryRead"]);
        assert!(win.lpac_opt_out);
        assert!(win.no_remote_images);
        assert!(win.win32k_lockdown);
        assert!(win.disallow_child_processes);
    }
}
