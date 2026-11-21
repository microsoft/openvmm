// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers shared by the unit tests.

use fuse::protocol::fuse_dirent_align;
use fuse::protocol::fuse_direntplus;
use zerocopy::FromBytes;

/// An entry decoded from a readdirplus reply.
pub(crate) struct DirEntry {
    pub name: String,
    /// The node ID of the entry, which hands the guest a lookup reference
    /// unless it is zero.
    pub node_id: u64,
    /// The offset at which the next read continues after this entry.
    pub offset: u64,
}

/// Decodes the entries of a readdirplus reply.
pub(crate) fn decode_dir_entries(mut reply: &[u8]) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    while !reply.is_empty() {
        let (entry, _) = fuse_direntplus::read_from_prefix(reply).unwrap();
        let name_start = size_of::<fuse_direntplus>();
        let name_end = name_start + entry.dirent.namelen as usize;
        entries.push(DirEntry {
            name: String::from_utf8(reply[name_start..name_end].to_vec()).unwrap(),
            node_id: entry.entry_out.nodeid,
            offset: entry.dirent.off,
        });
        reply = &reply[fuse_dirent_align(name_end)..];
    }
    entries
}

/// Asserts that `/proc` and `/sys`, the roots of procfs and sysfs, are two
/// directories with one inode number on two devices.
#[cfg(target_os = "linux")]
pub(crate) fn assert_proc_and_sys_share_an_inode_number() {
    use std::os::unix::fs::MetadataExt;

    let proc = std::fs::symlink_metadata("/proc").unwrap();
    let sys = std::fs::symlink_metadata("/sys").unwrap();
    assert_eq!(proc.ino(), sys.ino());
    assert_ne!(proc.dev(), sys.dev());
}
