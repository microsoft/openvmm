// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the helper functions for accessing VMGS entries.

use crate::hardware_key_sealing::HwKeyProtector;
use guid::Guid;
use openhcl_attestation_protocol::vmgs::AGENT_DATA_MAX_SIZE;
use openhcl_attestation_protocol::vmgs::GUEST_SECRET_KEY_MAX_SIZE;
use openhcl_attestation_protocol::vmgs::GuestSecretKey;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtector;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV3;
use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorV4;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use openhcl_attestation_protocol::vmgs::KeyProtectorById;
use openhcl_attestation_protocol::vmgs::SecurityProfile;
use thiserror::Error;
use vmgs::FileId;
use vmgs::Vmgs;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub(crate) enum ReadFromVmgsError {
    #[error("failed to read {file_id:?} from vmgs")]
    ReadFromVmgs {
        #[source]
        vmgs_err: vmgs::Error,
        file_id: FileId,
    },
    #[error("invalid data format, file id: {0:?}")]
    InvalidFormat(FileId),
    #[error("entry does not exist, file id: {0:?}")]
    EntryNotFound(FileId),
    #[error("{file_id:?} valid bytes {size} smaller than the minimal size {minimal_size}")]
    EntrySizeTooSmall {
        file_id: FileId,
        size: usize,
        minimal_size: usize,
    },
    #[error("{file_id:?} valid bytes {size} larger than the maximum size {maximum_size}")]
    EntrySizeTooLarge {
        file_id: FileId,
        size: usize,
        maximum_size: usize,
    },
    #[error("{file_id:?} valid bytes {size}, expected {expected_size}")]
    EntrySizeUnexpected {
        file_id: FileId,
        size: usize,
        expected_size: usize,
    },
}

/// Error while writing to vmgs
#[derive(Debug, Error)]
pub(crate) enum WriteToVmgsError {
    #[error("failed to write {file_id:?} to vmgs")]
    WriteToVmgs {
        #[source]
        vmgs_err: vmgs::Error,
        file_id: FileId,
    },
    #[error("invalid data format, file id: {0:?}")]
    InvalidFormat(FileId),
}

/// Read Key Protector data from the VMGS file. If [`FileId::KEY_PROTECTOR`] doesn't exist yet,
/// locally initialize a key_protector instance that can be written to.
pub async fn read_key_protector(
    vmgs: &mut Vmgs,
    dek_minimal_size: usize,
) -> Result<KeyProtector, ReadFromVmgsError> {
    use openhcl_attestation_protocol::vmgs::KEY_PROTECTOR_SIZE;

    let file_id = FileId::KEY_PROTECTOR;
    match vmgs.read_file(file_id).await {
        Ok(data) => {
            if data.len() < dek_minimal_size {
                Err(ReadFromVmgsError::EntrySizeTooSmall {
                    file_id,
                    size: data.len(),
                    minimal_size: dek_minimal_size,
                })?
            }

            if data.len() > KEY_PROTECTOR_SIZE {
                Err(ReadFromVmgsError::EntrySizeTooLarge {
                    file_id,
                    size: data.len(),
                    maximum_size: KEY_PROTECTOR_SIZE,
                })?
            }

            let data = if data.len() < KEY_PROTECTOR_SIZE {
                // Allow smaller buf by padding zero bytes
                let mut data = data;
                data.resize(KEY_PROTECTOR_SIZE, 0);
                data
            } else {
                data
            };

            // read_from_prefix expects input bytes to be larger than or equal to size_of::<Self>()
            KeyProtector::read_from_prefix(&data[..])
                .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))
                .map(|k| k.0) // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        }
        Err(vmgs::Error::FileInfoNotAllocated(_)) => Ok(KeyProtector::new_zeroed()),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { vmgs_err, file_id }),
    }
}

/// Write Key Protector data to the VMGS file.
pub async fn write_key_protector(
    key_protector: &KeyProtector,
    vmgs: &mut Vmgs,
) -> Result<(), WriteToVmgsError> {
    let file_id = FileId::KEY_PROTECTOR;
    vmgs.write_file(file_id, key_protector.as_bytes())
        .await
        .map_err(|vmgs_err| WriteToVmgsError::WriteToVmgs { vmgs_err, file_id })
}

