// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM virtio-fs state encoding and validation.

use super::MAX_FUSE_REQUEST_BYTES;
use super::profile::MICROVM_FUSE_MAJOR;
use super::profile::MICROVM_FUSE_MAX_WRITE;
use super::profile::MICROVM_FUSE_MIN_MINOR;
use super::profile::MicroVmAccessMode;
use super::profile::MicroVmVirtioFsProfile;
use super::saved_state::MAX_ALIAS_BYTES;
use super::saved_state::MAX_ALIASES;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_DIRECTORY_BYTES;
use super::saved_state::MAX_DIRECTORY_ENTRIES;
use super::saved_state::MAX_DIRECTORY_ENTRIES_PER_HANDLE;
use super::saved_state::MAX_HANDLES;
use super::saved_state::MAX_INODES;
use super::saved_state::MAX_PATH_BYTES;
use super::saved_state::SCHEMA_VERSION;
use super::saved_state::SavedNegotiation;
use super::saved_state::SavedObjectIdentity;
use super::saved_state::SavedState;
use crate::FuseNegotiation;
use crate::file::VirtioFsFile;
use crate::inode::VirtioFsVolume;
use anyhow::Context;
use fuse::SessionInfoState;
use fuse::SessionState;
use fuse::protocol::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

const MICROVM_SESSION_DEFAULT_WANT: u32 = FUSE_ASYNC_READ
    | FUSE_PARALLEL_DIROPS
    | FUSE_AUTO_INVAL_DATA
    | FUSE_HANDLE_KILLPRIV
    | FUSE_ASYNC_DIO
    | FUSE_ATOMIC_O_TRUNC
    | FUSE_BIG_WRITES
    | FUSE_MAX_PAGES
    | FUSE_INIT_EXT;

fn saved_access_mode(access_mode: MicroVmAccessMode) -> u32 {
    match access_mode {
        MicroVmAccessMode::ReadOnly => 1,
        MicroVmAccessMode::ReadWrite => 2,
    }
}

pub(crate) fn fuse_negotiation_from_session(state: SessionState) -> FuseNegotiation {
    FuseNegotiation {
        initialized: state.initialized,
        major: state.info.major,
        minor: state.info.minor,
        capable: state.info.capable,
        capable2: state.info.capable2,
        want: state.info.want,
        want2: state.info.want2,
        max_readahead: state.info.max_readahead,
        max_write: state.info.max_write,
        max_background: state.info.max_background,
        congestion_threshold: state.info.congestion_threshold,
        time_gran: state.info.time_gran,
    }
}

pub(crate) fn saved_negotiation(state: SessionState) -> SavedNegotiation {
    SavedNegotiation {
        initialized: state.initialized,
        major: state.info.major,
        minor: state.info.minor,
        capable: state.info.capable,
        capable2: state.info.capable2,
        want: state.info.want,
        want2: state.info.want2,
        max_readahead: state.info.max_readahead,
        max_write: state.info.max_write,
        max_background: state.info.max_background.into(),
        congestion_threshold: state.info.congestion_threshold.into(),
        time_gran: state.info.time_gran,
    }
}

pub(crate) fn session_state_from_saved(
    negotiation: &SavedNegotiation,
) -> anyhow::Result<SessionState> {
    let state = SessionState {
        initialized: negotiation.initialized,
        info: SessionInfoState {
            major: negotiation.major,
            minor: negotiation.minor,
            max_readahead: negotiation.max_readahead,
            capable: negotiation.capable,
            capable2: negotiation.capable2,
            want: negotiation.want,
            want2: negotiation.want2,
            max_background: negotiation
                .max_background
                .try_into()
                .context("saved FUSE max_background is out of range")?,
            congestion_threshold: negotiation
                .congestion_threshold
                .try_into()
                .context("saved FUSE congestion_threshold is out of range")?,
            max_write: negotiation.max_write,
            time_gran: negotiation.time_gran,
        },
    };
    state
        .validate()
        .map_err(anyhow::Error::from)
        .context("saved FUSE session state is invalid")?;
    Ok(state)
}

