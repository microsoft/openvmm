// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenHCL hibernate state stored in VMGS: the [`Token`] recorded in
//! [`vmgs::FileId::HIBERNATION_TOKEN`] and the UEFI firmware image snapshot in
//! [`vmgs::FileId::HIBERNATION_FIRMWARE`], with helpers to read, write, and
//! delete both.

use anyhow::Context;
use cvm_tracing::CVM_ALLOWED;
use guestmem::GuestMemory;
use memory_range::MemoryRange;
use std::fmt;
use vmgs_broker::VmgsBrokerError;
use vmgs_broker::VmgsClientError;
use vmgs_format::VMGS_HIBERNATION_FIRMWARE_MIN_SIZE;

/// The hibernate marker recorded in [`vmgs::FileId::HIBERNATION_TOKEN`],
/// decoupled from its on-disk encoding.
///
/// Stored as an 8-byte little-endian value. `0` is [`Self::NotHibernated`];
/// `1..=0xFFFF` is a firmware version (major in the high byte, minor in the low
/// byte); any other value is kept verbatim as [`Self::Other`] so the full `u64`
/// round-trips.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Token {
    /// Not hibernated; written on a clean power off / reset.
    NotHibernated,
    /// Hibernated under firmware version `major.minor`.
    Hibernated { major: u8, minor: u8 },
    /// An unrecognized value, preserved verbatim.
    Other(u64),
}

impl Token {
    /// Written when the current firmware hibernates; bump per release.
    pub const CURRENT: Self = Self::Hibernated { major: 1, minor: 9 };
    /// Written when the firmware version is unknown (e.g. after servicing).
    pub const UNKNOWN: Self = Self::Hibernated { major: 1, minor: 0 };
}

impl From<Token> for u64 {
    fn from(token: Token) -> Self {
        match token {
            Token::NotHibernated => 0,
            Token::Hibernated { major, minor } => (u64::from(major) << 8) | u64::from(minor),
            Token::Other(raw) => raw,
        }
    }
}

impl From<u64> for Token {
    fn from(raw: u64) -> Self {
        match raw {
            0 => Self::NotHibernated,
            1..=0xFFFF => Self::Hibernated {
                major: (raw >> 8) as u8,
                minor: raw as u8,
            },
            raw => Self::Other(raw),
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotHibernated => f.write_str("not-hibernated"),
            Self::Hibernated { major, minor } => write!(f, "{major}.{minor}"),
            Self::Other(raw) => write!(f, "{raw:#x}"),
        }
    }
}

/// Hibernation state the halt task needs to persist the token across a power
/// transition. Present only when hibernation is enabled and VMGS is available.
pub struct HaltState {
    /// VMGS client used to persist the hibernate token.
    pub vmgs_client: vmgs_broker::VmgsClient,
    /// Token to write on hibernate.
    pub current_token: Token,
}

/// Best-effort write of an 8-byte hibernate token to the VMGS. Failures are
/// logged but never block the power transition.
pub async fn write_token(vmgs_client: &vmgs_broker::VmgsClient, token: Token) {
    if let Err(err) = vmgs_client
        .write_file(
            vmgs::FileId::HIBERNATION_TOKEN,
            u64::from(token).to_le_bytes().to_vec(),
        )
        .await
    {
        tracing::error!(
            CVM_ALLOWED,
            error = &err as &dyn std::error::Error,
            %token,
            "failed to write hibernate token"
        );
    }
}

/// Best-effort deletion of the hibernate token, clearing any prior hibernate
/// marker. Never blocks the power transition.
pub async fn delete_token(vmgs_client: &vmgs_broker::VmgsClient) {
    match vmgs_client
        .delete_file(vmgs::FileId::HIBERNATION_TOKEN)
        .await
    {
        // An absent token is the common no-hibernate case.
        Ok(()) | Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated)) => {}
        Err(err) => {
            tracing::error!(
                CVM_ALLOWED,
                error = &err as &dyn std::error::Error,
                "failed to delete hibernate token"
            );
        }
    }
}

/// At boot, record which hibernate token (if any) the previous session left
/// behind.
pub async fn read_token(vmgs_client: &vmgs_broker::VmgsClient) -> Option<Token> {
    match vmgs_client.read_file(vmgs::FileId::HIBERNATION_TOKEN).await {
        Ok(buf) => match <[u8; 8]>::try_from(buf.as_slice()) {
            Ok(bytes) => {
                let token = Token::from(u64::from_le_bytes(bytes));
                tracing::info!(
                    CVM_ALLOWED,
                    %token,
                    "hibernation enabled: hibernation token found"
                );
                Some(token)
            }
            Err(_) => {
                tracing::warn!(
                    CVM_ALLOWED,
                    "hibernation enabled: corrupt hibernation token found"
                );
                None
            }
        },
        Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated)) => {
            tracing::info!(
                CVM_ALLOWED,
                "hibernation enabled: no hibernation token found"
            );
            None
        }
        Err(err) => {
            tracing::error!(
                CVM_ALLOWED,
                error = &err as &dyn std::error::Error,
                "hibernation enabled: failed to read hibernation token"
            );
            None
        }
    }
}