/// Read Key Protector ID from the VMGS file.
pub async fn read_key_protector_by_id(
    vmgs: &mut Vmgs,
) -> Result<KeyProtectorById, ReadFromVmgsError> {
    // This file could include state data following the GUID.
    // File contents vary with what paravisor previously wrote this file,
    // but a GUID must be present.
    // It is safe to write the file out with full structure, downlevel OS support this pattern.

    let file_id = FileId::VM_UNIQUE_ID;
    match vmgs.read_file(file_id).await {
        Ok(data) => match KeyProtectorById::read_from_prefix(&data[..])
            .ok() // TODO: zerocopy: ok (https://github.com/microsoft/openvmm/issues/759)
            .map(|k| k.0)
        {
            Some(key_protector_by_id) => Ok(key_protector_by_id),
            None => {
                let id_guid = Guid::read_from_prefix(&data[..])
                    .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                    .0;

                Ok(KeyProtectorById {
                    id_guid,
                    ..FromZeros::new_zeroed()
                })
            }
        },
        Err(vmgs::Error::FileInfoNotAllocated(_)) => Err(ReadFromVmgsError::EntryNotFound(file_id)),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { vmgs_err, file_id }),
    }
}

/// Write Key Protector Id (current Id) to the VMGS file.
///
/// Write if `bios_guid` is different from the one held in `key_protector_by_id` (which
/// will be set to `bios_guid` before write) or `force_write` is `true`.
pub async fn write_key_protector_by_id(
    key_protector_by_id: &mut KeyProtectorById,
    vmgs: &mut Vmgs,
    force_write: bool,
    bios_guid: Guid,
) -> Result<(), WriteToVmgsError> {
    if force_write || bios_guid != key_protector_by_id.id_guid {
        let file_id = FileId::VM_UNIQUE_ID;
        key_protector_by_id.id_guid = bios_guid;
        vmgs.write_file(file_id, key_protector_by_id.as_bytes())
            .await
            .map_err(|vmgs_err| WriteToVmgsError::WriteToVmgs { vmgs_err, file_id })?
    }

    Ok(())
}

/// Read the security profile from the VMGS file. If [`FileId::ATTEST`] doesn't exist yet,
/// return an empty vector.
pub async fn read_security_profile(vmgs: &mut Vmgs) -> Result<SecurityProfile, ReadFromVmgsError> {
    let file_id = FileId::ATTEST;
    match vmgs.read_file(file_id).await {
        Ok(data) => {
            if data.len() > AGENT_DATA_MAX_SIZE {
                Err(ReadFromVmgsError::EntrySizeTooLarge {
                    file_id,
                    size: data.len(),
                    maximum_size: AGENT_DATA_MAX_SIZE,
                })?
            }

            let data = if data.len() < AGENT_DATA_MAX_SIZE {
                // Allow smaller buf by padding zero bytes
                let mut data = data;
                data.resize(AGENT_DATA_MAX_SIZE, 0);
                data
            } else {
                data
            };

            // read_from_prefix expects input bytes to be larger than or equal to size_of::<Self>()
            Ok(SecurityProfile::read_from_prefix(&data[..])
                .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?
                .0) // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        }
        Err(vmgs::Error::FileInfoNotAllocated(_)) => Ok(SecurityProfile::new_zeroed()),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { file_id, vmgs_err })?,
    }
}

/// Read the hardware key protector from the VMGS file.
pub async fn read_hardware_key_protector(
    vmgs: &mut Vmgs,
) -> Result<HwKeyProtector, ReadFromVmgsError> {
    let file_id = FileId::HW_KEY_PROTECTOR;
    let data = match vmgs.read_file(file_id).await {
        Ok(data) => data,
        Err(vmgs::Error::FileInfoNotAllocated(_)) => {
            return Err(ReadFromVmgsError::EntryNotFound(file_id));
        }
        Err(vmgs_err) => {
            return Err(ReadFromVmgsError::ReadFromVmgs { vmgs_err, file_id });
        }
    };

    parse_hardware_key_protector(&data)
}