pub(crate) fn saved_identity(stat: &lx::Stat) -> SavedObjectIdentity {
    SavedObjectIdentity {
        device_id: stat.device_nr,
        inode_id: stat.inode_nr,
        kind: stat.mode & lx::S_IFMT,
    }
}

pub(crate) fn validate_identity(
    stat: &lx::Stat,
    saved: &SavedObjectIdentity,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        saved.kind == stat.mode & lx::S_IFMT,
        "object kind changed from {:#x} to {:#x}",
        saved.kind,
        stat.mode & lx::S_IFMT
    );
    anyhow::ensure!(
        saved.device_id == stat.device_nr && saved.inode_id == stat.inode_nr,
        "object identity changed"
    );
    Ok(())
}

pub(crate) fn validate_saved_identity(
    actual: &SavedObjectIdentity,
    expected: &SavedObjectIdentity,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        actual.kind == expected.kind
            && actual.device_id == expected.device_id
            && actual.inode_id == expected.inode_id,
        "object identity changed between aliases"
    );
    Ok(())
}

pub(crate) fn validate_reopenable_alias(
    volume: &VirtioFsVolume,
    path: &Path,
) -> anyhow::Result<lx::Stat> {
    validate_relative_path(path, true).map_err(anyhow::Error::from)?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            anyhow::bail!("alias contains a non-normal path component");
        };
        prefix.push(component);
        let stat = volume
            .lstat(&prefix)
            .map_err(anyhow::Error::from)
            .context("alias component cannot be inspected")?;
        anyhow::ensure!(
            stat.mode & lx::S_IFMT != lx::S_IFLNK,
            "alias contains a symbolic-link component"
        );
    }
    volume
        .lstat(path)
        .map_err(anyhow::Error::from)
        .context("alias cannot be inspected")
}

