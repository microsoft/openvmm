// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for performing microVM requests as their guest callers.

use super::owner::CallerIdentity;
use test_with_tracing::test;

#[test]
fn guest_root_is_squashed_to_the_export_root_owner() {
    let identity = CallerIdentity::for_export_root_owner(1000, 1001).unwrap();
    assert_eq!(identity.host_identity(0, 0), (1000, 1001));
    assert_eq!(identity.host_identity(0, 5), (1000, 5));
    assert_eq!(identity.host_identity(4242, 0), (4242, 1001));
    assert_eq!(identity.host_identity(4242, 4343), (4242, 4343));
}

#[test]
fn root_owned_export_roots_are_rejected() {
    for (uid, gid) in [(0, 1000), (1000, 0), (0, 0)] {
        assert!(CallerIdentity::for_export_root_owner(uid, gid).is_err());
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::super::owner;
    use super::super::owner::CallerIdentity;
    use super::super::profile::MICROVM_ATTACHMENT_ID;
    use super::super::profile::MicroVmOwnerMode;
    use super::super::profile::MicroVmVirtioFsProfile;
    use super::super::profile::microvm_root_identity;
    use super::super::saved_state::CALLER_IDENTITY_SCHEMA_VERSION;
    use super::super::saved_state::PREVIOUS_SCHEMA_VERSION;
    use super::super::saved_state::SCHEMA_VERSION;
    use super::super::state::validate_microvm_state;
    use crate::VirtioFs;
    use fuse::Request;
    use fuse::Session;
    use fuse::SessionState;
    use fuse::protocol::FUSE_GETATTR;
    use fuse::protocol::FUSE_INIT;
    use fuse::protocol::FUSE_MKDIR;
    use fuse::protocol::FUSE_ROOT_ID;
    use fuse::protocol::fuse_entry_out;
    use fuse::protocol::fuse_getattr_in;
    use fuse::protocol::fuse_in_header;
    use fuse::protocol::fuse_init_in;
    use fuse::protocol::fuse_mkdir_in;
    use fuse::protocol::fuse_out_header;
    use lxutil::FsIdentity;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;
    use test_with_tracing::test;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    const EACCES: i32 = -13;
    const EPERM: i32 = -1;

    /// The export root owner when the tests run as root, which caller
    /// ownership refuses as an owner.
    const ROOT_RUN_OWNER: u32 = 65533;

    fn profile(root: &Path, owner_mode: MicroVmOwnerMode) -> MicroVmVirtioFsProfile {
        MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root).unwrap(),
            false,
            Vec::new(),
        )
        .unwrap()
        .with_owner_mode(owner_mode)
        .unwrap()
    }

    /// Creates a world-writable export root that root does not own.
    fn export_root() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        if FsIdentity::current().uid == 0 {
            std::os::unix::fs::chown(directory.path(), Some(ROOT_RUN_OWNER), Some(ROOT_RUN_OWNER))
                .unwrap();
        }
        directory
    }

    fn owner(path: &Path) -> (u32, u32) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        (metadata.uid(), metadata.gid())
    }

    fn vmm() -> (u32, u32) {
        let current = FsIdentity::current();
        (current.uid, current.gid)
    }

    /// Returns whether the calling thread has supplementary groups other than
    /// its filesystem GID, which an unprivileged VMM cannot drop.
    fn has_other_groups() -> bool {
        let gid = FsIdentity::current().gid;
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let groups = status
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))
            .unwrap();
        groups
            .split_whitespace()
            .any(|group| group.parse::<u32>().unwrap() != gid)
    }

    /// Runs `test` without effective capabilities, as an unprivileged VMM does,
    /// even if the tests run as root.
    fn unprivileged<T>(test: impl FnOnce() -> T) -> T {
        let mut test = Some(test);
        // Keeping the current identity clears the capabilities and the other
        // supplementary groups. Only a thread without CAP_SETGID, which is
        // already unprivileged, fails to drop those groups.
        match lxutil::with_fs_identity(FsIdentity::current(), || test.take().unwrap()()) {
            Ok(result) => result,
            Err(_) => test.take().unwrap()(),
        }
    }

    /// Records the host identities that `dispatch_with` assumes, performing
    /// each request as the test's own identity.
    #[derive(Default)]
    struct Recorder(Vec<(u32, u32)>);

    impl Recorder {
        fn run_as(&mut self) -> impl FnOnce(u32, u32, &mut dyn FnMut()) -> lx::Result<()> + '_ {
            |uid, gid, operation| {
                self.0.push((uid, gid));
                operation();
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct Reply(Vec<u8>);

    impl fuse::ReplySender for Reply {
        fn send(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<()> {
            self.0.clear();
            for buf in bufs {
                self.0.extend_from_slice(buf);
            }
            Ok(())
        }
    }

    /// A mounted microVM share whose requests are dispatched the way the
    /// virtio-fs worker dispatches them.
    struct Share {
        directory: TempDir,
        session: Session,
        caller_identity: Option<CallerIdentity>,
        next_unique: u64,
    }

    impl Share {
        fn new(owner_mode: MicroVmOwnerMode) -> Self {
            let directory = export_root();
            let fs = VirtioFs::new_microvm(directory.path(), profile(directory.path(), owner_mode))
                .unwrap();
            let mut share = Self {
                session: Session::new(fs.clone()),
                caller_identity: fs.caller_identity(),
                directory,
                next_unique: 1,
            };
            // Negotiation never accesses host files, so it never runs as its
            // caller, and any caller may mount.
            let mut init = fuse_init_in::new_zeroed();
            init.major = 7;
            init.minor = 31;
            let foreign = share.foreign();
            let (error, _) = share.send_with(
                FUSE_INIT,
                FUSE_ROOT_ID,
                foreign,
                init.as_bytes(),
                |_, _, _| unreachable!("negotiation never runs as its caller"),
            );
            assert_eq!(error, 0);
            share
        }

        /// Returns an identity that is neither the VMM's nor the export root
        /// owner's.
        fn foreign(&self) -> (u32, u32) {
            let taken = [vmm(), self.root_owner()];
            let mut candidate = (4242, 4343);
            while taken
                .iter()
                .any(|&(uid, gid)| uid == candidate.0 || gid == candidate.1)
            {
                candidate = (candidate.0 + 2, candidate.1 + 2);
            }
            candidate
        }

        fn root_owner(&self) -> (u32, u32) {
            owner(self.directory.path())
        }

        fn owner(&self, name: &str) -> (u32, u32) {
            owner(&self.directory.path().join(name))
        }

        /// Sends one request and returns the reply's error and payload.
        fn send(
            &mut self,
            opcode: u32,
            node_id: u64,
            caller: (u32, u32),
            body: &[u8],
        ) -> (i32, Vec<u8>) {
            self.exchange(
                opcode,
                node_id,
                caller,
                body,
                |identity, session, request, reply| {
                    owner::dispatch(identity, session, request, reply, None);
                },
            )
        }

        /// Sends one request like [`Self::send`], but performs a request that
        /// runs as its caller through `run_as`.
        fn send_with(
            &mut self,
            opcode: u32,
            node_id: u64,
            caller: (u32, u32),
            body: &[u8],
            run_as: impl FnOnce(u32, u32, &mut dyn FnMut()) -> lx::Result<()>,
        ) -> (i32, Vec<u8>) {
            self.exchange(
                opcode,
                node_id,
                caller,
                body,
                |identity, session, request, reply| {
                    owner::dispatch_with(identity, session, request, reply, None, run_as);
                },
            )
        }

        fn exchange(
            &mut self,
            opcode: u32,
            node_id: u64,
            (uid, gid): (u32, u32),
            body: &[u8],
            dispatch: impl FnOnce(Option<&CallerIdentity>, &Session, Request, &mut Reply),
        ) -> (i32, Vec<u8>) {
            let header = fuse_in_header {
                len: (size_of::<fuse_in_header>() + body.len()) as u32,
                opcode,
                unique: self.next_unique,
                nodeid: node_id,
                uid,
                gid,
                pid: 1,
                padding: 0,
            };
            self.next_unique += 1;
            let mut bytes = header.as_bytes().to_vec();
            bytes.extend_from_slice(body);
            let mut reply = Reply::default();
            dispatch(
                self.caller_identity.as_ref(),
                &self.session,
                Request::new(bytes.as_slice()).unwrap(),
                &mut reply,
            );
            let (header, payload) = fuse_out_header::read_from_prefix(&reply.0).unwrap();
            (header.error, payload.to_vec())
        }

        /// Creates the directory `name` in `parent` as `caller` and returns
        /// the reply's error and the new node ID.
        fn mkdir(&mut self, parent: u64, name: &str, caller: (u32, u32)) -> (i32, u64) {
            let (error, payload) = self.send(FUSE_MKDIR, parent, caller, &mkdir_body(name));
            (error, entry_node_id(error, &payload))
        }

        /// Creates a directory like [`Self::mkdir`], but performs the request
        /// through `run_as`.
        fn mkdir_with(
            &mut self,
            parent: u64,
            name: &str,
            caller: (u32, u32),
            run_as: impl FnOnce(u32, u32, &mut dyn FnMut()) -> lx::Result<()>,
        ) -> (i32, u64) {
            let (error, payload) =
                self.send_with(FUSE_MKDIR, parent, caller, &mkdir_body(name), run_as);
            (error, entry_node_id(error, &payload))
        }

        fn getattr(&mut self, node_id: u64, caller: (u32, u32)) -> i32 {
            let arg = fuse_getattr_in::new_zeroed();
            self.send(FUSE_GETATTR, node_id, caller, arg.as_bytes()).0
        }
    }

    fn mkdir_body(name: &str) -> Vec<u8> {
        let mut body = fuse_mkdir_in {
            mode: 0o755,
            umask: 0,
        }
        .as_bytes()
        .to_vec();
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body
    }

    fn entry_node_id(error: i32, payload: &[u8]) -> u64 {
        if error == 0 {
            fuse_entry_out::read_from_prefix(payload).unwrap().0.nodeid
        } else {
            0
        }
    }

    #[test]
    fn requests_run_as_their_callers_with_guest_root_squashed() {
        let mut share = Share::new(MicroVmOwnerMode::Caller);
        let foreign = share.foreign();
        // Only a privileged process can create an export root that neither it
        // nor `foreign` owns, so map guest root to such an owner directly.
        let mut owner = (foreign.0 + 1, foreign.1 + 1);
        while owner.0 == vmm().0 || owner.1 == vmm().1 {
            owner = (owner.0 + 2, owner.1 + 2);
        }
        share.caller_identity =
            Some(CallerIdentity::for_export_root_owner(owner.0, owner.1).unwrap());
        for (name, caller, expected) in [
            ("root", (0, 0), owner),
            ("root-user", (0, foreign.1), (owner.0, foreign.1)),
            ("root-group", (foreign.0, 0), (foreign.0, owner.1)),
            ("foreign", foreign, foreign),
        ] {
            let mut recorder = Recorder::default();
            let (error, _) = share.mkdir_with(FUSE_ROOT_ID, name, caller, recorder.run_as());
            assert_eq!(error, 0, "{name}");
            assert_eq!(recorder.0, [expected], "{name}");
        }

        // A request that cannot run as its caller fails without being
        // performed.
        let (error, _) = share.mkdir_with(FUSE_ROOT_ID, "denied", foreign, |_, _, _| {
            Err(lx::Error::EPERM)
        });
        assert_eq!(error, EPERM);
        assert!(!share.directory.path().join("denied").exists());
    }

    #[test]
    fn guest_root_creates_entries_as_the_export_root_owner() {
        let mut share = Share::new(MicroVmOwnerMode::Caller);
        let (uid, gid) = share.root_owner();
        if lxutil::with_fs_identity(FsIdentity { uid, gid }, || ()).is_err() {
            // An unprivileged VMM with other supplementary groups cannot drop
            // them, so even guest root fails closed.
            assert_eq!(share.mkdir(FUSE_ROOT_ID, "root", (0, 0)).0, EPERM);
            assert!(!share.directory.path().join("root").exists());
            return;
        }
        let (error, directory) = share.mkdir(FUSE_ROOT_ID, "root", (0, 0));
        assert_eq!(error, 0);
        let (error, _) = share.mkdir(directory, "nested", (0, 0));
        assert_eq!(error, 0);
        assert_eq!(share.owner("root"), share.root_owner());
        assert_eq!(share.owner("root/nested"), share.root_owner());
    }

    #[test]
    fn callers_with_the_vmm_identity_need_no_privilege_without_other_groups() {
        // Root cannot own an export root that caller ownership accepts.
        if FsIdentity::current().uid == 0 {
            return;
        }
        let mut share = Share::new(MicroVmOwnerMode::Caller);
        assert_eq!(share.root_owner(), vmm());
        unprivileged(|| {
            if has_other_groups() {
                // An unprivileged VMM cannot drop its other supplementary
                // groups, so even callers with its identity fail closed.
                assert_eq!(share.mkdir(FUSE_ROOT_ID, "root", (0, 0)).0, EPERM);
                assert_eq!(share.mkdir(FUSE_ROOT_ID, "vmm", vmm()).0, EPERM);
                assert!(!share.directory.path().join("root").exists());
                assert!(!share.directory.path().join("vmm").exists());
            } else {
                assert_eq!(share.mkdir(FUSE_ROOT_ID, "root", (0, 0)).0, 0);
                assert_eq!(share.mkdir(FUSE_ROOT_ID, "vmm", vmm()).0, 0);
                assert_eq!(share.owner("root"), vmm());
                assert_eq!(share.owner("vmm"), vmm());
            }
        });
    }

    #[test]
    fn foreign_callers_fail_closed_without_privilege() {
        let mut share = Share::new(MicroVmOwnerMode::Caller);
        let foreign = share.foreign();
        let identity = FsIdentity::current();
        unprivileged(|| {
            assert_eq!(share.getattr(FUSE_ROOT_ID, foreign), EPERM);
            assert_eq!(share.mkdir(FUSE_ROOT_ID, "foreign", foreign).0, EPERM);
            assert_eq!(FsIdentity::current(), identity);
        });
        assert!(!share.directory.path().join("foreign").exists());
    }

    #[test]
    fn vmm_mode_performs_every_request_as_the_vmm() {
        let mut share = Share::new(MicroVmOwnerMode::Vmm);
        assert!(share.caller_identity.is_none());
        let foreign = share.foreign();
        assert_eq!(share.mkdir(FUSE_ROOT_ID, "foreign", foreign).0, 0);
        assert_eq!(share.owner("foreign"), vmm());
    }

    #[test]
    fn root_owned_export_root_is_rejected() {
        let root = Path::new("/");
        if owner(root) != (0, 0) {
            return;
        }
        let error = VirtioFs::new_microvm(root, profile(root, MicroVmOwnerMode::Caller))
            .err()
            .unwrap();
        assert!(error.to_string().contains("must not be UID 0 or GID 0"));
        VirtioFs::new_microvm(root, profile(root, MicroVmOwnerMode::Vmm)).unwrap();
    }

    #[test]
    fn saved_state_requires_the_same_owner_mode() {
        let directory = export_root();
        let caller = profile(directory.path(), MicroVmOwnerMode::Caller);
        let vmm = profile(directory.path(), MicroVmOwnerMode::Vmm);
        for (saved, other) in [(&caller, &vmm), (&vmm, &caller)] {
            let mut state = VirtioFs::new_microvm(directory.path(), saved.clone())
                .unwrap()
                .save_microvm_state(saved, SessionState::default())
                .unwrap();
            let caller_owned = saved.owner_mode() == MicroVmOwnerMode::Caller;
            assert_eq!(state.caller_identity, caller_owned);
            assert_eq!(
                state.schema_version,
                if caller_owned {
                    CALLER_IDENTITY_SCHEMA_VERSION
                } else {
                    SCHEMA_VERSION
                }
            );
            // Readers that predate caller ownership accept only the previous
            // versions, so they refuse to restore a caller-owned attachment as
            // the VMM.
            assert_eq!(
                matches!(
                    state.schema_version,
                    PREVIOUS_SCHEMA_VERSION | SCHEMA_VERSION
                ),
                !caller_owned
            );
            validate_microvm_state(&state, saved).unwrap();
            assert!(validate_microvm_state(&state, other).is_err());

            state.schema_version = if caller_owned {
                SCHEMA_VERSION
            } else {
                CALLER_IDENTITY_SCHEMA_VERSION
            };
            assert!(validate_microvm_state(&state, saved).is_err());
        }
    }

    /// Run with `CAP_SETUID` and `CAP_SETGID`, for example as root.
    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID"]
    fn foreign_callers_own_what_they_create_with_privilege() {
        let mut share = Share::new(MicroVmOwnerMode::Caller);
        let caller = share.foreign();
        let (error, directory) = share.mkdir(FUSE_ROOT_ID, "workload", caller);
        assert_eq!(error, 0, "run with CAP_SETUID and CAP_SETGID");
        // The caller can populate the directory that it created.
        assert_eq!(share.mkdir(directory, "nested", caller).0, 0);
        assert_eq!(share.owner("workload"), caller);
        assert_eq!(share.owner("workload/nested"), caller);

        // Host permissions apply to the caller, without the VMM's capabilities.
        let intruder = (caller.0 + 1, caller.1 + 1);
        assert_eq!(share.mkdir(directory, "intruder", intruder).0, EACCES);
        // Guest root is squashed to the export root owner, never host root.
        assert_eq!(share.mkdir(FUSE_ROOT_ID, "root", (0, 0)).0, 0);
        assert_eq!(share.owner("root"), share.root_owner());

        let (uid, gid) = caller;
        let workload = share.directory.path().join("workload");
        lxutil::with_fs_identity(FsIdentity { uid, gid }, || {
            std::fs::remove_dir_all(workload)
        })
        .unwrap()
        .unwrap();
    }
}
