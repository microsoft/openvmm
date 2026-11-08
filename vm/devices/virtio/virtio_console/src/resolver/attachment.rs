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
                        | VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
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

#[cfg(test)]
mod tests {
    use super::validate_attachment;
    use test_with_tracing::test;
    use virtio_resources::console::attachment::VirtioConsoleAttachment;
    use virtio_resources::console::attachment::VirtioConsoleAttachmentMode;
    use virtio_resources::console::attachment::VirtioConsoleBackendKind;
    use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

    fn discarding_listener() -> VirtioConsoleAttachment {
        VirtioConsoleAttachment {
            stable_id: "console:microvm-virtio0".to_owned(),
            backend_kind: VirtioConsoleBackendKind::UnixSocket,
            mode: VirtioConsoleAttachmentMode::Listen,
            endpoint_identity: "/snapshots/clone.console".to_owned(),
            reconnect_policy: VirtioConsoleReconnectPolicy::DiscardWhileDisconnected,
            required: false,
            reconnect_timeout_ms: 0,
        }
    }

    #[test]
    fn a_listener_may_discard_guest_output_while_disconnected() {
        validate_attachment(Some(&discarding_listener())).unwrap();

        // It stays optional, with no reconnect timeout, as a disconnected
        // attachment does.
        for attachment in [
            VirtioConsoleAttachment {
                required: true,
                ..discarding_listener()
            },
            VirtioConsoleAttachment {
                reconnect_timeout_ms: 5000,
                ..discarding_listener()
            },
        ] {
            assert!(validate_attachment(Some(&attachment)).is_err());
        }
    }

    #[test]
    fn a_client_never_discards_guest_output() {
        let client = VirtioConsoleAttachment {
            mode: VirtioConsoleAttachmentMode::Connect,
            required: true,
            reconnect_timeout_ms: 5000,
            ..discarding_listener()
        };
        assert!(validate_attachment(Some(&client)).is_err());
    }
}
