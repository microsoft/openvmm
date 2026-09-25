// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stable, device-private microVM virtio-fs state.
//!
//! These are deliberately separate from the runtime objects. In particular,
//! they contain relative names and object identities, never `LxFile`s, OS
//! handles, or host attachment paths.

use mesh::payload::Protobuf;
use vmcore::save_restore::SavedStateRoot;

pub(crate) const SCHEMA_VERSION: u32 = 4;
pub(crate) const MAX_INODES: usize = 4096;
pub(crate) const MAX_HANDLES: usize = 4096;
pub(crate) const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_ALIASES_PER_INODE: usize = 256;
pub(crate) const MAX_ALIASES: usize = 16 * 1024;
pub(crate) const MAX_ALIAS_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DIRECTORY_ENTRIES_PER_HANDLE: usize = 4096;
pub(crate) const MAX_DIRECTORY_ENTRY_NAME_BYTES: usize = 255;
pub(crate) const MAX_DIRECTORY_SNAPSHOT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 16 * 1024;
pub(crate) const MAX_DIRECTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedState {
    #[mesh(1)]
    pub schema_version: u32,
    #[mesh(2)]
    pub attachment_id: String,
    #[mesh(3)]
    pub access_mode: u32,
    #[mesh(4)]
    pub request_queues: u32,
    #[mesh(5)]
    pub shared_memory_size: u64,
    #[mesh(6)]
    pub packed_rings: bool,
    #[mesh(7)]
    pub direct_io: bool,
    #[mesh(8)]
    pub entry_cache_timeout_ns: u64,
    #[mesh(9)]
    pub attribute_cache_timeout_ns: u64,
    #[mesh(10)]
    pub negotiation: SavedNegotiation,
    #[mesh(11)]
    pub next_node_id: u64,
    #[mesh(12)]
    pub next_handle_id: u64,
    #[mesh(13)]
    pub inodes: Vec<SavedInode>,
    #[mesh(14)]
    pub handles: Vec<SavedHandle>,
    #[mesh(15)]
    pub attachment_root_identity: Vec<u8>,
    #[mesh(16)]
    pub maximum_request_size: u32,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedNegotiation {
    #[mesh(1)]
    pub initialized: bool,
    #[mesh(2)]
    pub major: u32,
    #[mesh(3)]
    pub minor: u32,
    #[mesh(4)]
    pub capable: u32,
    #[mesh(5)]
    pub capable2: u32,
    #[mesh(6)]
    pub want: u32,
    #[mesh(7)]
    pub want2: u32,
    #[mesh(8)]
    pub max_readahead: u32,
    #[mesh(9)]
    pub max_write: u32,
    #[mesh(10)]
    pub max_background: u32,
    #[mesh(11)]
    pub congestion_threshold: u32,
    #[mesh(12)]
    pub time_gran: u32,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedObjectIdentity {
    #[mesh(1)]
    pub device_id: u64,
    #[mesh(2)]
    pub inode_id: u64,
    #[mesh(3)]
    pub kind: u32,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedInode {
    #[mesh(1)]
    pub node_id: u64,
    #[mesh(2)]
    pub volume_id: u32,
    #[mesh(3)]
    pub relative_aliases: Vec<Vec<u8>>,
    #[mesh(4)]
    pub lookup_count: u64,
    #[mesh(5)]
    pub guest_inode_id: u64,
    #[mesh(6)]
    pub object_identity: SavedObjectIdentity,
}

#[derive(Clone, Protobuf)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedDirectoryEntry {
    #[mesh(1)]
    pub name: Vec<u8>,
    #[mesh(2)]
    pub next_cookie: u64,
    #[mesh(3)]
    pub guest_inode_id: u64,
    #[mesh(4)]
    pub kind: u32,
}

#[derive(Protobuf)]
#[mesh(package = "virtio.fs")]
pub(crate) struct SavedHandle {
    #[mesh(1)]
    pub handle_id: u64,
    #[mesh(2)]
    pub node_id: u64,
    #[mesh(3)]
    pub open_flags: u32,
    #[mesh(4)]
    pub kind: u32,
    #[mesh(5)]
    pub object_identity: SavedObjectIdentity,
    #[mesh(6)]
    pub directory_entries: Vec<SavedDirectoryEntry>,
    #[mesh(7)]
    pub directory_snapshot_built: bool,
}
