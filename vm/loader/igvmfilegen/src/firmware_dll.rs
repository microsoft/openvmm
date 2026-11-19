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

#[derive(Clone, Copy, Debug, Default, clap::Args)]
pub(crate) struct ResourceSelector {
    /// Numeric VMFW resource ID. Required when a DLL has multiple firmware images.
    #[clap(long)]
    pub resource_id: Option<u16>,
    /// Numeric resource language. Required when the selected resource has multiple languages.
    #[clap(long, requires = "resource_id")]
    pub resource_language: Option<u16>,
}

/// Read an IGVM image from `path`, transparently extracting it from a
/// `vmfirmwareigvm` resource DLL when `path` points at a PE/DLL rather than a
/// raw IGVM file.
///
/// A resource DLL is a PE image beginning with the `MZ` signature; a raw IGVM
/// file does not. When a PE is detected, the embedded `VMFW` resource is
/// returned; otherwise the file bytes are returned unchanged.
pub fn read_igvm_image(path: &Path, selector: ResourceSelector) -> anyhow::Result<Vec<u8>> {
    let bytes = fs_err::read(path).context("reading input file")?;
    if bytes.starts_with(b"MZ") {
        extract_vmfw_resource(&bytes, selector).with_context(|| {
            format!(
                "extracting embedded IGVM ({VMFW_RESOURCE_TYPE} resource) from resource DLL {}",
                path.display()
            )
        })
    } else {
        anyhow::ensure!(
            selector.resource_id.is_none() && selector.resource_language.is_none(),
            "resource selection requires a firmware DLL, not a raw IGVM"
        );
        Ok(bytes)
    }
}

