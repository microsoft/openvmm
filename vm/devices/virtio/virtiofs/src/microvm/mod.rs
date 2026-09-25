// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM-specific virtio-fs profile, policy, persistence, and device support.

pub(crate) mod device;
pub(crate) mod file;
pub(crate) mod fs;
#[cfg(test)]
mod fs_tests;
pub(crate) mod inode;
mod limits;
pub mod profile;
pub(crate) mod resolver;
pub(crate) mod saved_state;
pub(crate) mod state;

const MAX_FUSE_REQUEST_HEADER_BYTES: usize = 4096;

pub(crate) const MAX_FUSE_REQUEST_BYTES: usize =
    profile::MICROVM_FUSE_MAX_WRITE as usize + MAX_FUSE_REQUEST_HEADER_BYTES;

const _: () = assert!(MAX_FUSE_REQUEST_BYTES > profile::MICROVM_FUSE_MAX_WRITE as usize);
