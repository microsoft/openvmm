// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM directory enumeration and persisted directory snapshots.

use super::saved_state::MAX_DIRECTORY_BYTES;
use super::saved_state::MAX_DIRECTORY_ENTRIES;
use super::saved_state::MAX_DIRECTORY_ENTRIES_PER_HANDLE;
use super::saved_state::MAX_DIRECTORY_ENTRY_NAME_BYTES;
use super::saved_state::MAX_DIRECTORY_SNAPSHOT_BYTES;
use super::saved_state::SavedDirectoryEntry;
use crate::VirtioFs;
use crate::file::DirectorySnapshot;
use crate::file::VirtioFsFile;
use fuse::DirEntryWriter;
use fuse::Fuse;
use fuse::protocol::fuse_entry_out;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// The directory snapshots that the handles of a microVM attachment hold, in
/// total. A saved state holds at most [`MAX_DIRECTORY_ENTRIES`] entries and
/// [`MAX_DIRECTORY_BYTES`] of names in the snapshots of all its handles, so a
/// handle takes its snapshot's share of these bounds before it keeps the
/// snapshot, and a save never finds more.
#[derive(Default)]
pub(crate) struct DirectorySnapshotBudget {
    used: Mutex<DirectorySnapshotSize>,
}

/// The entries of directory snapshots, and the bytes of their names.
#[derive(Default)]
struct DirectorySnapshotSize {
    entries: usize,
    bytes: usize,
}

/// The share of a [`DirectorySnapshotBudget`] that a directory snapshot
/// takes, which returns to the budget when the snapshot drops with its
/// handle.
pub(crate) struct DirectorySnapshotCharge {
    budget: Arc<DirectorySnapshotBudget>,
    size: DirectorySnapshotSize,
}

impl DirectorySnapshotBudget {
    /// Charges a snapshot of `entries` to the budget, or fails with `ENOSPC`
    /// when the snapshots would then exceed either bound.
    pub(crate) fn charge(
        self: &Arc<Self>,
        entries: &[SavedDirectoryEntry],
    ) -> lx::Result<DirectorySnapshotCharge> {
        let size = DirectorySnapshotSize {
            entries: entries.len(),
            bytes: entries
                .iter()
                .try_fold(0usize, |bytes, entry| bytes.checked_add(entry.name.len()))
                .ok_or(lx::Error::ENOSPC)?,
        };
        let mut used = self.used.lock();
        let entries = used
            .entries
            .checked_add(size.entries)
            .filter(|&entries| entries <= MAX_DIRECTORY_ENTRIES)
            .ok_or(lx::Error::ENOSPC)?;
        let bytes = used
            .bytes
            .checked_add(size.bytes)
            .filter(|&bytes| bytes <= MAX_DIRECTORY_BYTES)
            .ok_or(lx::Error::ENOSPC)?;
        *used = DirectorySnapshotSize { entries, bytes };
        Ok(DirectorySnapshotCharge {
            budget: Arc::clone(self),
            size,
        })
    }
}

impl Drop for DirectorySnapshotCharge {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock();
        used.entries -= self.size.entries;
        used.bytes -= self.size.bytes;
    }
}

/// The guest inode numbers by which the host lists a directory's entries now,
/// by name.
type HostListing = HashMap<Vec<u8>, lx::ino_t>;

