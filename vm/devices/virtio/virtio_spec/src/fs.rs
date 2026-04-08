// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio filesystem device definitions (virtio spec §5.11).

use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Maximum length of the filesystem tag in bytes.
pub const TAG_LEN: usize = 36;

/// Config space layout for virtio-fs (virtio spec §5.11.4).
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout)]
pub struct Config {
    pub tag: [u8; TAG_LEN],
    pub num_request_queues: crate::u32_le,
}
