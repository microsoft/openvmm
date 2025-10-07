// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Function to parse a resource dll for a given ID.

use crate::DllResourceDescriptor;
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
pub(crate) fn try_find_resource_from_dll(
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
