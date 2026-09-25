// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Validation of the host endpoint attachment carried by console resources.

use virtio_resources::console::attachment::VirtioConsoleAttachment;
use virtio_resources::console::attachment::VirtioConsoleAttachmentMode;
use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

pub(super) fn validate_attachment(
    attachment: Option<&VirtioConsoleAttachment>,
) -> anyhow::Result<()> {
    if let Some(attachment) = attachment {
        anyhow::ensure!(
            !attachment.stable_id.is_empty() && attachment.stable_id.len() <= 128,
            "virtio-console attachment has an invalid stable ID"
        );
        anyhow::ensure!(
            !attachment.endpoint_identity.is_empty() && attachment.endpoint_identity.len() <= 4096,
            "virtio-console attachment has an invalid endpoint identity"
        );
        anyhow::ensure!(
            matches!(
                (attachment.mode, attachment.reconnect_policy),
                (
                    VirtioConsoleAttachmentMode::Listen,
                    VirtioConsoleReconnectPolicy::RecreateListener
                ) | (
                    VirtioConsoleAttachmentMode::Connect,
                    VirtioConsoleReconnectPolicy::ReconnectClient
                ) | (
                    VirtioConsoleAttachmentMode::Inherited,
                    VirtioConsoleReconnectPolicy::RequireInheritedAttachment
                ) | (
                    VirtioConsoleAttachmentMode::Inherited,
                    VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
                )
            ),
            "virtio-console attachment mode and reconnect policy conflict"
        );
        match attachment.reconnect_policy {
            VirtioConsoleReconnectPolicy::RecreateListener => anyhow::ensure!(
                !attachment.required && attachment.reconnect_timeout_ms == 0,
                "listener console attachments must be optional and have no reconnect timeout"
            ),
            VirtioConsoleReconnectPolicy::ReconnectClient => anyhow::ensure!(
                attachment.required && attachment.reconnect_timeout_ms != 0,
                "client console attachments must be required with a bounded timeout"
            ),
            VirtioConsoleReconnectPolicy::RequireInheritedAttachment => anyhow::ensure!(
                attachment.required && attachment.reconnect_timeout_ms == 0,
                "inherited console attachments must be required with no reconnect timeout"
            ),
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected => anyhow::ensure!(
                !attachment.required && attachment.reconnect_timeout_ms == 0,
                "discarding console attachments must be optional with no reconnect timeout"
            ),
        }
    }
    Ok(())
}
