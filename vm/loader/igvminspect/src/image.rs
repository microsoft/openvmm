// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared input handling for raw IGVM files and firmware resource DLLs.

use anyhow::Context;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

/// Read an IGVM image, unwrapping the `VMFW` resource with ID 1 when needed.
pub(crate) fn read_igvm_image(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut file = fs_err::File::open(path).context("opening input file")?;
    let file_len = file.metadata().context("reading input file size")?.len();
    let mut magic = [0; 2];
    file.read_exact(&mut magic)
        .context("reading input file signature")?;

    let (offset, len) = if magic == *b"MZ" {
        let descriptor = resource_dll_parser::DllResourceDescriptor::new(b"VMFW", 1);
        let (offset, len) = resource_dll_parser::try_find_resource_from_dll(&file, &descriptor)
            .with_context(|| format!("locating VMFW resource with ID 1 in {}", path.display()))?
            .context("input is not a valid 64-bit firmware resource DLL")?;
        (
            offset,
            u64::try_from(len).context("resource size overflow")?,
        )
    } else {
        (0, file_len)
    };

    anyhow::ensure!(
        offset.checked_add(len).is_some_and(|end| end <= file_len),
        "IGVM resource range exceeds input file size"
    );
    file.seek(SeekFrom::Start(offset))
        .context("seeking to IGVM image")?;
    // Grow with the bytes actually read, not an untrusted resource size.
    let mut image = Vec::new();
    file.take(len)
        .read_to_end(&mut image)
        .context("reading IGVM image")?;
    anyhow::ensure!(
        image.len() as u64 == len,
        "input file was truncated while reading IGVM image"
    );
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::read_igvm_image;
    use crate::dump_igvm_file;
    use igvm::IgvmDirectiveHeader;
    use igvm::IgvmFile;
    use igvm::IgvmInitializationHeader;
    use igvm::IgvmPlatformHeader;
    use igvm::IgvmRevision;
    use igvm_defs::IGVM_FIXED_HEADER;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use igvm_defs::IgvmPlatformType;
    use std::io::Write;
    use test_with_tracing::test;
    use zerocopy::FromBytes;

    fn igvm_image(initializations: Vec<IgvmInitializationHeader>) -> Vec<u8> {
        let igvm = IgvmFile::new(
            IgvmRevision::V1,
            vec![IgvmPlatformHeader::SupportedPlatform(
                IGVM_VHS_SUPPORTED_PLATFORM {
                    compatibility_mask: 1,
                    highest_vtl: 0,
                    platform_type: IgvmPlatformType::VSM_ISOLATION,
                    platform_version: 1,
                    shared_gpa_boundary: 0,
                },
            )],
            initializations,
            vec![IgvmDirectiveHeader::PageData {
                gpa: 0x1000,
                compatibility_mask: 1,
                flags: IgvmPageDataFlags::new(),
                data_type: IgvmPageDataType::NORMAL,
                data: vec![0x55; 4096],
            }],
        )
        .unwrap();
        let mut image = Vec::new();
        igvm.serialize(&mut image).unwrap();
        image
    }

    fn input_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    const RESOURCE_OFFSET: usize = 0x200;
    const RESOURCE_RVA: u32 = 0x1000;
    const PAYLOAD_OFFSET: usize = 104;
    const DATA_ENTRY_OFFSET: usize = 72;

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Build a PE32+ resource-only DLL with the same VMFW/1 resource tree as
    /// openhcl/vmfirmwareigvm_dll/resources.rc, without a Windows toolchain.
    fn firmware_dll(payload: &[u8]) -> Vec<u8> {
        let resource_len = PAYLOAD_OFFSET + payload.len();
        let raw_size = resource_len.next_multiple_of(0x200);
        let mut dll = vec![0; RESOURCE_OFFSET + raw_size];

        dll[..2].copy_from_slice(b"MZ");
        put_u32(&mut dll, 0x3c, 0x80); // DOS e_lfanew
        dll[0x80..0x84].copy_from_slice(b"PE\0\0");
        put_u16(&mut dll, 0x84, 0x8664); // AMD64
        put_u16(&mut dll, 0x86, 1); // One section
        put_u16(&mut dll, 0x94, 240); // Optional header size
        put_u16(&mut dll, 0x96, 0x2022); // Executable, large-address-aware DLL

        let optional = 0x98;
        put_u16(&mut dll, optional, 0x20b); // PE32+
        put_u32(&mut dll, optional + 32, 0x1000); // Section alignment
        put_u32(&mut dll, optional + 36, 0x200); // File alignment
        put_u32(
            &mut dll,
            optional + 56,
            RESOURCE_RVA + resource_len.next_multiple_of(0x1000) as u32,
        ); // Image size
        put_u32(&mut dll, optional + 60, RESOURCE_OFFSET as u32);
        put_u16(&mut dll, optional + 68, 3); // Console subsystem
        put_u32(&mut dll, optional + 108, 16); // Data directory count
        put_u32(&mut dll, optional + 128, RESOURCE_RVA);
        put_u32(&mut dll, optional + 132, resource_len as u32);

        let section = optional + 240;
        dll[section..section + 5].copy_from_slice(b".rsrc");
        put_u32(&mut dll, section + 8, resource_len as u32);
        put_u32(&mut dll, section + 12, RESOURCE_RVA);
        put_u32(&mut dll, section + 16, raw_size as u32);
        put_u32(&mut dll, section + 20, RESOURCE_OFFSET as u32);
        put_u32(&mut dll, section + 36, 0x40000040); // Readable initialized data

        let resource = &mut dll[RESOURCE_OFFSET..];
        // Directory headers and entries: root -> VMFW -> ID 1 -> language.
        put_u16(resource, 12, 1); // One named root entry
        put_u32(resource, 16, 0x80000000 | 88); // VMFW string
        put_u32(resource, 20, 0x80000000 | 24); // Type directory
        put_u16(resource, 24 + 14, 1); // One ID entry
        put_u32(resource, 40, 1); // Resource ID
        put_u32(resource, 44, 0x80000000 | 48); // Language directory
        put_u16(resource, 48 + 14, 1); // One language
        put_u32(resource, 64, 0x409); // en-US
        put_u32(resource, 68, DATA_ENTRY_OFFSET as u32);
        put_u32(
            resource,
            DATA_ENTRY_OFFSET,
            RESOURCE_RVA + PAYLOAD_OFFSET as u32,
        );
        put_u32(resource, DATA_ENTRY_OFFSET + 4, payload.len() as u32);
        put_u16(resource, 88, 4); // UTF-16 resource type length
        resource[90..98].copy_from_slice(b"V\0M\0F\0W\0");
        resource[PAYLOAD_OFFSET..PAYLOAD_OFFSET + payload.len()].copy_from_slice(payload);
        dll
    }

    #[test]
    fn reads_and_dumps_raw_igvm() {
        let image = igvm_image(vec![]);
        let file = input_file(&image);
        assert_eq!(read_igvm_image(file.path()).unwrap(), image);

        let mut output = Vec::new();
        dump_igvm_file(file.path(), &mut output).unwrap();
        let (header, _) = IGVM_FIXED_HEADER::read_from_prefix(image.as_slice()).unwrap();
        let parsed = IgvmFile::new_from_binary(&image, None).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "Total file size: {} bytes\n\n{:#X?}\n{}\n",
                header.total_file_size, header, parsed
            )
        );
    }

    #[test]
    fn firmware_dll_matches_raw_dump() {
        let image = igvm_image(vec![]);
        let raw = input_file(&image);
        let dll = input_file(&firmware_dll(&image));
        assert_eq!(read_igvm_image(dll.path()).unwrap(), image);

        let mut raw_output = Vec::new();
        let mut dll_output = Vec::new();
        dump_igvm_file(raw.path(), &mut raw_output).unwrap();
        dump_igvm_file(dll.path(), &mut dll_output).unwrap();
        assert_eq!(dll_output, raw_output);
    }

    #[test]
    fn extracts_firmware_dll() {
        let image = igvm_image(vec![]);
        let dll = input_file(&firmware_dll(&image));
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        crate::extract::extract_igvm_file(dll.path(), None, &output).unwrap();
        assert!(output.join("headers/platforms.txt").exists());
        assert_eq!(
            fs_err::read(output.join("regions/0000_unmapped.bin")).unwrap(),
            vec![0x55; 4096]
        );
    }

    #[test]
    fn dumps_corim_document_and_signature() {
        let initializations = vec![
            IgvmInitializationHeader::CorimDocument {
                compatibility_mask: 1,
                document: vec![0xa1, 0x02, 0x03, 0x04],
            },
            IgvmInitializationHeader::CorimSignature {
                compatibility_mask: 1,
                signature: vec![0xd2, 0x84, 0x43, 0xa1],
            },
        ];
        let image = igvm_image(initializations.clone());
        for bytes in [&image, &firmware_dll(&image)] {
            let file = input_file(bytes);
            let bytes = read_igvm_image(file.path()).unwrap();
            let parsed = IgvmFile::new_from_binary(&bytes, None).unwrap();
            assert_eq!(parsed.initializations(), initializations);
            let mut output = Vec::new();
            dump_igvm_file(file.path(), &mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("CorimDocument"));
            assert!(output.contains("CorimSignature"));
        }
    }

    #[test]
    fn rejects_malformed_igvm_without_output() {
        for bytes in [b"".as_slice(), b"I", b"IGVM", &[0; 64]] {
            let file = input_file(bytes);
            let mut output = Vec::new();
            assert!(dump_igvm_file(file.path(), &mut output).is_err());
            assert!(output.is_empty());
        }
    }

    #[test]
    fn preserves_igvm_parse_errors_with_input_path() {
        let file = input_file(&[0; 64]);
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("output");
        let mut output = Vec::new();
        let dump_error = dump_igvm_file(file.path(), &mut output).unwrap_err();
        let extract_error =
            crate::extract::extract_igvm_file(file.path(), None, &output_dir).unwrap_err();

        for error in [dump_error, extract_error] {
            assert_eq!(
                error.to_string(),
                format!("parsing IGVM file {}", file.path().display())
            );
            assert!(matches!(
                error.downcast_ref::<igvm::Error>(),
                Some(igvm::Error::InvalidFixedHeader)
            ));
        }
        assert!(output.is_empty());
        assert!(!output_dir.exists());
    }

    #[test]
    fn rejects_malformed_dll() {
        let file = input_file(b"MZnot a PE image");
        assert!(read_igvm_image(file.path()).is_err());
    }

    #[test]
    fn rejects_missing_vmfw_resource() {
        let mut dll = firmware_dll(&igvm_image(vec![]));
        dll[RESOURCE_OFFSET + 90] = b'X';
        let file = input_file(&dll);
        let error = read_igvm_image(file.path()).unwrap_err();
        assert!(format!("{error:#}").contains("no entry for resource type"));
    }

    #[test]
    fn rejects_wrong_resource_id() {
        let mut dll = firmware_dll(&igvm_image(vec![]));
        put_u32(&mut dll, RESOURCE_OFFSET + 40, 2);
        let file = input_file(&dll);
        let error = read_igvm_image(file.path()).unwrap_err();
        assert!(format!("{error:#}").contains("no entry for id"));
    }

    #[test]
    fn rejects_unmapped_resource_offset() {
        let mut dll = firmware_dll(&igvm_image(vec![]));
        put_u32(&mut dll, RESOURCE_OFFSET + DATA_ENTRY_OFFSET, u32::MAX);
        let file = input_file(&dll);
        assert!(read_igvm_image(file.path()).is_err());
    }

    #[test]
    fn rejects_oversized_resource_before_allocation() {
        let mut dll = firmware_dll(&igvm_image(vec![]));
        put_u32(&mut dll, RESOURCE_OFFSET + DATA_ENTRY_OFFSET + 4, u32::MAX);
        let file = input_file(&dll);
        let error = read_igvm_image(file.path()).unwrap_err();
        assert!(error.to_string().contains("range exceeds input file size"));
    }

    #[test]
    fn rejects_truncated_dll() {
        let image = igvm_image(vec![]);
        let mut dll = firmware_dll(&image);
        dll.truncate(RESOURCE_OFFSET + PAYLOAD_OFFSET + image.len() - 1);
        let file = input_file(&dll);
        assert!(read_igvm_image(file.path()).is_err());
    }

    #[test]
    fn reports_dump_write_errors() {
        let file = input_file(&igvm_image(vec![]));
        let mut buffer = [];
        let error = dump_igvm_file(file.path(), buffer.as_mut_slice()).unwrap_err();
        assert!(error.to_string().contains("writing IGVM dump"));
    }
}