fn parse_hardware_key_protector(data: &[u8]) -> Result<HwKeyProtector, ReadFromVmgsError> {
    use openhcl_attestation_protocol::vmgs as format;

    let file_id = FileId::HW_KEY_PROTECTOR;
    let (version, _) =
        u32::read_from_prefix(data).map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?;
    let expected_size = match version {
        format::HW_KEY_PROTECTOR_VERSION_1 | format::HW_KEY_PROTECTOR_VERSION_2 => {
            format::HW_KEY_PROTECTOR_SIZE
        }
        format::HW_KEY_PROTECTOR_VERSION_3 => format::HW_KEY_PROTECTOR_V3_SIZE,
        format::HW_KEY_PROTECTOR_VERSION_4 => format::HW_KEY_PROTECTOR_V4_SIZE,
        _ => return Err(ReadFromVmgsError::InvalidFormat(file_id)),
    };
    if data.len() != expected_size {
        return Err(ReadFromVmgsError::EntrySizeUnexpected {
            file_id,
            size: data.len(),
            expected_size,
        });
    }
    let protector = match version {
        format::HW_KEY_PROTECTOR_VERSION_1 | format::HW_KEY_PROTECTOR_VERSION_2 => {
            HardwareKeyProtector::read_from_bytes(data)
                .map(HwKeyProtector::Legacy)
                .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?
        }
        format::HW_KEY_PROTECTOR_VERSION_3 => HardwareKeyProtectorV3::read_from_bytes(data)
            .map(HwKeyProtector::V3)
            .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?,
        format::HW_KEY_PROTECTOR_VERSION_4 => HardwareKeyProtectorV4::read_from_bytes(data)
            .map(HwKeyProtector::V4)
            .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?,
        _ => return Err(ReadFromVmgsError::InvalidFormat(file_id)),
    };
    if !protector.has_valid_header() {
        return Err(ReadFromVmgsError::InvalidFormat(file_id));
    }
    Ok(protector)
}

/// Validate and write the hardware key protector to the VMGS file.
pub async fn write_hardware_key_protector(
    hardware_key_protector: &HwKeyProtector,
    vmgs: &mut Vmgs,
) -> Result<(), WriteToVmgsError> {
    let file_id = FileId::HW_KEY_PROTECTOR;
    if !hardware_key_protector.has_valid_header() {
        return Err(WriteToVmgsError::InvalidFormat(file_id));
    }
    vmgs.write_file(file_id, hardware_key_protector.as_bytes())
        .await
        .map_err(|vmgs_err| WriteToVmgsError::WriteToVmgs { vmgs_err, file_id })
}

