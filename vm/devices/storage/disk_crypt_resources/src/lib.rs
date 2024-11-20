// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for the encrypted disk device.

#![warn(missing_docs)]

use mesh::MeshPayload;
use vm_resource::kind::DiskHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceId;

/// A handle to an encrypted disk.
#[derive(MeshPayload)]
pub struct DiskCryptHandle {
    /// The inner disk.
    pub inner: Resource<DiskHandleKind>,
    /// The cipher to use for encryption.
    pub cipher: Cipher,
    /// The key. This must be appropriately sized for the cipher.
    pub key: Vec<u8>,
}

impl ResourceId<DiskHandleKind> for DiskCryptHandle {
    const ID: &'static str = "crypt";
}

/// The cipher to use to encrypt the payload.
#[derive(MeshPayload)]
pub enum Cipher {
    /// AES-XTS-256, using the disk sector number as the tweak value (equivalent
    /// to and compatible with dm-crypt's "aes-xts-plain64").
    ///
    /// This requires a 512-bit key.
    AesXts256,
}