/// The result of attempting to store a firmware image into VMGS for hibernation.
pub enum StoreFirmwareOutcome {
    /// The firmware image was stored to VMGS.
    Stored,
    /// The VMGS backing store is too small to hold the firmware image. This is
    /// not an error: hibernation falls back to not preserving the firmware
    /// image across resume.
    InsufficientSpace {
        firmware_size: u64,
        device_size: u64,
    },
}

/// Stores the pristine UEFI firmware image out of VTL0 guest memory and into
/// [`vmgs::FileId::HIBERNATION_FIRMWARE`] so it can be restored identically on a
/// hibernation resume where the host cannot overload the firmware itself.
///
/// This must be called *before* any dynamic configuration is written into the
/// firmware region (i.e. before `write_uefi_config`), so that the stored image
/// matches the measured firmware as loaded from the IGVM file.
///
/// The image is only stored if the VMGS backing store is large enough to hold
/// it; otherwise [`StoreFirmwareOutcome::InsufficientSpace`] is returned and the
/// caller falls back to not preserving the firmware across hibernation. An
/// `Err` is only returned for an actual failure to query or write VMGS.
pub async fn store_firmware(
    gm: &GuestMemory,
    firmware_memory: MemoryRange,
    vmgs_client: &vmgs_broker::VmgsClient,
) -> anyhow::Result<StoreFirmwareOutcome> {
    let len = firmware_memory.len();

    // The firmware image can only be stored if the VMGS backing store is large
    // enough. Require a minimum overall size so there is room for the firmware
    // image alongside the other VMGS files, and also verify the image fits.
    // Insufficient space is a normal fallback, not an error.
    let device_size = vmgs_client
        .device_size()
        .await
        .context("failed to query VMGS size")?;
    if device_size < VMGS_HIBERNATION_FIRMWARE_MIN_SIZE || len > device_size {
        return Ok(StoreFirmwareOutcome::InsufficientSpace {
            firmware_size: len,
            device_size,
        });
    }

    let firmware_len = usize::try_from(len).context("firmware image size does not fit in usize")?;
    let mut firmware = vec![0u8; firmware_len];
    gm.read_at(firmware_memory.start(), &mut firmware)
        .context("failed to read UEFI firmware image from VTL0 memory")?;
    vmgs_client
        .write_file(vmgs::FileId::HIBERNATION_FIRMWARE, firmware)
        .await
        .context("failed to write UEFI firmware snapshot to VMGS")?;
    tracing::info!(
        CVM_ALLOWED,
        size_bytes = len,
        "stored UEFI firmware image to VMGS for hibernation"
    );
    Ok(StoreFirmwareOutcome::Stored)
}

/// Reads a stored UEFI firmware image from VMGS, returning `None` if none is
/// stored (the usual case, e.g. first hibernation or a VMGS too small to hold
/// one) or the read fails.
pub async fn read_firmware(vmgs_client: &vmgs_broker::VmgsClient) -> Option<Vec<u8>> {
    match vmgs_client
        .read_file(vmgs::FileId::HIBERNATION_FIRMWARE)
        .await
    {
        Ok(firmware) => Some(firmware),
        // No stored image is the usual case; not an error.
        Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated)) => None,
        Err(err) => {
            tracing::warn!(
                CVM_ALLOWED,
                error = &err as &dyn std::error::Error,
                "failed to read UEFI firmware image from VMGS on hibernation resume"
            );
            None
        }
    }
}