/// Extract the embedded IGVM payload (the `VMFW` resource) from a PE/DLL image.
fn extract_vmfw_resource(pe_bytes: &[u8], selector: ResourceSelector) -> anyhow::Result<Vec<u8>> {
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
    // Never silently select a different image or language in a multi-resource DLL.
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
        let id_entry = if let Some(id) = selector.resource_id {
            id_table
                .entries
                .iter()
                .find(|entry| {
                    matches!(entry.name_or_id(),
                        ResourceNameOrId::Id(value) if value == id
                    )
                })
                .context("requested VMFW resource ID not found")?
        } else {
            anyhow::ensure!(
                id_table.entries.len() == 1,
                "expected one VMFW resource; specify --resource-id for a multi-image DLL"
            );
            &id_table.entries[0]
        };

        let ResourceDirectoryEntryData::Table(lang_table) = id_entry
            .data(dir)
            .context("reading VMFW resource language table")?
        else {
            anyhow::bail!("VMFW resource id entry is not a subdirectory");
        };
        let lang_entry = if let Some(language) = selector.resource_language {
            lang_table
                .entries
                .iter()
                .find(|entry| {
                    matches!(entry.name_or_id(),
                        ResourceNameOrId::Id(value) if value == language
                    )
                })
                .context("requested VMFW resource language not found")?
        } else {
            anyhow::ensure!(
                lang_table.entries.len() == 1,
                "expected one VMFW resource language; specify --resource-language"
            );
            &lang_table.entries[0]
        };

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
    use super::ResourceSelector;
    use super::extract_vmfw_resource;
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

    fn resource_dll(resources: &[(u16, u16, &[u8])]) -> Vec<u8> {
        fn put16(bytes: &mut [u8], offset: usize, value: u16) {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        fn put32(bytes: &mut [u8], offset: usize, value: usize) {
            bytes[offset..offset + 4].copy_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
        }
        fn allocate(bytes: &mut Vec<u8>, size: usize) -> usize {
            let offset = bytes.len();
            bytes.resize(offset + size, 0);
            offset
        }
        let mut ids = std::collections::BTreeMap::<u16, Vec<(u16, &[u8])>>::new();
        for &(id, language, payload) in resources {
            ids.entry(id).or_default().push((language, payload));
        }
        let mut data = vec![0; 36];
        put16(&mut data, 12, 1);
        put32(&mut data, 16, 0x8000_0000 | 24);
        put16(&mut data, 24, 4);
        data[26..34].copy_from_slice(&utf16le("VMFW"));
        let id_table = allocate(&mut data, 16 + 8 * ids.len());
        put32(&mut data, 20, 0x8000_0000 | id_table);
        put16(&mut data, id_table + 14, ids.len().try_into().unwrap());
        for (i, (id, languages)) in ids.iter().enumerate() {
            let lang_table = allocate(&mut data, 16 + 8 * languages.len());
            put32(&mut data, id_table + 16 + 8 * i, usize::from(*id));
            put32(&mut data, id_table + 20 + 8 * i, 0x8000_0000 | lang_table);
            put16(
                &mut data,
                lang_table + 14,
                languages.len().try_into().unwrap(),
            );
            for (j, (language, payload)) in languages.iter().enumerate() {
                let entry = allocate(&mut data, 16);
                let payload_offset = allocate(&mut data, payload.len());
                put32(&mut data, lang_table + 16 + 8 * j, usize::from(*language));
                put32(&mut data, lang_table + 20 + 8 * j, entry);
                put32(&mut data, entry, 0x1000 + payload_offset);
                put32(&mut data, entry + 4, payload.len());
                data[payload_offset..payload_offset + payload.len()].copy_from_slice(payload);
                while !data.len().is_multiple_of(4) {
                    data.push(0);
                }
            }
        }
        let mut pe = vec![0; 512];
        pe[..2].copy_from_slice(b"MZ");
        put32(&mut pe, 0x3c, 0x80);
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        put16(&mut pe, 0x84, 0x8664);
        put16(&mut pe, 0x86, 1);
        put16(&mut pe, 0x94, 240);
        put16(&mut pe, 0x98, 0x20b);
        put32(&mut pe, 0x98 + 32, 0x1000);
        put32(&mut pe, 0x98 + 36, 512);
        put32(&mut pe, 0x98 + 56, 0x2000);
        put32(&mut pe, 0x98 + 60, 512);
        put32(&mut pe, 0x98 + 108, 16);
        put32(&mut pe, 0x98 + 128, 0x1000);
        put32(&mut pe, 0x98 + 132, data.len());
        pe[0x188..0x190].copy_from_slice(b".rsrc\0\0\0");
        put32(&mut pe, 0x188 + 8, data.len());
        put32(&mut pe, 0x188 + 12, 0x1000);
        put32(&mut pe, 0x188 + 16, data.len());
        put32(&mut pe, 0x188 + 20, 512);
        pe.extend(data);
        pe
    }

    #[test]
    fn selects_exact_resource_and_language_without_ambiguous_fallback() {
        let bytes = resource_dll(&[
            (1, 1033, b"first"),
            (2, 1033, b"second"),
            (2, 1041, b"localized"),
        ]);
        let select = |id, language| {
            extract_vmfw_resource(
                &bytes,
                ResourceSelector {
                    resource_id: id,
                    resource_language: language,
                },
            )
        };
        assert!(select(None, None).is_err());
        assert_eq!(select(Some(1), None).unwrap(), b"first");
        assert!(select(Some(2), None).is_err());
        assert_eq!(select(Some(2), Some(1033)).unwrap(), b"second");
        assert_eq!(select(Some(2), Some(1041)).unwrap(), b"localized");
        assert!(select(Some(3), None).is_err());
        assert!(select(Some(1), Some(1041)).is_err());
        assert_eq!(
            extract_vmfw_resource(
                &resource_dll(&[(1, 1033, b"only")]),
                ResourceSelector::default()
            )
            .unwrap(),
            b"only"
        );
        assert!(extract_vmfw_resource(b"not a PE", ResourceSelector::default()).is_err());
    }

    #[test]
    fn rejects_resource_selectors_on_raw_images() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs_err::write(file.path(), b"raw").unwrap();
        assert_eq!(
            super::read_igvm_image(file.path(), ResourceSelector::default()).unwrap(),
            b"raw"
        );
        assert!(
            super::read_igvm_image(
                file.path(),
                ResourceSelector {
                    resource_id: Some(1),
                    resource_language: None,
                }
            )
            .is_err()
        );
    }
}
