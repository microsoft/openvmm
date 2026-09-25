// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounds of the microVM virtio-fs inode, alias, and handle tables.

pub(crate) const MAX_INODES: usize = 4096;
pub(crate) const MAX_HANDLES: usize = 4096;
pub(crate) const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_ALIASES_PER_INODE: usize = 256;
pub(crate) const MAX_ALIASES: usize = 16 * 1024;
pub(crate) const MAX_ALIAS_BYTES: usize = 1024 * 1024;
