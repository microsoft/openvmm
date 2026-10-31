// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMDK disk format support.

#![deny(unsafe_code)]

mod errors;
mod flat;
#[cfg(target_os = "linux")]
mod io_uring;

pub use errors::OpenError;
pub use errors::ParseError;
pub use flat::FlatVmdkResolver;
pub use flat::ResolveFlatVmdkError;

use std::path::Path;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

// Reject VMDK descriptor files larger than 1 MiB.
pub(crate) const MAX_DESCRIPTOR_SIZE: u64 = 1 << 20;

/// Opens and validates a VMDK descriptor and its extent files.
pub fn open_vmdk(path: &Path, read_only: bool) -> Result<Resource<DiskHandleKind>, OpenError> {
    Ok(Resource::new(flat::open_flat_vmdk(path, read_only)?))
}
