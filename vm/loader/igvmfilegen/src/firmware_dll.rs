// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Read an IGVM image from either a raw IGVM file or a `vmfirmwareigvm`
//! resource DLL.
//!
//! Production OpenHCL IGVM files are shipped encapsulated in a Windows
//! resource-only DLL (`vmfirmwareigvm.dll` / `vmfirmwarecvm.dll`), where the
//! IGVM payload is stored as a custom `VMFW` resource with id `1` -- see the
//! `1 VMFW <igvm>` entry in `openhcl/vmfirmwareigvm_dll/resources.rc`. To let
//! `dump` / `dump-corim` operate directly on those shipped DLLs (e.g. to
//! confirm a CoRIM is present in the packaged firmware), this module detects a
//! PE input and extracts the embedded IGVM from it.

use anyhow::Context;
use std::path::Path;

/// Custom PE resource *type* name under which the IGVM payload is stored in a
/// `vmfirmwareigvm` resource DLL (the `1 VMFW <igvm>` entry in `resources.rc`).
const VMFW_RESOURCE_TYPE: &str = "VMFW";

/// Read an IGVM image from `path`, transparently extracting it from a
/// `vmfirmwareigvm` resource DLL when `path` points at a PE/DLL rather than a
/// raw IGVM file.
///
/// A resource DLL is a PE image beginning with the `MZ` signature; a raw IGVM
/// file does not. When a PE is detected, the embedded `VMFW` resource is
/// returned; otherwise the file bytes are returned unchanged.
pub fn read_igvm_image(path: &Path) -> anyhow::Result<Vec<u8>> {
    let bytes = fs_err::read(path).context("reading input file")?;
    if bytes.starts_with(b"MZ") {
        extract_vmfw_resource(&bytes).with_context(|| {
            format!(
                "extracting embedded IGVM ({VMFW_RESOURCE_TYPE} resource) from resource DLL {}",
                path.display()
            )
        })
    } else {
        Ok(bytes)
    }
}

/// Extract the embedded IGVM payload (the `VMFW` resource) from a PE/DLL image.
fn extract_vmfw_resource(pe_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use object::read::pe::PeFile64;
    use object::read::pe::ResourceDirectoryEntryData;
    use object::read::pe::ResourceNameOrId;

    let pe =
        PeFile64::parse(pe_bytes).context("parsing PE image (expected a 64-bit resource DLL)")?;
    let sections = pe.section_table();
    let dir = pe
        .data_directories()
        .resource_directory(pe_bytes, &sections)
        .context("reading resource directory")?
        .context("PE image has no resource directory")?;
    let root = dir.root().context("reading resource directory root")?;

    // The resource tree has three levels: type -> id/name -> language -> data.
    // Locate the custom `VMFW` resource type, then descend to the first (and
    // only) id and language entry to reach the IGVM payload.
    for type_entry in root.entries {
        let ResourceNameOrId::Name(name) = type_entry.name_or_id() else {
            // The IGVM is stored under a named type (`VMFW`), not a numeric one.
            continue;
        };
        let raw_name = name.raw_data(dir).context("reading resource type name")?;
        if !utf16le_eq(raw_name, VMFW_RESOURCE_TYPE) {
            continue;
        }

        let ResourceDirectoryEntryData::Table(id_table) = type_entry
            .data(dir)
            .context("reading VMFW resource id table")?
        else {
            anyhow::bail!("VMFW resource type entry is not a subdirectory");
        };
        let id_entry = id_table
            .entries
            .first()
            .context("VMFW resource type has no entries")?;

        let ResourceDirectoryEntryData::Table(lang_table) = id_entry
            .data(dir)
            .context("reading VMFW resource language table")?
        else {
            anyhow::bail!("VMFW resource id entry is not a subdirectory");
        };
        let lang_entry = lang_table
            .entries
            .first()
            .context("VMFW resource has no language entries")?;

        let ResourceDirectoryEntryData::Data(data) = lang_entry
            .data(dir)
            .context("reading VMFW resource data entry")?
        else {
            anyhow::bail!("VMFW resource language entry is not a data entry");
        };

        // The data entry references the payload by RVA + size; resolve it via
        // the section table and take exactly `size` bytes.
        let rva = data.offset_to_data.get(object::LittleEndian);
        let size = data.size.get(object::LittleEndian) as usize;
        let payload = sections
            .pe_data_at(pe_bytes, rva)
            .context("resolving VMFW resource RVA in section table")?
            .get(..size)
            .context("VMFW resource size extends past its section")?;
        return Ok(payload.to_vec());
    }

    anyhow::bail!(
        "no '{VMFW_RESOURCE_TYPE}' resource found; input does not look like a vmfirmwareigvm resource DLL"
    )
}

/// Compare a little-endian UTF-16 PE resource name to an ASCII string.
fn utf16le_eq(utf16le: &[u8], ascii: &str) -> bool {
    if utf16le.len() != ascii.len() * 2 {
        return false;
    }
    utf16le
        .chunks_exact(2)
        .zip(ascii.chars())
        .all(|(pair, c)| u16::from_le_bytes([pair[0], pair[1]]) == c as u16)
}

#[cfg(test)]
mod tests {
    use super::utf16le_eq;

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn utf16le_eq_matches() {
        assert!(utf16le_eq(&utf16le("VMFW"), "VMFW"));
    }

    #[test]
    fn utf16le_eq_rejects_mismatch_and_length() {
        assert!(!utf16le_eq(&utf16le("VMFX"), "VMFW"));
        assert!(!utf16le_eq(&utf16le("VMF"), "VMFW"));
        assert!(!utf16le_eq(&utf16le("VMFWX"), "VMFW"));
        // Odd-length (truncated) UTF-16 buffers never match.
        assert!(!utf16le_eq(&[0x56, 0x00, 0x4d], "VM"));
    }
}