impl VirtioFsFile {
    pub(crate) fn read_dir_microvm(
        &self,
        fs: &VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        if size as usize > crate::MAX_GUEST_BUFFER_SIZE {
            return Err(lx::Error::E2BIG);
        }

        let entries = {
            let mut snapshot = self.directory_snapshot.write();
            if !snapshot.built {
                if offset != 0 {
                    return Err(lx::Error::EINVAL);
                }
                let entries = self.build_microvm_directory_snapshot()?;
                let charge = fs.inner.directory_snapshot_budget.read().charge(&entries)?;
                *snapshot = DirectorySnapshot {
                    built: true,
                    entries,
                    _charge: Some(charge),
                };
            }
            snapshot.entries.clone()
        };

        let start = if offset == 0 {
            0
        } else {
            entries
                .iter()
                .position(|entry| entry.next_cookie == offset)
                .map(|index| index + 1)
                .ok_or(lx::Error::EINVAL)?
        };
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(size as usize)
            .map_err(|_| lx::Error::ENOMEM)?;

        let mut skipped = 0;
        // Listed again at most once per read, for the first entry whose
        // lookup reports another number than the snapshot.
        let mut host_listing = None;
        for entry in &entries[start..] {
            let name = lx::LxStr::from_bytes(&entry.name);
            if plus {
                if entry.name == b"." || entry.name == b".." {
                    if !buffer.dir_entry_plus(
                        name,
                        entry.next_cookie,
                        fuse_entry_out::new_dot(entry.guest_inode_id, entry.kind << 12),
                    ) {
                        break;
                    }
                } else {
                    match self.write_microvm_entry_plus(fs, &mut buffer, entry, &mut host_listing) {
                        Ok(true) => {}
                        Ok(false) => break,
                        // As in `read_dir_with`, skip an entry whose name is
                        // gone, as after an unlink or a rename since the
                        // snapshot listed it, or reaches an object that the
                        // access policy hides, as through a hard link to a
                        // hidden file. Failing the read instead would fail
                        // every later read too, since each starts at the
                        // entry, so the guest could never list past it.
                        Err(error) if error == lx::Error::ENOENT || error == lx::Error::EACCES => {
                            skipped += 1;
                        }
                        // Each entry in the reply hands the guest a lookup
                        // reference, which the guest can release only if it
                        // receives the entry. So, as in `read_dir_with`, return
                        // the entries read so far, and report the error only if
                        // there are none; the guest's next read starts at the
                        // failing entry.
                        Err(error) if buffer.is_empty() => return Err(error),
                        Err(_) => break,
                    }
                }
            } else if !buffer.dir_entry(name, entry.guest_inode_id, entry.next_cookie, entry.kind) {
                break;
            }
        }

        // An empty reply ends the listing. A read that returns no entry
        // without having skipped every entry left stopped at one that the
        // guest's buffer cannot hold.
        if buffer.is_empty() && entries[start..].len() > skipped {
            return Err(lx::Error::EINVAL);
        }
        Ok(buffer)
    }

    /// Writes the readdirplus entry of `entry`, an entry of this directory's
    /// snapshot other than `.` and `..`, to `buffer`, and returns whether it
    /// fit. `host_listing` is the directory as the host lists it now, if this
    /// read has listed it already (see [`Self::check_still_listed`]).
    ///
    /// The entry's lookup takes the lookup reference that the entry hands to
    /// the guest, so the reference is held only if the entry is written: an
    /// entry that does not fit, or that fails, holds none. An entry whose name
    /// reaches the object of this handle's own node hands the guest no node,
    /// so it holds none either.
    fn write_microvm_entry_plus(
        &self,
        fs: &VirtioFs,
        buffer: &mut Vec<u8>,
        entry: &SavedDirectoryEntry,
        host_listing: &mut Option<HostListing>,
    ) -> lx::Result<bool> {
        let name = lx::LxStr::from_bytes(&entry.name);
        // This check must happen before insert_lookup, because a
        // readdirplus lookup creates a guest lookup reference.
        if !buffer.check_dir_entry_plus(name) {
            return Ok(false);
        }
        let (child, attr) = self.inode.lookup_child(name)?;
        // The snapshot numbers each entry as an object of this directory's
        // host device. The host lists a mount point or a btrfs subvolume,
        // whose lookup reaches another device, by the number of another
        // object, so only an entry on this directory's device shows whether
        // the name still reaches the listed object.
        if child.device_nr() == self.inode.device_nr() && attr.ino != entry.guest_inode_id {
            self.check_still_listed(entry, host_listing)?;
        }
        // A name can reach the object of this handle's own node: a bind mount
        // of the directory on one of its own subdirectories does, and so does
        // the node's directory once the host moves it into a new directory at
        // the node's path, which a handle opened on the node then lists.
        // Inserting the object would make the name's path the node's path, at
        // which this read looks up the directory's other names, and hand the
        // guest a directory below itself, which the guest cannot hold. So the
        // entry hands out no node, as `.` and `..` do, and lists the name as
        // READDIR does.
        if child.dedup_key() == self.inode.dedup_key()
            && crate::InodeMap::reuses(&self.inode, &child)
        {
            return Ok(buffer.dir_entry_plus(
                name,
                entry.next_cookie,
                fuse_entry_out::new_dot(entry.guest_inode_id, entry.kind << 12),
            ));
        }
        let fuse_entry = fs.insert_lookup(child, attr)?;
        let node_id = fuse_entry.nodeid;
        if !buffer.dir_entry_plus(name, entry.next_cookie, fuse_entry) {
            // The guest never receives the entry, so release its reference.
            fs.forget(node_id, 1);
            return Err(lx::Error::EIO);
        }
        Ok(true)
    }