pub(crate) fn validate_microvm_state(
    state: &SavedState,
    profile: &MicroVmVirtioFsProfile,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        state.schema_version == SCHEMA_VERSION,
        "unsupported virtio-fs state schema version {}",
        state.schema_version
    );
    anyhow::ensure!(
        state.attachment_id == profile.attachment_id(),
        "saved attachment ID does not match the microVM ABI"
    );
    anyhow::ensure!(
        state.attachment_root_identity == profile.root_identity(),
        "saved root identity does not match the restore attachment"
    );
    anyhow::ensure!(
        state.access_mode == saved_access_mode(profile.access_mode()),
        "saved access mode does not match the restore profile"
    );
    anyhow::ensure!(
        state.request_queues == profile.request_queues()
            && state.shared_memory_size == 0
            && !state.packed_rings
            && state.direct_io
            && state.entry_cache_timeout_ns == 0
            && state.attribute_cache_timeout_ns == 0
            && state.maximum_request_size == MAX_FUSE_REQUEST_BYTES as u32,
        "saved device policy does not match the fixed microVM ABI"
    );
    anyhow::ensure!(
        state.inodes.len() <= MAX_INODES,
        "saved inode table is too large"
    );
    anyhow::ensure!(
        state.handles.len() <= MAX_HANDLES,
        "saved handle table is too large"
    );
    anyhow::ensure!(
        state.next_node_id != 0 && state.next_handle_id != 0,
        "saved allocation state is invalid"
    );

    let session_state = session_state_from_saved(&state.negotiation)?;
    if session_state.initialized {
        let expected_want = (MICROVM_SESSION_DEFAULT_WANT & state.negotiation.capable)
            | (state.negotiation.capable & (FUSE_DO_READDIRPLUS | FUSE_READDIRPLUS_AUTO));
        let expected_want2 = if expected_want & FUSE_INIT_EXT != 0 {
            state.negotiation.capable2 & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2
        } else {
            0
        };
        anyhow::ensure!(
            state.negotiation.major == MICROVM_FUSE_MAJOR
                && state.negotiation.minor >= MICROVM_FUSE_MIN_MINOR
                && state.negotiation.minor <= FUSE_KERNEL_MINOR_VERSION
                && state.negotiation.want == expected_want
                && state.negotiation.want2 == expected_want2
                && state.negotiation.max_background == 0
                && state.negotiation.congestion_threshold == 0
                && state.negotiation.time_gran == 1
                && state.negotiation.max_write == MICROVM_FUSE_MAX_WRITE,
            "saved FUSE negotiation is not reproducible by the microVM profile"
        );
    }

    let mut root_count = 0;
    let mut largest_node_id = 0;
    let mut inode_ids = HashSet::with_capacity(state.inodes.len());
    let mut alias_paths = HashSet::new();
    let mut alias_count = 0usize;
    let mut alias_bytes = 0usize;
    for inode in &state.inodes {
        anyhow::ensure!(
            inode.node_id != 0 && inode_ids.insert(inode.node_id),
            "saved inode IDs are invalid or duplicated"
        );
        largest_node_id = largest_node_id.max(inode.node_id);
        if inode.node_id == FUSE_ROOT_ID {
            root_count += 1;
        }
        anyhow::ensure!(
            inode.volume_id == 0,
            "saved aggregate volumes are unsupported"
        );
        anyhow::ensure!(
            inode.lookup_count != 0,
            "saved inode lookup count is invalid"
        );
        anyhow::ensure!(
            inode.object_identity.kind != lx::S_IFLNK,
            "saved symlink identities are unsupported by the microVM profile"
        );
        anyhow::ensure!(
            !inode.relative_aliases.is_empty()
                && inode.relative_aliases.len() <= MAX_ALIASES_PER_INODE,
            "saved inode has no bounded aliases"
        );
        alias_count = alias_count
            .checked_add(inode.relative_aliases.len())
            .context("saved alias count overflow")?;
        anyhow::ensure!(alias_count <= MAX_ALIASES, "saved alias table is too large");
        for alias in &inode.relative_aliases {
            anyhow::ensure!(
                alias.len() <= MAX_PATH_BYTES,
                "saved relative alias is too long"
            );
            alias_bytes = alias_bytes
                .checked_add(alias.len())
                .context("saved alias bytes overflow")?;
            anyhow::ensure!(
                alias_bytes <= MAX_ALIAS_BYTES,
                "saved aliases exceed the aggregate byte bound"
            );
            let path = decode_relative_path(alias)?;
            validate_relative_path(&path, true).map_err(anyhow::Error::from)?;
            if inode.node_id == FUSE_ROOT_ID {
                anyhow::ensure!(
                    path.as_os_str().is_empty(),
                    "root inode has a non-root alias"
                );
            } else {
                anyhow::ensure!(
                    !path.as_os_str().is_empty(),
                    "non-root inode has a root alias"
                );
            }
            anyhow::ensure!(
                alias_paths.insert(path),
                "saved aliases are duplicated or ambiguous"
            );
        }
    }
    anyhow::ensure!(root_count == 1, "saved inode table must contain one root");
    anyhow::ensure!(
        state.next_node_id > largest_node_id,
        "saved next inode ID can reuse an active ID"
    );

    let mut largest_handle_id = 0;
    let mut handle_ids = HashSet::with_capacity(state.handles.len());
    let mut directory_entry_count = 0usize;
    let mut directory_bytes = 0usize;
    for handle in &state.handles {
        anyhow::ensure!(
            handle.handle_id != 0 && handle_ids.insert(handle.handle_id),
            "saved handle IDs are invalid or duplicated"
        );
        largest_handle_id = largest_handle_id.max(handle.handle_id);
        anyhow::ensure!(
            inode_ids.contains(&handle.node_id),
            "saved handle refers to an unknown inode"
        );
        anyhow::ensure!(
            handle.object_identity.kind == handle.kind && handle.kind != lx::S_IFLNK,
            "saved handle has an unsupported object kind"
        );
        let reopen_flags = reopen_flags(handle.open_flags)?;
        if profile.is_readonly() {
            anyhow::ensure!(
                reopen_flags & lx::O_ACCESS_MASK as u32 == lx::O_RDONLY as u32,
                "read-only microVM profile cannot restore a writable handle"
            );
        }
        anyhow::ensure!(
            handle.directory_entries.len() <= MAX_DIRECTORY_ENTRIES_PER_HANDLE,
            "saved directory enumeration is too large"
        );
        anyhow::ensure!(
            handle.directory_snapshot_built || handle.directory_entries.is_empty(),
            "uninitialized directory snapshot retained entries"
        );
        anyhow::ensure!(
            !handle.directory_snapshot_built || handle.kind == lx::S_IFDIR,
            "saved directory snapshot belongs to a non-directory handle"
        );
        VirtioFsFile::validate_directory_entries(&handle.directory_entries)
            .map_err(anyhow::Error::from)?;
        directory_entry_count = directory_entry_count
            .checked_add(handle.directory_entries.len())
            .context("saved directory entry count overflow")?;
        anyhow::ensure!(
            directory_entry_count <= MAX_DIRECTORY_ENTRIES,
            "saved directory entries exceed the aggregate bound"
        );
        for entry in &handle.directory_entries {
            directory_bytes = directory_bytes
                .checked_add(entry.name.len())
                .context("saved directory bytes overflow")?;
        }
        anyhow::ensure!(
            directory_bytes <= MAX_DIRECTORY_BYTES,
            "saved directory entries exceed the aggregate byte bound"
        );
    }
    anyhow::ensure!(
        state.next_handle_id > largest_handle_id,
        "saved next handle ID can reuse an active ID"
    );

    Ok(())
}