/// Best-effort deletion of any stored hibernation firmware image, used on a
/// clean power off / reset so a later boot does not restore a stale image. A
/// missing image is the common case and is not logged.
pub async fn delete_firmware(vmgs_client: &vmgs_broker::VmgsClient) {
    match vmgs_client
        .delete_file(vmgs::FileId::HIBERNATION_FIRMWARE)
        .await
    {
        Ok(()) | Err(VmgsClientError::Vmgs(VmgsBrokerError::FileInfoNotAllocated)) => {}
        Err(err) => {
            tracing::error!(
                CVM_ALLOWED,
                error = &err as &dyn std::error::Error,
                "failed to delete stored UEFI firmware image from VMGS"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disklayer_ram::ram_disk;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Task;
    use vmgs::Vmgs;
    use vmgs_broker::VmgsClient;
    use vmgs_broker::spawn_vmgs_broker;

    async fn new_client(driver: &DefaultDriver) -> (VmgsClient, Task<()>) {
        let disk = ram_disk(4 * 1024 * 1024, false).unwrap();
        let vmgs = Vmgs::format_new(disk, None).await.unwrap();
        spawn_vmgs_broker(driver.clone(), vmgs)
    }

    #[async_test]
    async fn read_absent_is_none(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        assert_eq!(read_token(&client).await, None);
    }

    #[async_test]
    async fn write_read_delete(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        write_token(&client, Token::CURRENT).await;
        assert_eq!(read_token(&client).await, Some(Token::CURRENT));
        delete_token(&client).await;
        assert_eq!(read_token(&client).await, None);
    }

    #[async_test]
    async fn write_not_hibernated(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        write_token(&client, Token::NotHibernated).await;
        assert_eq!(read_token(&client).await, Some(Token::NotHibernated));
    }

    #[async_test]
    async fn firmware_store_read_delete(driver: DefaultDriver) {
        // The store is gated on VMGS_HIBERNATION_FIRMWARE_MIN_SIZE, so use a
        // backing store larger than that minimum.
        let disk = ram_disk(48 * 1024 * 1024, false).unwrap();
        let vmgs = Vmgs::format_new(disk, None).await.unwrap();
        let (client, _task) = spawn_vmgs_broker(driver.clone(), vmgs);

        // No image stored initially.
        assert!(read_firmware(&client).await.is_none());

        let gm = GuestMemory::allocate(0x2000);
        let firmware_memory = MemoryRange::new(0x1000..0x2000);
        let image: Vec<u8> = (0..0x1000u32).map(|i| i as u8).collect();
        gm.write_at(firmware_memory.start(), &image).unwrap();

        assert!(matches!(
            store_firmware(&gm, firmware_memory, &client).await.unwrap(),
            StoreFirmwareOutcome::Stored
        ));
        assert_eq!(
            read_firmware(&client).await.as_deref(),
            Some(image.as_slice())
        );

        delete_firmware(&client).await;
        assert!(read_firmware(&client).await.is_none());
    }

    #[async_test]
    async fn firmware_store_insufficient_space(driver: DefaultDriver) {
        // `new_client` uses a 4 MB disk, below VMGS_HIBERNATION_FIRMWARE_MIN_SIZE.
        let (client, _task) = new_client(&driver).await;
        let gm = GuestMemory::allocate(0x1000);
        let firmware_memory = MemoryRange::new(0..0x1000);
        assert!(matches!(
            store_firmware(&gm, firmware_memory, &client).await.unwrap(),
            StoreFirmwareOutcome::InsufficientSpace { .. }
        ));
        assert!(read_firmware(&client).await.is_none());
    }

    #[test]
    fn constants_encode_as_expected() {
        assert_eq!(u64::from(Token::NotHibernated), 0);
        assert_eq!(u64::from(Token::CURRENT), 0x0109);
        assert_eq!(u64::from(Token::UNKNOWN), 0x0100);
    }

    #[test]
    fn classification_boundaries() {
        // 0 is the not-hibernated marker.
        assert_eq!(Token::from(0), Token::NotHibernated);
        // 1..=0xFFFF decode as firmware versions.
        assert_eq!(Token::from(1), Token::Hibernated { major: 0, minor: 1 });
        assert_eq!(
            Token::from(0xFFFF),
            Token::Hibernated {
                major: 0xFF,
                minor: 0xFF
            }
        );
        // The first value past the version range falls through to Other.
        assert_eq!(Token::from(0x1_0000), Token::Other(0x1_0000));
        assert_eq!(Token::from(u64::MAX), Token::Other(u64::MAX));
    }

    #[test]
    fn all_versions_round_trip() {
        // Exhaustively cover the entire firmware-version range.
        for raw in 1..=0xFFFFu64 {
            let token = Token::from(raw);
            assert!(matches!(token, Token::Hibernated { .. }));
            assert_eq!(u64::from(token), raw);
        }
    }

    #[test]
    fn other_values_round_trip() {
        // Spot-check a spread of values above the version range, including the
        // boundary, powers of two, and the maximum.
        let mut cases = vec![0x1_0000u64, 0x1_0001, u64::MAX, u64::MAX - 1];
        for shift in 16..64 {
            cases.push(1u64 << shift);
        }
        for raw in cases {
            let token = Token::from(raw);
            assert_eq!(token, Token::Other(raw));
            assert_eq!(u64::from(token), raw);
        }
    }

    #[async_test]
    async fn read_corrupt_is_none(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        // A token shorter than 8 bytes is treated as corrupt.
        client
            .write_file(vmgs::FileId::HIBERNATION_TOKEN, vec![1, 2, 3])
            .await
            .unwrap();
        assert_eq!(read_token(&client).await, None);
    }

    #[async_test]
    async fn read_oversized_is_none(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        // A token longer than 8 bytes is also treated as corrupt.
        client
            .write_file(vmgs::FileId::HIBERNATION_TOKEN, vec![0; 16])
            .await
            .unwrap();
        assert_eq!(read_token(&client).await, None);
    }
}
