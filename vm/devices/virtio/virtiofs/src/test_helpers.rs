// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers shared by the unit tests.

use fuse::protocol::fuse_dirent;
use fuse::protocol::fuse_dirent_align;
use fuse::protocol::fuse_direntplus;
use zerocopy::FromBytes;

/// An entry decoded from a readdir or readdirplus reply.
pub(crate) struct DirEntry {
    pub name: String,
    /// The node ID of a readdirplus entry, which hands the guest a lookup
    /// reference unless it is zero. Always zero for a readdir entry.
    pub node_id: u64,
    /// The offset at which the next read continues after this entry.
    pub offset: u64,
}

/// Decodes the entries of a readdir reply, or of a readdirplus reply if `plus`.
pub(crate) fn decode_dir_entries(mut reply: &[u8], plus: bool) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    while !reply.is_empty() {
        let (node_id, dirent, header_len) = if plus {
            let (entry, _) = fuse_direntplus::read_from_prefix(reply).unwrap();
            (
                entry.entry_out.nodeid,
                entry.dirent,
                size_of::<fuse_direntplus>(),
            )
        } else {
            let (dirent, _) = fuse_dirent::read_from_prefix(reply).unwrap();
            (0, dirent, size_of::<fuse_dirent>())
        };
        let name_end = header_len + dirent.namelen as usize;
        entries.push(DirEntry {
            name: String::from_utf8(reply[header_len..name_end].to_vec()).unwrap(),
            node_id,
            offset: dirent.off,
        });
        reply = &reply[fuse_dirent_align(name_end)..];
    }
    entries
}