pub(crate) fn reopen_flags(saved: u32) -> anyhow::Result<u32> {
    const O_NOCTTY: u32 = 0x100;
    const O_NONBLOCK: u32 = 0x800;
    const O_DSYNC: u32 = 0x1000;
    const O_SYNC: u32 = 0x101000;
    const O_LARGEFILE: u32 = 0x8000;
    const O_CLOEXEC: u32 = 0x80000;
    const O_TMPFILE: u32 = 0x410000;

    anyhow::ensure!(
        saved & O_TMPFILE != O_TMPFILE,
        "saved handle uses unsupported O_TMPFILE semantics"
    );
    let allowed = lx::O_ACCESS_MASK as u32
        | lx::O_CREAT as u32
        | lx::O_EXCL as u32
        | lx::O_TRUNC as u32
        | lx::O_APPEND as u32
        | lx::O_DIRECTORY as u32
        | lx::O_NOFOLLOW as u32
        | lx::O_NOATIME as u32
        | O_NOCTTY
        | O_NONBLOCK
        | O_DSYNC
        | O_SYNC
        | O_LARGEFILE
        | O_CLOEXEC;
    anyhow::ensure!(
        saved & !allowed == 0,
        "saved handle has unsupported open flags {:#x}",
        saved
    );
    anyhow::ensure!(
        saved & lx::O_ACCESS_MASK as u32 != lx::O_NOACCESS as u32,
        "saved handle has invalid access mode"
    );
    Ok(saved & !(lx::O_CREAT as u32 | lx::O_EXCL as u32 | lx::O_TRUNC as u32))
}

pub(crate) fn encode_relative_path(path: &Path) -> anyhow::Result<Vec<u8>> {
    validate_relative_path(path, true).map_err(anyhow::Error::from)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let mut bytes = Vec::new();
        for unit in path.as_os_str().encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(bytes)
    }
}

pub(crate) fn relative_path_encoded_len(path: &Path) -> lx::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str()
            .encode_wide()
            .count()
            .checked_mul(size_of::<u16>())
            .ok_or(lx::Error::E2BIG)
    }
}

pub(crate) fn decode_relative_path(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        bytes.len() <= MAX_PATH_BYTES,
        "saved relative alias is too long"
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;

        anyhow::ensure!(
            bytes.len().is_multiple_of(2),
            "saved UTF-16 path is malformed"
        );
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect::<Vec<_>>();
        Ok(PathBuf::from(OsString::from_wide(&units)))
    }
}

pub(crate) fn validate_relative_path(path: &Path, strict: bool) -> lx::Result<()> {
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(lx::Error::EINVAL);
        };
        if component.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let component = component.as_bytes();
            if component.contains(&b'\0')
                || (strict && (component.contains(&b'\\') || component.contains(&b':')))
            {
                return Err(lx::Error::EINVAL);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;

            let units = component.encode_wide().collect::<Vec<_>>();
            if units.contains(&0)
                || (strict && (units.contains(&(b'\\' as u16)) || units.contains(&(b':' as u16))))
            {
                return Err(lx::Error::EINVAL);
            }
        }
    }
    Ok(())
}