/// Read the guest secret key from VMGS file.
pub async fn read_guest_secret_key(vmgs: &mut Vmgs) -> Result<GuestSecretKey, ReadFromVmgsError> {
    let file_id = FileId::GUEST_SECRET_KEY;
    match vmgs.read_file(file_id).await {
        Ok(data) => {
            if data.len() > GUEST_SECRET_KEY_MAX_SIZE {
                Err(ReadFromVmgsError::EntrySizeTooLarge {
                    file_id,
                    size: data.len(),
                    maximum_size: GUEST_SECRET_KEY_MAX_SIZE,
                })?
            }

            let data = if data.len() < GUEST_SECRET_KEY_MAX_SIZE {
                // Allow smaller buf by padding zero bytes
                let mut data = data;
                data.resize(GUEST_SECRET_KEY_MAX_SIZE, 0);
                data
            } else {
                data
            };

            // read_from_prefix expects input bytes to be larger than or equal to size_of::<Self>()
            Ok(GuestSecretKey::read_from_prefix(&data[..])
                .map_err(|_| ReadFromVmgsError::InvalidFormat(file_id))?
                .0) // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        }
        Err(vmgs::Error::FileInfoNotAllocated(_)) => Err(ReadFromVmgsError::EntryNotFound(file_id)),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { file_id, vmgs_err }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_backend::Disk;
    use disklayer_ram::ram_disk;
    use openhcl_attestation_protocol::vmgs::AES_CBC_IV_LENGTH;
    use openhcl_attestation_protocol::vmgs::AES_GCM_KEY_LENGTH;
    use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;
    use openhcl_attestation_protocol::vmgs::DekKp;
    use openhcl_attestation_protocol::vmgs::GSP_BUFFER_SIZE;
    use openhcl_attestation_protocol::vmgs::GspKp;
    use openhcl_attestation_protocol::vmgs::HMAC_SHA_256_KEY_LENGTH;
    use openhcl_attestation_protocol::vmgs::HW_KEY_PROTECTOR_SVN_SIZE;
    use openhcl_attestation_protocol::vmgs::HW_KEY_PROTECTOR_TEE_TYPE_SNP;
    use openhcl_attestation_protocol::vmgs::HW_KEY_PROTECTOR_V3_SIZE;
    use openhcl_attestation_protocol::vmgs::HardwareKeyProtectorHeaderV3;
    use openhcl_attestation_protocol::vmgs::KEY_PROTECTOR_SIZE;
    use openhcl_attestation_protocol::vmgs::KeyProtector;
    use openhcl_attestation_protocol::vmgs::KeyProtectorById;
    use openhcl_attestation_protocol::vmgs::NUMBER_KP;
    use pal_async::async_test;
    use test_with_tracing::test;

    const ONE_MEGA_BYTE: u64 = 1024 * 1024;

    fn new_test_file() -> Disk {
        ram_disk(4 * ONE_MEGA_BYTE, false).unwrap()
    }

    async fn new_formatted_vmgs() -> Vmgs {
        let disk = new_test_file();

        Vmgs::format_new(disk, None).await.unwrap()
    }

    fn new_hardware_key_protector() -> HwKeyProtector {
        let mut svn = [0; HW_KEY_PROTECTOR_SVN_SIZE];
        svn[..8].copy_from_slice(&2u64.to_le_bytes());
        let header = HardwareKeyProtectorHeaderV3::new(
            HW_KEY_PROTECTOR_V3_SIZE as u32,
            HW_KEY_PROTECTOR_TEE_TYPE_SNP,
            svn,
            1,
        );
        let iv = [3; AES_CBC_IV_LENGTH];
        let ciphertext = [4; AES_GCM_KEY_LENGTH];
        let hmac = [5; HMAC_SHA_256_KEY_LENGTH];

        HwKeyProtector::V3(HardwareKeyProtectorV3 {
            header,
            iv,
            ciphertext,
            hmac,
        })
    }

    fn new_v4_hardware_key_protector() -> HwKeyProtector {
        let HwKeyProtector::V3(p) = new_hardware_key_protector() else {
            unreachable!();
        };
        HwKeyProtector::V4(HardwareKeyProtectorV4 {
            header: openhcl_attestation_protocol::vmgs::HardwareKeyProtectorHeaderV4::new(
                p.header.tee_type,
                p.header.svn,
                p.header.mix_measurement,
                [0x73; 32],
            ),
            iv: p.iv,
            ciphertext: p.ciphertext,
            hmac: p.hmac,
        })
    }

    #[test]
    fn hardware_key_protector_wire_sizes() {
        use openhcl_attestation_protocol::vmgs as format;

        assert_eq!(format::HW_KEY_PROTECTOR_SIZE, 104);
        assert_eq!(HW_KEY_PROTECTOR_V3_SIZE, 128);
        assert_eq!(format::HW_KEY_PROTECTOR_V4_SIZE, 160);
        assert_eq!(size_of::<format::HardwareKeyProtectorHeaderV4>(), 80);
        assert_eq!(std::mem::offset_of!(HardwareKeyProtectorV4, iv), 80);
        assert_eq!(std::mem::offset_of!(HardwareKeyProtectorV4, ciphertext), 96);
        assert_eq!(std::mem::offset_of!(HardwareKeyProtectorV4, hmac), 128);
        for p in [
            new_hardware_key_protector(),
            new_v4_hardware_key_protector(),
        ] {
            let restored = parse_hardware_key_protector(p.as_bytes()).unwrap();
            assert_eq!(restored.as_bytes(), p.as_bytes());
            assert_eq!(
                restored.key_release_context_hash(),
                p.key_release_context_hash()
            );
            for size in 0..p.as_bytes().len() {
                assert!(parse_hardware_key_protector(&p.as_bytes()[..size]).is_err());
            }
            let mut extended = p.as_bytes().to_vec();
            extended.push(0);
            assert!(parse_hardware_key_protector(&extended).is_err());
        }
    }

    #[async_test]
    async fn reject_malformed_hardware_key_protector_headers() {
        let mut vmgs = new_formatted_vmgs().await;
        // V3 and V4 have identical field offsets before the context hash.
        // Test unknown/mismatched versions, length, TEE tag, SNP padding,
        // boolean encoding, and every reserved byte on both read and write.
        for original in [
            new_hardware_key_protector(),
            new_v4_hardware_key_protector(),
        ] {
            for (offset, value) in [
                (0, 0),
                (0, 1),
                (0, 2),
                (0, 3),
                (0, 4),
                (0, 5),
                (4, 0),
                (4, 104),
                (8, 2),
                (20, 1),
                (43, 1),
                (44, 2),
                (45, 1),
                (46, 1),
                (47, 1),
            ] {
                let mut bytes = original.as_bytes().to_vec();
                if bytes[offset] == value {
                    continue;
                }
                bytes[offset] = value;
                assert!(parse_hardware_key_protector(&bytes).is_err());
                let malformed = match &original {
                    HwKeyProtector::V3(_) => {
                        HwKeyProtector::V3(HardwareKeyProtectorV3::read_from_bytes(&bytes).unwrap())
                    }
                    HwKeyProtector::V4(_) => {
                        HwKeyProtector::V4(HardwareKeyProtectorV4::read_from_bytes(&bytes).unwrap())
                    }
                    HwKeyProtector::Legacy(_) => unreachable!(),
                };
                assert!(malformed.key_derivation_policy().is_none());
                assert!(matches!(
                    write_hardware_key_protector(&malformed, &mut vmgs).await,
                    Err(WriteToVmgsError::InvalidFormat(FileId::HW_KEY_PROTECTOR))
                ));
                vmgs.write_file(FileId::HW_KEY_PROTECTOR, &bytes)
                    .await
                    .unwrap();
                assert!(read_hardware_key_protector(&mut vmgs).await.is_err());
            }
        }
    }

    #[async_test]
    async fn write_read_v4_hardware_key_protector() {
        let mut vmgs = new_formatted_vmgs().await;
        for tee_type in [
            HW_KEY_PROTECTOR_TEE_TYPE_SNP,
            openhcl_attestation_protocol::vmgs::HW_KEY_PROTECTOR_TEE_TYPE_TDX,
        ] {
            let HwKeyProtector::V4(mut p) = new_v4_hardware_key_protector() else {
                unreachable!();
            };
            p.header.tee_type = tee_type;
            if tee_type != HW_KEY_PROTECTOR_TEE_TYPE_SNP {
                p.header.svn = [0x12; 32];
            }
            let p = HwKeyProtector::V4(p);
            write_hardware_key_protector(&p, &mut vmgs).await.unwrap();
            let restored = read_hardware_key_protector(&mut vmgs).await.unwrap();
            assert_eq!(restored.as_bytes(), p.as_bytes());
            assert_eq!(restored.key_release_context_hash(), Some([0x73; 32]));
            assert!(restored.key_derivation_policy().is_some());
        }
    }

    #[test]
    fn legacy_v2_metadata_acceptance_is_unchanged() {
        use openhcl_attestation_protocol::vmgs as format;

        let mut legacy = HardwareKeyProtector::new_zeroed();
        legacy.header.version = format::HW_KEY_PROTECTOR_VERSION_2;
        // These were historically ignored (length/reserved), or interpreted
        // as nonzero == true (mix_measurement). Do not tighten V2 acceptance.
        legacy.header.length = 0;
        legacy.header._reserved = [0xff; 7];
        legacy.header.mix_measurement = 0xff;
        let parsed = parse_hardware_key_protector(legacy.as_bytes()).unwrap();
        assert_eq!(parsed.as_bytes(), legacy.as_bytes());
        assert!(parsed.key_derivation_policy().unwrap().mix_measurement);
        assert_eq!(parsed.key_release_context_hash(), None);
        legacy.header.version = format::HW_KEY_PROTECTOR_VERSION_1;
        let parsed = parse_hardware_key_protector(legacy.as_bytes()).unwrap();
        assert!(parsed.key_derivation_policy().is_none());
        legacy.header.version = 99;
        assert!(parse_hardware_key_protector(legacy.as_bytes()).is_err());
    }

    fn new_key_protector() -> KeyProtector {
        // Ingress and egress KPs are assumed to be the only two KPs, therefore `NUMBER_KP` should be 2
        assert_eq!(NUMBER_KP, 2);

        let ingress_dek = DekKp {
            dek_buffer: [1; DEK_BUFFER_SIZE],
        };
        let egress_dek = DekKp {
            dek_buffer: [2; DEK_BUFFER_SIZE],
        };
        let ingress_gsp = GspKp {
            gsp_length: GSP_BUFFER_SIZE as u32,
            gsp_buffer: [3; GSP_BUFFER_SIZE],
        };
        let egress_gsp = GspKp {
            gsp_length: GSP_BUFFER_SIZE as u32,
            gsp_buffer: [4; GSP_BUFFER_SIZE],
        };
        KeyProtector {
            dek: [ingress_dek, egress_dek],
            gsp: [ingress_gsp, egress_gsp],
            active_kp: u32::MAX,
        }
    }

    #[async_test]
    async fn write_read_vmgs_key_protector() {
        let mut vmgs = new_formatted_vmgs().await;
        let key_protector = new_key_protector();
        write_key_protector(&key_protector, &mut vmgs)
            .await
            .unwrap();

        let key_protector = read_key_protector(&mut vmgs, KEY_PROTECTOR_SIZE)
            .await
            .unwrap();

        assert!(key_protector.dek[0].dek_buffer.iter().all(|&x| x == 1));
        assert!(key_protector.dek[1].dek_buffer.iter().all(|&x| x == 2));

        assert_eq!(key_protector.gsp[0].gsp_length, GSP_BUFFER_SIZE as u32);
        assert!(key_protector.gsp[0].gsp_buffer.iter().all(|&x| x == 3));

        assert_eq!(key_protector.gsp[1].gsp_length, GSP_BUFFER_SIZE as u32);
        assert!(key_protector.gsp[1].gsp_buffer.iter().all(|&x| x == 4));

        assert_eq!(key_protector.active_kp, u32::MAX);

        // Read an undersized key protector
        let key_protector_bytes = key_protector.as_bytes();
        vmgs.write_file(
            FileId::KEY_PROTECTOR,
            &key_protector_bytes[..key_protector_bytes.len() - 1],
        )
        .await
        .unwrap();
        let found_key_protector_result =
            read_key_protector(&mut vmgs, key_protector_bytes.len()).await;
        assert!(found_key_protector_result.is_err());
        assert_eq!(
            found_key_protector_result.unwrap_err().to_string(),
            "KEY_PROTECTOR valid bytes 2059 smaller than the minimal size 2060"
        );

        // Read an oversized key protector
        vmgs.write_file(FileId::KEY_PROTECTOR, &[1; KEY_PROTECTOR_SIZE + 1])
            .await
            .unwrap();
        let found_key_protector_result = read_key_protector(&mut vmgs, KEY_PROTECTOR_SIZE).await;
        assert!(found_key_protector_result.is_err());
        assert_eq!(
            found_key_protector_result.unwrap_err().to_string(),
            "KEY_PROTECTOR valid bytes 2061 larger than the maximum size 2060"
        );

        // Read a key protector that is equal to the `dek_minimal_size` and smaller than the `KEY_PROTECTOR_SIZE`
        // so that padding is added
        vmgs.write_file(
            FileId::KEY_PROTECTOR,
            &key_protector_bytes[..(key_protector_bytes.len() - 10)],
        )
        .await
        .unwrap();
        let found_key_protector = read_key_protector(&mut vmgs, key_protector_bytes.len() - 10)
            .await
            .unwrap();
        assert_eq!(
            found_key_protector.as_bytes()[..(key_protector_bytes.len() - 10)],
            key_protector_bytes[..(key_protector_bytes.len() - 10)]
        );
        assert_eq!(
            found_key_protector.as_bytes()[key_protector_bytes.len() - 10..],
            [0; 10]
        );
    }

    #[async_test]
    async fn write_vmgs_key_protector_by_id() {
        let kp_guid = Guid::new_random();

        let mut vmgs = new_formatted_vmgs().await;
        let mut key_protector_by_id = KeyProtectorById {
            id_guid: kp_guid,
            ported: 1,
            pad: [0; 3],
        };

        // Try to read the `key_protector_by_id` from the VMGS file which doesn't have a `key_protector_by_id` entry
        let found_key_protector_by_id_result = read_key_protector_by_id(&mut vmgs).await;
        assert!(found_key_protector_by_id_result.is_err());
        assert_eq!(
            found_key_protector_by_id_result.unwrap_err().to_string(),
            "entry does not exist, file id: VM_UNIQUE_ID"
        );

        // Populate the VMGS file with `key_protector_by_id`
        write_key_protector_by_id(&mut key_protector_by_id, &mut vmgs, true, kp_guid)
            .await
            .unwrap();

        // Without using force, write the same `kp_guid` to the VMGS file and find that nothing changes
        write_key_protector_by_id(&mut key_protector_by_id, &mut vmgs, false, kp_guid)
            .await
            .unwrap();
        // `key_protector_by_id` should still hold `kp_guid`
        let found_key_protector_by_id = read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(found_key_protector_by_id.id_guid, kp_guid);

        // Without using force, write a new `Guid` to the VMGS file and find that the `key_protector_by_id` is updated
        let bios_guid = Guid::new_random();
        write_key_protector_by_id(&mut key_protector_by_id, &mut vmgs, false, bios_guid)
            .await
            .unwrap();
        // `key_protector_by_id` should now hold `new_guid`
        let found_key_protector_by_id = read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(found_key_protector_by_id.id_guid, bios_guid);

        // Read a key protector by id from the VMGS file that is undersized
        // ported and pad fields are expected to be zeroed
        let undersized_key_protector_by_id = key_protector_by_id.as_bytes();
        let undersized_key_protector_by_id =
            &undersized_key_protector_by_id[..undersized_key_protector_by_id.len() - 1];
        vmgs.write_file(FileId::VM_UNIQUE_ID, undersized_key_protector_by_id)
            .await
            .unwrap();

        let found_key_protector_by_id = read_key_protector_by_id(&mut vmgs).await.unwrap();
        assert_eq!(
            found_key_protector_by_id.id_guid,
            key_protector_by_id.id_guid
        );
        assert_eq!(found_key_protector_by_id.ported, 0);
        assert_eq!(found_key_protector_by_id.pad, [0, 0, 0]);
    }

    #[async_test]
    async fn read_security_profile_from_vmgs() {
        let mut vmgs = new_formatted_vmgs().await;
        let found_security_profile = read_security_profile(&mut vmgs).await.unwrap();

        // When no security profile exists, a zeroed security profile will be written to the VMGS
        assert_eq!(
            found_security_profile.agent_data,
            SecurityProfile::new_zeroed().agent_data
        );

        // Write a security profile to the VMGS
        let security_profile = SecurityProfile {
            agent_data: [5; AGENT_DATA_MAX_SIZE],
        };
        vmgs.write_file(FileId::ATTEST, security_profile.as_bytes())
            .await
            .unwrap();
        let found_security_profile = read_security_profile(&mut vmgs).await.unwrap();
        assert_eq!(
            found_security_profile.agent_data,
            security_profile.agent_data
        );

        // Write a security profile larger than the maximum size to the VMGS
        let oversized_security_profile = [6u8; AGENT_DATA_MAX_SIZE + 1];
        vmgs.write_file(FileId::ATTEST, oversized_security_profile.as_bytes())
            .await
            .unwrap();
        let found_security_profile_result = read_security_profile(&mut vmgs).await;
        assert!(found_security_profile_result.is_err());
        assert_eq!(
            found_security_profile_result.unwrap_err().to_string(),
            "ATTEST valid bytes 2049 larger than the maximum size 2048"
        );

        // Write a security profile smaller than the maximum size to the VMGS and observe that it is padded with zeros
        let undersized_security_profile = [7u8; AGENT_DATA_MAX_SIZE - 10];
        vmgs.write_file(FileId::ATTEST, undersized_security_profile.as_bytes())
            .await
            .unwrap();
        let found_security_profile = read_security_profile(&mut vmgs).await.unwrap();
        assert_eq!(
            found_security_profile.agent_data[..AGENT_DATA_MAX_SIZE - 10],
            undersized_security_profile[..]
        );
        assert_eq!(
            found_security_profile.agent_data[AGENT_DATA_MAX_SIZE - 10..],
            [0; 10]
        );
    }

    #[async_test]
    async fn write_read_hardware_key_protector() {
        let mut vmgs = new_formatted_vmgs().await;
        let hardware_key_protector = new_hardware_key_protector();
        write_hardware_key_protector(&hardware_key_protector, &mut vmgs)
            .await
            .unwrap();

        let found_hardware_key_protector = read_hardware_key_protector(&mut vmgs).await.unwrap();
        assert_eq!(
            found_hardware_key_protector.as_bytes(),
            hardware_key_protector.as_bytes()
        );

        // Write and then fail to read a hardware key protector with an unexpected size to the VMGS
        let mut oversized_hardware_key_protector = hardware_key_protector.as_bytes().to_vec();
        oversized_hardware_key_protector.push(0);
        vmgs.write_file(FileId::HW_KEY_PROTECTOR, &oversized_hardware_key_protector)
            .await
            .unwrap();
        let found_hardware_key_protector_result = read_hardware_key_protector(&mut vmgs).await;
        assert!(found_hardware_key_protector_result.is_err());
        assert_eq!(
            found_hardware_key_protector_result.unwrap_err().to_string(),
            format!(
                "HW_KEY_PROTECTOR valid bytes {}, expected {}",
                HW_KEY_PROTECTOR_V3_SIZE + 1,
                HW_KEY_PROTECTOR_V3_SIZE
            )
        );
    }

    #[async_test]
    async fn read_guest_secret_key_from_vmgs() {
        let mut vmgs = new_formatted_vmgs().await;

        // When no guest secret key exists, an error should be returned
        let found_guest_secret_key_result = read_guest_secret_key(&mut vmgs).await;
        assert!(found_guest_secret_key_result.is_err());
        assert_eq!(
            found_guest_secret_key_result.unwrap_err().to_string(),
            "entry does not exist, file id: GUEST_SECRET_KEY"
        );

        // Write a guest secret key to the VMGS
        let guest_secret_key = GuestSecretKey {
            guest_secret_key: [9; GUEST_SECRET_KEY_MAX_SIZE],
        };
        vmgs.write_file(FileId::GUEST_SECRET_KEY, guest_secret_key.as_bytes())
            .await
            .unwrap();
        let found_guest_secret_key = read_guest_secret_key(&mut vmgs).await.unwrap();
        assert_eq!(
            found_guest_secret_key.guest_secret_key,
            guest_secret_key.guest_secret_key
        );

        // Write a guest secret key larger than the maximum size to the VMGS
        let oversized_guest_secret_key = [10u8; GUEST_SECRET_KEY_MAX_SIZE + 1];
        vmgs.write_file(FileId::GUEST_SECRET_KEY, &oversized_guest_secret_key)
            .await
            .unwrap();
        let found_guest_secret_key_result = read_guest_secret_key(&mut vmgs).await;
        assert!(found_guest_secret_key_result.is_err());
        assert_eq!(
            found_guest_secret_key_result.unwrap_err().to_string(),
            "GUEST_SECRET_KEY valid bytes 2049 larger than the maximum size 2048"
        );

        // Write a guest secret smaller than the maximum size to the VMGS and observe that it is padded with zeros
        let undersized_guest_secret_key = [7u8; GUEST_SECRET_KEY_MAX_SIZE - 10];
        vmgs.write_file(
            FileId::GUEST_SECRET_KEY,
            undersized_guest_secret_key.as_bytes(),
        )
        .await
        .unwrap();
        let found_guest_secret_key = read_guest_secret_key(&mut vmgs).await.unwrap();
        assert_eq!(
            found_guest_secret_key.guest_secret_key[..AGENT_DATA_MAX_SIZE - 10],
            undersized_guest_secret_key[..]
        );
        assert_eq!(
            found_guest_secret_key.guest_secret_key[AGENT_DATA_MAX_SIZE - 10..],
            [0; 10]
        );
    }
}