    /// Checks that `entry`, an entry of this directory's snapshot whose lookup
    /// reached an object of the directory's host device with another number
    /// than the snapshot's, still names the listed object.
    ///
    /// The host lists some entries on a directory's own device by another
    /// number than a lookup of the entry reports: a mount point of that
    /// device, such as a bind mount of one of its directories, by the number
    /// of what the mount covers, and the placeholder that a btrfs snapshot
    /// holds of a nested subvolume by the subvolume's ID, while a lookup
    /// reaches an empty directory numbered 2. Such an entry names the listed
    /// object as long as the host lists it by the snapshot's number, so the
    /// directory is listed again, as a snapshot lists it, into
    /// `host_listing`, at most once per read. An entry that the host now
    /// lists by another number, as after a rename over it, fails with EIO,
    /// and one that it no longer lists is gone, which fails with ENOENT. A
    /// mount point names whatever is mounted on it when the guest looks it
    /// up, as one of another device does. An entry that the host lists by
    /// the number 0, as a Windows file system that reports no file IDs does,
    /// has no number to check, so it fails with EIO too.
    fn check_still_listed(
        &self,
        entry: &SavedDirectoryEntry,
        host_listing: &mut Option<HostListing>,
    ) -> lx::Result<()> {
        if entry.guest_inode_id == self.inode.guest_ino(0) {
            return Err(lx::Error::EIO);
        }
        // The lookup resolved the name in the directory at the inode's path,
        // which must therefore still be the directory that this handle lists.
        let listed_directory = self.object_stat()?;
        let named_directory = self.inode.object_stat()?;
        if (listed_directory.device_nr, listed_directory.inode_nr)
            != (named_directory.device_nr, named_directory.inode_nr)
        {
            return Err(lx::Error::EIO);
        }
        let host_listing = match host_listing {
            Some(host_listing) => host_listing,
            None => host_listing.insert(
                self.build_microvm_directory_snapshot()?
                    .into_iter()
                    .map(|listed| (listed.name, listed.guest_inode_id))
                    .collect(),
            ),
        };
        match host_listing.get(&entry.name) {
            Some(&guest_inode_id) if guest_inode_id == entry.guest_inode_id => Ok(()),
            Some(_) => Err(lx::Error::EIO),
            None => Err(lx::Error::ENOENT),
        }
    }

    /// Lists this handle's directory into the entries that the access policy
    /// shows, `.` and `..` included, in the host's order, and fails with
    /// `E2BIG` when they exceed a handle's bounds of
    /// [`MAX_DIRECTORY_ENTRIES_PER_HANDLE`] entries or
    /// [`MAX_DIRECTORY_SNAPSHOT_BYTES`] of names. The entries that the policy
    /// hides count toward neither bound, wherever the host lists them.
    fn build_microvm_directory_snapshot(&self) -> lx::Result<Vec<SavedDirectoryEntry>> {
        self.build_microvm_directory_snapshot_within(MAX_DIRECTORY_ENTRIES_PER_HANDLE)
    }

