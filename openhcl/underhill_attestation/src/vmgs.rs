// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the helper functions for accessing VMGS entries.

use crate::protocol;
use crate::protocol::vmgs::GuestSecretKey;
use crate::protocol::vmgs::HardwareKeyProtector;
use crate::protocol::vmgs::KeyProtector;
use crate::protocol::vmgs::KeyProtectorById;
use crate::protocol::vmgs::SecurityProfile;
use crate::protocol::vmgs::AGENT_DATA_MAX_SIZE;
use crate::protocol::vmgs::GUEST_SECRET_KEY_MAX_SIZE;
use cvm_tracing::CVM_ALLOWED;
use guid::Guid;
use thiserror::Error;
use vmgs::FileId;
use vmgs::Vmgs;
use zerocopy::AsBytes;
use zerocopy::FromBytes;
use zerocopy::FromZeroes;

#[allow(missing_docs)] // self-explanatory fields
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
#[error("failed to write {file_id:?} to vmgs")]
pub(crate) struct WriteToVmgsError {
    #[source]
    vmgs_err: vmgs::Error,
    file_id: FileId,
}

/// Read Key Protector data from the VMGS file. If [`FileId::KEY_PROTECTOR`] doesn't exist yet,
/// locally initialize a key_protector instance that can be written to.
pub async fn read_key_protector(
    vmgs: &mut Vmgs,
    dek_minimal_size: usize,
) -> Result<KeyProtector, ReadFromVmgsError> {
    use protocol::vmgs::KEY_PROTECTOR_SIZE;

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

            tracing::info!(
                CVM_ALLOWED,
                entry_size = data.len(),
                "Key protector data read from VMGS"
            );

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
                .ok_or(ReadFromVmgsError::InvalidFormat(file_id))
        }
        Err(vmgs::Error::FileInfoAllocated) => Ok(KeyProtector::new_zeroed()),
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
        .map_err(|vmgs_err| WriteToVmgsError { vmgs_err, file_id })
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
        Ok(data) => match KeyProtectorById::read_from_prefix(&data[..]) {
            Some(key_protector_by_id) => Ok(key_protector_by_id),
            None => {
                let id_guid = Guid::read_from_prefix(&data[..])
                    .ok_or_else(|| ReadFromVmgsError::InvalidFormat(file_id))?;

                Ok(KeyProtectorById {
                    id_guid,
                    ..FromZeroes::new_zeroed()
                })
            }
        },
        Err(vmgs::Error::FileInfoAllocated) => Err(ReadFromVmgsError::EntryNotFound(file_id)),
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
            .map_err(|vmgs_err| WriteToVmgsError { vmgs_err, file_id })?
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
                .ok_or(ReadFromVmgsError::InvalidFormat(file_id))?)
        }
        Err(vmgs::Error::FileInfoAllocated) => Ok(SecurityProfile::new_zeroed()),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { file_id, vmgs_err })?,
    }
}

/// Read the hardware key protector from the VMGS file.
pub async fn read_hardware_key_protector(
    vmgs: &mut Vmgs,
) -> Result<HardwareKeyProtector, ReadFromVmgsError> {
    use protocol::vmgs::HW_KEY_PROTECTOR_SIZE;

    let file_id = FileId::HW_KEY_PROTECTOR;
    let data = vmgs
        .read_file(file_id)
        .await
        .map_err(|vmgs_err| ReadFromVmgsError::ReadFromVmgs { vmgs_err, file_id })?;

    if data.len() != HW_KEY_PROTECTOR_SIZE {
        Err(ReadFromVmgsError::EntrySizeUnexpected {
            file_id,
            size: data.len(),
            expected_size: HW_KEY_PROTECTOR_SIZE,
        })?
    }

    HardwareKeyProtector::read_from_prefix(&data).ok_or(ReadFromVmgsError::InvalidFormat(file_id))
}

/// Write Key Protector Id (current Id) to the VMGS file.
pub async fn write_hardware_key_protector(
    hardware_key_protector: &HardwareKeyProtector,
    vmgs: &mut Vmgs,
) -> Result<(), WriteToVmgsError> {
    let file_id = FileId::HW_KEY_PROTECTOR;
    vmgs.write_file(file_id, hardware_key_protector.as_bytes())
        .await
        .map_err(|vmgs_err| WriteToVmgsError { vmgs_err, file_id })
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
                .ok_or(ReadFromVmgsError::InvalidFormat(file_id))?)
        }
        Err(vmgs::Error::FileInfoAllocated) => Err(ReadFromVmgsError::EntryNotFound(file_id)),
        Err(vmgs_err) => Err(ReadFromVmgsError::ReadFromVmgs { file_id, vmgs_err }),
    }
}
