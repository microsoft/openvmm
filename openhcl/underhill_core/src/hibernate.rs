// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenHCL hibernate token handling: the values written to
//! [`vmgs::FileId::HIBERNATION_TOKEN`] and helpers to read, write, and delete
//! it. Tokens encode the firmware version in 16 bits: the high 8 bits are the
//! major version and the low 8 bits are the minor version.

use cvm_tracing::CVM_ALLOWED;
use vmgs_broker::VmgsBrokerError;
use vmgs_broker::VmgsClientError;

/// Hibernate token values written to [`vmgs::FileId::HIBERNATION_TOKEN`].
pub mod token {
    /// Written on a clean power off / reset; the guest is not hibernated.
    pub const NONE: u64 = 0x0;
    /// Written when the firmware version is unknown (e.g. after servicing).
    pub const HIBERNATED_UNKNOWN: u64 = 0x0100;
    /// Written on hibernate under firmware version 1.7.
    #[expect(dead_code, reason = "reserved firmware version, not yet written")]
    pub const HIBERNATED_1_7: u64 = (1 << 8) | 7;
    /// Written on hibernate under firmware version 1.8.
    #[expect(dead_code, reason = "reserved firmware version, not yet written")]
    pub const HIBERNATED_1_8: u64 = (1 << 8) | 8;
    /// Written on hibernate under firmware version 1.9.
    pub const HIBERNATED_1_9: u64 = (1 << 8) | 9;
    /// The current firmware version's token, written on hibernate.
    pub const DEFAULT: u64 = HIBERNATED_1_9;
}

/// Hibernation state the halt task needs to persist the token across a power
/// transition. Present only when hibernation is enabled and VMGS is available.
pub struct HaltState {
    /// VMGS client used to persist the hibernate token.
    pub vmgs_client: vmgs_broker::VmgsClient,
    /// Token to write on hibernate.
    pub current_token: u64,
}

/// Best-effort write of an 8-byte hibernate token to the VMGS. Failures are
/// logged but never block the power transition.
pub async fn write_token(vmgs_client: &vmgs_broker::VmgsClient, token: u64) {
    if let Err(err) = vmgs_client
        .write_file(
            vmgs::FileId::HIBERNATION_TOKEN,
            token.to_le_bytes().to_vec(),
        )
        .await
    {
        tracing::error!(
            CVM_ALLOWED,
            error = &err as &dyn std::error::Error,
            token = format_args!("{token:#x}"),
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
pub async fn read_token(vmgs_client: &vmgs_broker::VmgsClient) -> Option<u64> {
    match vmgs_client.read_file(vmgs::FileId::HIBERNATION_TOKEN).await {
        Ok(buf) => match <[u8; 8]>::try_from(buf.as_slice()) {
            Ok(bytes) => {
                let token = u64::from_le_bytes(bytes);
                tracing::info!(
                    CVM_ALLOWED,
                    token = format_args!("{token:#x}"),
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
        write_token(&client, token::DEFAULT).await;
        assert_eq!(read_token(&client).await, Some(token::DEFAULT));
        delete_token(&client).await;
        assert_eq!(read_token(&client).await, None);
    }

    #[async_test]
    async fn read_none_value(driver: DefaultDriver) {
        let (client, _task) = new_client(&driver).await;
        write_token(&client, token::NONE).await;
        assert_eq!(read_token(&client).await, Some(token::NONE));
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
