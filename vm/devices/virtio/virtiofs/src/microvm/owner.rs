// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host identity of guest filesystem requests (`--mount-owner`).
//!
//! In caller mode, a worker thread performs each request as the host UID and
//! GID of its guest caller, as the guest kernel reports them in the FUSE
//! request header. Guest UID 0 and GID 0 are squashed to the owner of the
//! export root, so the guest can create neither root-owned nor setuid-root
//! files on the host. A request that cannot run as its caller fails with
//! `EPERM`; it never falls back to the VMM's own identity.

use fuse::protocol::FUSE_BATCH_FORGET;
use fuse::protocol::FUSE_DESTROY;
use fuse::protocol::FUSE_FORGET;
use fuse::protocol::FUSE_INIT;
use fuse::protocol::FUSE_INTERRUPT;
use fuse::protocol::FUSE_RELEASE;
use fuse::protocol::FUSE_RELEASEDIR;

/// Maps guest callers to the host identities that perform their requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CallerIdentity {
    /// The owner of the export root, which replaces guest UID 0.
    squash_uid: lx::uid_t,
    /// The group of the export root, which replaces guest GID 0.
    squash_gid: lx::gid_t,
}

impl CallerIdentity {
    /// Builds the mapping for an export root owned by `uid` and `gid`.
    pub(crate) fn for_export_root_owner(uid: lx::uid_t, gid: lx::gid_t) -> anyhow::Result<Self> {
        anyhow::ensure!(
            uid != 0 && gid != 0,
            "microVM virtio-fs caller ownership squashes guest root to the owner of the export root, which must not be UID 0 or GID 0"
        );
        Ok(Self {
            squash_uid: uid,
            squash_gid: gid,
        })
    }

    /// Returns the host UID and GID that perform a request from guest `uid`
    /// and `gid`.
    pub(crate) fn host_identity(&self, uid: lx::uid_t, gid: lx::gid_t) -> (lx::uid_t, lx::gid_t) {
        (
            if uid == 0 { self.squash_uid } else { uid },
            if gid == 0 { self.squash_gid } else { gid },
        )
    }
}

/// Returns whether handling a request with `opcode` may access the host
/// filesystem.
///
/// Negotiating the session and releasing host objects never check
/// permissions, so these requests run as the VMM. The guest can therefore
/// always mount, unmount, and release what it opened, even when its other
/// requests fail closed.
fn accesses_host_files(opcode: u32) -> bool {
    !matches!(
        opcode,
        FUSE_INIT
            | FUSE_DESTROY
            | FUSE_FORGET
            | FUSE_BATCH_FORGET
            | FUSE_INTERRUPT
            | FUSE_RELEASE
            | FUSE_RELEASEDIR
    )
}

/// Dispatches `request` to `session`, performing it as the host identity of
/// its caller when `caller_identity` is set.
pub(crate) fn dispatch(
    caller_identity: Option<&CallerIdentity>,
    session: &fuse::Session,
    request: fuse::Request,
    sender: &mut impl fuse::ReplySender,
    mapper: Option<&dyn fuse::Mapper>,
) {
    dispatch_with(caller_identity, session, request, sender, mapper, run_as);
}

/// Dispatches `request` like [`dispatch`], but performs a request that runs
/// as its caller through `run_as`, which receives the caller's host UID and
/// GID and either performs the operation as that identity or fails.
pub(crate) fn dispatch_with(
    caller_identity: Option<&CallerIdentity>,
    session: &fuse::Session,
    request: fuse::Request,
    sender: &mut impl fuse::ReplySender,
    mapper: Option<&dyn fuse::Mapper>,
    run_as: impl FnOnce(lx::uid_t, lx::gid_t, &mut dyn FnMut()) -> lx::Result<()>,
) {
    match caller_identity {
        Some(caller_identity) if accesses_host_files(request.opcode()) => {
            let unique = request.unique();
            let (uid, gid) = caller_identity.host_identity(request.uid(), request.gid());
            let mut request = Some(request);
            let result = run_as(uid, gid, &mut || {
                if let Some(request) = request.take() {
                    session.dispatch(request, sender, mapper);
                }
            });
            if let Err(error) = result {
                tracelimit::warn_ratelimited!(
                    uid,
                    gid,
                    error = error.value(),
                    "virtio-fs request cannot run as its caller; failing it with EPERM"
                );
                if let Err(error) = sender.send_error(unique, lx::Error::EPERM.value()) {
                    tracelimit::error_ratelimited!(
                        unique,
                        error = &error as &dyn std::error::Error,
                        "Failed to send reply",
                    );
                }
            }
        }
        _ => session.dispatch(request, sender, mapper),
    }
}

/// Performs `operation` as host `uid` and `gid`.
#[cfg(target_os = "linux")]
fn run_as(uid: lx::uid_t, gid: lx::gid_t, operation: &mut dyn FnMut()) -> lx::Result<()> {
    lxutil::with_fs_identity(lxutil::FsIdentity { uid, gid }, operation)
}

/// Fails, because the profile accepts caller ownership only on Linux.
#[cfg(not(target_os = "linux"))]
fn run_as(_uid: lx::uid_t, _gid: lx::gid_t, _operation: &mut dyn FnMut()) -> lx::Result<()> {
    Err(lx::Error::EPERM)
}
