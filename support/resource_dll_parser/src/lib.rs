// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Package to agnostically parse a resource dll for a given ID.
//!
//! # Example
//! ```no_run
//! use resource_dll_parser::{DllResourceDescriptor, try_find_resource_from_dll};
//! use fs_err::File;
//!
//! # fn main() -> anyhow::Result<()> {
//! let file = File::open("vmfirmware.dll")?;
//! let descriptor = DllResourceDescriptor::new(b"VMFW", 13515);
//! let resource = try_find_resource_from_dll(&file, &descriptor)?;
//! # Ok(())
//! # }
//! ```

use anyhow::Context;
use anyhow::bail;
use fs_err::File;
use object::LittleEndian;
use object::ReadCache;
use object::read::pe::PeFile64;

/// Tries to read the given resource from a resource dll. If the given data
/// buffer is not a valid PE file this function returns Ok(None). If it is a PE
/// file, but the given resource can not be found or loaded this function
/// returns Err(...). On success the return value contains the starting offset
/// into the file and its length.
/// TODO: change the return types to a proper enum with variants like 'NotPeFile, NotFound, Ok(u64, usize)'
pub fn try_find_resource_from_dll(
    file: &File,
    descriptor: &DllResourceDescriptor,
) -> anyhow::Result<Option<(u64, usize)>> {
    let data = &ReadCache::new(file);
    if let Ok(pe_file) = PeFile64::parse(data) {
        let rsrc = pe_file
            .data_directories()
            .resource_directory(data, &pe_file.section_table())?
            .context("no resource section")?;

        let type_match = rsrc
            .root()?
            .entries
            .iter()
            .find(|e| {
                e.name_or_id().name().map(|n| n.raw_data(rsrc))
                    == Some(Ok(&descriptor.resource_type))
            })
            .context("no entry for resource type found")?
            .data(rsrc)?
            .table()
            .context("resource type entry not a table")?;

        let id_match = type_match
            .entries
            .iter()
            .find(|e| e.name_or_id.get(LittleEndian) == descriptor.id)
            .context("no entry for id found")?
            .data(rsrc)?
            .table()
            .context("id entry not a table")?;

        if id_match.entries.len() != 1 {
            bail!(
                "id table doesn't contain exactly 1 entry, contains {}",
                id_match.entries.len()
            );
        }
        let data_desc = id_match.entries[0]
            .data(rsrc)?
            .data()
            .context("resource entry not data")?;

        let (offset, len) = (
            data_desc.offset_to_data.get(LittleEndian),
            data_desc.size.get(LittleEndian),
        );

        let result = &pe_file
            .section_table()
            .pe_file_range_at(offset)
            .context("unable to map data offset")?;

        Ok(Some((result.0 as u64, len as usize)))
    } else {
        // Failing to parse the file as a dll is fine, it means the file is
        // probably a blob instead.
        Ok(None)
    }
}

/// Descriptor for locating a resource within a DLL file.
///
/// Contains the resource type (as a 4-character ASCII string encoded in LE UTF-16)
/// and a numeric resource ID.
pub struct DllResourceDescriptor {
    /// 4 characters encoded in LE UTF-16
    resource_type: [u8; 8],
    id: u32,
}

impl DllResourceDescriptor {
    /// Creates a new DLL resource descriptor with the given resource type and ID.
    ///
    /// The resource type must be a 4-character ASCII string, which will be converted
    /// to little-endian UTF-16 encoding.
    pub const fn new(resource_type: &[u8; 4], id: u32) -> Self {
        Self {
            id,
            // Convert to LE UTF-16, only support ASCII names today
            resource_type: [
                resource_type[0],
                0,
                resource_type[1],
                0,
                resource_type[2],
                0,
                resource_type[3],
                0,
            ],
        }
    }
}
