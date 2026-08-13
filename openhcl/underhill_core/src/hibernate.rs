// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenHCL hibernate token handling: the [`HibernateToken`] recorded in
//! [`vmgs::FileId::HIBERNATION_TOKEN`] and helpers to read, write, and delete
//! it.

use cvm_tracing::CVM_ALLOWED;
use std::fmt;
use vmgs_broker::VmgsBrokerError;
use vmgs_broker::VmgsClientError;

/// The hibernate marker recorded in [`vmgs::FileId::HIBERNATION_TOKEN`],
/// decoupled from its on-disk encoding.
///
/// Stored as an 8-byte little-endian value. `0` is [`Self::NotHibernated`];
/// `1..=0xFFFF` is a firmware version (major in the high byte, minor in the low
/// byte); any other value is kept verbatim as [`Self::Other`] so the full `u64`
/// round-trips.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HibernateToken {
    /// Not hibernated; written on a clean power off / reset.
    NotHibernated,
    /// Hibernated under firmware version `major.minor`.
    Hibernated { major: u8, minor: u8 },
    /// An unrecognized value, preserved verbatim.
    Other(u64),
}

impl HibernateToken {
    /// Written when the current firmware hibernates; bump per release.
    pub const CURRENT: Self = Self::Hibernated { major: 1, minor: 9 };
    /// Written when the firmware version is unknown (e.g. after servicing).
    pub const UNKNOWN: Self = Self::Hibernated { major: 1, minor: 0 };
}

impl From<HibernateToken> for u64 {
    fn from(token: HibernateToken) -> Self {
        match token {
            HibernateToken::NotHibernated => 0,
            HibernateToken::Hibernated { major, minor } => {
                (u64::from(major) << 8) | u64::from(minor)
            }
            HibernateToken::Other(raw) => raw,
        }
    }
}

impl From<u64> for HibernateToken {
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

impl fmt::Display for HibernateToken {
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
    pub current_token: HibernateToken,
}

/// Best-effort write of an 8-byte hibernate token to the VMGS. Failures are
/// logged but never block the power transition.
pub async fn write_token(vmgs_client: &vmgs_broker::VmgsClient, token: HibernateToken) {
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
pub async fn read_token(vmgs_client: &vmgs_broker::VmgsClient) -> Option<HibernateToken> {
    match vmgs_client.read_file(vmgs::FileId::HIBERNATION_TOKEN).await {
        Ok(buf) => match <[u8; 8]>::try_from(buf.as_slice()) {
            Ok(bytes) => {
                let token = HibernateToken::from(u64::from_le_bytes(bytes));
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
        write_token(&client, HibernateToken::CURRENT).await;
        assert_eq!(read_token(&client).await, Some(HibernateToken::CURRENT));
        delete_token(&client).await;
        assert_eq!(read_token(&client).await, None);
    }

    #[async_test]
    async fn write_not_hibernated(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        write_token(&client, HibernateToken::NotHibernated).await;
        assert_eq!(
            read_token(&client).await,
            Some(HibernateToken::NotHibernated)
        );
    }

    #[test]
    fn constants_encode_as_expected() {
        assert_eq!(u64::from(HibernateToken::NotHibernated), 0);
        assert_eq!(u64::from(HibernateToken::CURRENT), 0x0109);
        assert_eq!(u64::from(HibernateToken::UNKNOWN), 0x0100);
    }

    #[test]
    fn classification_boundaries() {
        // 0 is the not-hibernated marker.
        assert_eq!(HibernateToken::from(0), HibernateToken::NotHibernated);
        // 1..=0xFFFF decode as firmware versions.
        assert_eq!(
            HibernateToken::from(1),
            HibernateToken::Hibernated { major: 0, minor: 1 }
        );
        assert_eq!(
            HibernateToken::from(0xFFFF),
            HibernateToken::Hibernated {
                major: 0xFF,
                minor: 0xFF
            }
        );
        // The first value past the version range falls through to Other.
        assert_eq!(
            HibernateToken::from(0x1_0000),
            HibernateToken::Other(0x1_0000)
        );
        assert_eq!(
            HibernateToken::from(u64::MAX),
            HibernateToken::Other(u64::MAX)
        );
    }

    #[test]
    fn all_versions_round_trip() {
        // Exhaustively cover the entire firmware-version range.
        for raw in 1..=0xFFFFu64 {
            let token = HibernateToken::from(raw);
            assert!(matches!(token, HibernateToken::Hibernated { .. }));
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
            let token = HibernateToken::from(raw);
            assert_eq!(token, HibernateToken::Other(raw));
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