    /// Lists this handle's directory as
    /// [`Self::build_microvm_directory_snapshot`] does, into at most
    /// `max_entries` entries. Tests pass fewer than a handle's bound, since a
    /// directory of that many entries takes seconds to create on Windows.
    pub(crate) fn build_microvm_directory_snapshot_within(
        &self,
        max_entries: usize,
    ) -> lx::Result<Vec<SavedDirectoryEntry>> {
        let stat = self.object_stat()?;
        if stat.mode & lx::S_IFMT != lx::S_IFDIR {
            return Err(lx::Error::ENOTDIR);
        }

        let self_inode_nr = self.inode.guest_inode_nr();
        let mut total_bytes = 0usize;
        let mut entries = Vec::new();
        let mut add_entry = |entry: lx::DirEntry| -> lx::Result<bool> {
            let name = entry.name.as_bytes();
            if name.is_empty() || name.len() > MAX_DIRECTORY_ENTRY_NAME_BYTES {
                return Err(lx::Error::EINVAL);
            }
            if name != b"." && name != b".." {
                match self.inode.child_path(&entry.name) {
                    Ok(path) if self.inode.volume.entry_listable(&path, entry.file_type) => {}
                    Ok(_) => return Ok(true),
                    Err(error) if error == lx::Error::EACCES => return Ok(true),
                    Err(error) => return Err(error),
                }
            }
            // Only here, so that the entries that the policy hides count
            // toward neither bound.
            if entries.len() == max_entries {
                return Err(lx::Error::E2BIG);
            }
            total_bytes = total_bytes
                .checked_add(name.len())
                .ok_or(lx::Error::E2BIG)?;
            if total_bytes > MAX_DIRECTORY_SNAPSHOT_BYTES {
                return Err(lx::Error::E2BIG);
            }
            let next_cookie = u64::try_from(entries.len())
                .ok()
                .and_then(|index| index.checked_add(1))
                .ok_or(lx::Error::E2BIG)?;
            entries.try_reserve(1).map_err(|_| lx::Error::ENOMEM)?;
            entries.push(SavedDirectoryEntry {
                name: name.to_vec(),
                next_cookie,
                guest_inode_id: if name == b"." || name == b".." {
                    self_inode_nr
                } else {
                    self.inode.guest_ino(entry.inode_nr)
                },
                kind: entry.file_type as u32,
            });
            Ok(true)
        };
        // A read can end before the directory does, as one on Windows does at
        // the end of each cached window of the host's enumeration, so read on
        // from the last entry until a read returns none.
        let mut file = self.file.write();
        let mut offset = 0;
        loop {
            let mut read_any = false;
            file.read_dir(offset, |entry| {
                read_any = true;
                offset = entry.offset;
                add_entry(entry)
            })?;
            if !read_any {
                break;
            }
        }
        Self::validate_directory_entries(&entries)?;
        Ok(entries)
    }

    pub(crate) fn validate_directory_entries(entries: &[SavedDirectoryEntry]) -> lx::Result<()> {
        if entries.len() > MAX_DIRECTORY_ENTRIES_PER_HANDLE {
            return Err(lx::Error::E2BIG);
        }
        let mut total_bytes = 0usize;
        let mut previous_cookie = 0;
        for (index, entry) in entries.iter().enumerate() {
            let dot_entry = entry.name == b"." || entry.name == b"..";
            if entry.name.is_empty()
                || entry.name.len() > MAX_DIRECTORY_ENTRY_NAME_BYTES
                || entry.kind > 15
                || (!dot_entry
                    && (entry.name.contains(&b'/')
                        || entry.name.contains(&b'\0')
                        || entry.name.contains(&b'\\')
                        || entry.name.contains(&b':')))
            {
                return Err(lx::Error::EINVAL);
            }
            if (index == 0 && entry.next_cookie == 0)
                || (index != 0 && entry.next_cookie <= previous_cookie)
            {
                return Err(lx::Error::EINVAL);
            }
            previous_cookie = entry.next_cookie;
            total_bytes = total_bytes
                .checked_add(entry.name.len())
                .ok_or(lx::Error::E2BIG)?;
            if total_bytes > MAX_DIRECTORY_SNAPSHOT_BYTES {
                return Err(lx::Error::E2BIG);
            }
        }
        Ok(())
    }
}
