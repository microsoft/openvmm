// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM virtio-console attachments.

use crate::cli_args::SerialConfigCli;
use anyhow::Context;
#[cfg(unix)]
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// A microVM console endpoint: its effective serial backend, device attachment
/// configuration, and snapshot attachment identity.
pub(super) type ConsoleEndpoint = (
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
);

pub(crate) const MICROVM_CONSOLE_STABLE_ID: &str = "console:microvm-virtio0";
pub(crate) const MICROVM_CONSOLE_ATTACHMENT_KIND: &str = "virtio-console";

pub(crate) struct MicrovmConsoleSocketCleanup {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl MicrovmConsoleSocketCleanup {
    #[cfg(unix)]
    fn new(path: PathBuf) -> anyhow::Result<Self> {
        use std::os::unix::fs::FileTypeExt as _;
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs_err::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect console socket {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "microVM console path is not a Unix socket: {}",
            path.display()
        );
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(crate) fn remove_if_owned(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt as _;
            use std::os::unix::fs::MetadataExt as _;

            let metadata = match fs_err::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            anyhow::ensure!(
                metadata.file_type().is_socket()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode,
                "microVM console socket ownership changed: {}",
                self.path.display()
            );
            fs_err::remove_file(&self.path).with_context(|| {
                format!("failed to remove console socket {}", self.path.display())
            })?;
        }
        Ok(())
    }
}

impl Drop for MicrovmConsoleSocketCleanup {
    fn drop(&mut self) {
        let _ = self.remove_if_owned();
    }
}

pub(crate) fn microvm_console_socket_cleanup(
    path: PathBuf,
) -> anyhow::Result<Option<MicrovmConsoleSocketCleanup>> {
    #[cfg(unix)]
    {
        Ok(Some(MicrovmConsoleSocketCleanup::new(path)?))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn canonical_microvm_console_path(path: &Path) -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    if path.starts_with("//./pipe") {
        return Ok(path.to_owned());
    }

    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory for console endpoint")?
            .join(path)
    };
    let file_name = absolute
        .file_name()
        .context("microVM console endpoint must name a socket or pipe")?;
    let parent = absolute
        .parent()
        .context("microVM console endpoint has no parent directory")?;
    let parent = fs_err::canonicalize(parent).with_context(|| {
        format!(
            "failed to canonicalize microVM console endpoint parent {}",
            parent.display()
        )
    })?;
    let canonical = parent.join(file_name);
    if let Ok(metadata) = fs_err::symlink_metadata(&canonical) {
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "microVM console endpoint must not be a symbolic link: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn microvm_console_attachment_from_cli_with_identity(
    config: &SerialConfigCli,
    stable_id: &'static str,
    attachment_kind: &'static str,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    use virtio_resources::console::attachment::VirtioConsoleAttachment;
    use virtio_resources::console::attachment::VirtioConsoleAttachmentMode;
    use virtio_resources::console::attachment::VirtioConsoleBackendKind;
    use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

    let (
        effective,
        backend_kind,
        identity_kind,
        endpoint_identity,
        mode,
        reconnect_policy,
        reconnect_policy_name,
        required,
        reconnect_timeout_ms,
    ) = match config {
        SerialConfigCli::Pipe(path) | SerialConfigCli::ConnectPipe(path) => {
            let path = canonical_microvm_console_path(path)?;
            let identity = path
                .to_str()
                .context("microVM console endpoint path is not valid UTF-8")?
                .to_owned();
            #[cfg(windows)]
            let (backend_kind, identity_kind) = {
                let name = path
                    .to_str()
                    .and_then(|path| path.strip_prefix("//./pipe/openvmm-microvm-"));
                anyhow::ensure!(
                    name.is_some_and(|name| {
                        !name.is_empty()
                            && name.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                            })
                    }),
                    "Windows microVM console pipes must use //./pipe/openvmm-microvm-<NAME>"
                );
                (VirtioConsoleBackendKind::NamedPipe, "named-pipe")
            };
            #[cfg(not(windows))]
            let (backend_kind, identity_kind) =
                (VirtioConsoleBackendKind::UnixSocket, "unix-socket");
            let is_client = matches!(config, SerialConfigCli::ConnectPipe(_));
            (
                if is_client {
                    SerialConfigCli::ConnectPipe(path)
                } else {
                    SerialConfigCli::Pipe(path)
                },
                backend_kind,
                identity_kind,
                identity,
                if is_client {
                    VirtioConsoleAttachmentMode::Connect
                } else {
                    VirtioConsoleAttachmentMode::Listen
                },
                if is_client {
                    VirtioConsoleReconnectPolicy::ReconnectClient
                } else {
                    VirtioConsoleReconnectPolicy::RecreateListener
                },
                if is_client {
                    "reconnect-client"
                } else {
                    "recreate-listener"
                },
                is_client,
                if is_client {
                    openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                } else {
                    0
                },
            )
        }
        SerialConfigCli::Tcp(address) | SerialConfigCli::ConnectTcp(address) => {
            anyhow::ensure!(
                address.port() != 0,
                "microVM console TCP endpoint requires a nonzero stable port"
            );
            anyhow::ensure!(
                address.ip().is_loopback(),
                "microVM console TCP endpoints must use a loopback address"
            );
            let is_client = matches!(config, SerialConfigCli::ConnectTcp(_));
            (
                if is_client {
                    SerialConfigCli::ConnectTcp(*address)
                } else {
                    SerialConfigCli::Tcp(*address)
                },
                VirtioConsoleBackendKind::Tcp,
                "tcp",
                address.to_string(),
                if is_client {
                    VirtioConsoleAttachmentMode::Connect
                } else {
                    VirtioConsoleAttachmentMode::Listen
                },
                if is_client {
                    VirtioConsoleReconnectPolicy::ReconnectClient
                } else {
                    VirtioConsoleReconnectPolicy::RecreateListener
                },
                if is_client {
                    "reconnect-client"
                } else {
                    "recreate-listener"
                },
                is_client,
                if is_client {
                    openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
                } else {
                    0
                },
            )
        }
        SerialConfigCli::Console => (
            SerialConfigCli::Console,
            VirtioConsoleBackendKind::Inherited,
            "provider",
            "console".to_owned(),
            VirtioConsoleAttachmentMode::Inherited,
            VirtioConsoleReconnectPolicy::RequireInheritedAttachment,
            "require-inherited-attachment",
            true,
            0,
        ),
        SerialConfigCli::None => (
            SerialConfigCli::None,
            VirtioConsoleBackendKind::Disconnected,
            "disconnected",
            "discard".to_owned(),
            VirtioConsoleAttachmentMode::Inherited,
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected,
            "discard-while-disconnected",
            false,
            0,
        ),
        _ => anyhow::bail!(
            "microVM virtio-console requires listen=..., connect=..., console, or none"
        ),
    };
    anyhow::ensure!(
        !endpoint_identity.is_empty() && endpoint_identity.len() <= 4096,
        "microVM console endpoint identity is empty or exceeds 4096 bytes"
    );

    let attachment = VirtioConsoleAttachment {
        stable_id: stable_id.to_owned(),
        backend_kind,
        mode,
        endpoint_identity: endpoint_identity.clone(),
        reconnect_policy,
        required,
        reconnect_timeout_ms,
    };
    let snapshot_attachment = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
        stable_id: stable_id.to_owned(),
        kind: attachment_kind.to_owned(),
        required,
        reconnect_policy: reconnect_policy_name.to_owned(),
        identity_kind: identity_kind.to_owned(),
        identity: endpoint_identity.into_bytes(),
        length: 0,
        reconnect_timeout_ms,
    };
    Ok((effective, attachment, snapshot_attachment))
}

pub(crate) fn microvm_console_attachment_from_cli(
    config: &SerialConfigCli,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    microvm_console_attachment_from_cli_with_identity(
        config,
        MICROVM_CONSOLE_STABLE_ID,
        MICROVM_CONSOLE_ATTACHMENT_KIND,
    )
}

pub(crate) fn validate_microvm_console_attachment_namespace(
    attachment: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    snapshot_dir: &Path,
) -> anyhow::Result<()> {
    match attachment.identity_kind.as_str() {
        "unix-socket" => {
            let identity = std::str::from_utf8(&attachment.identity)
                .context("microVM console socket identity is not valid UTF-8")?;
            let endpoint = Path::new(identity);
            let endpoint_parent = endpoint
                .parent()
                .context("microVM console socket identity has no parent")?;
            let snapshot_parent = snapshot_dir
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let snapshot_parent = fs_err::canonicalize(snapshot_parent).with_context(|| {
                format!(
                    "failed to canonicalize snapshot parent {}",
                    snapshot_parent.display()
                )
            })?;
            anyhow::ensure!(
                endpoint_parent == snapshot_parent,
                "microVM console socket must be inside the snapshot parent namespace {}",
                snapshot_parent.display()
            );
        }
        "named-pipe" => {
            let identity = std::str::from_utf8(&attachment.identity)
                .context("microVM console pipe identity is not valid UTF-8")?;
            let name = identity
                .strip_prefix("//./pipe/openvmm-microvm-")
                .filter(|name| !name.is_empty() && !name.contains(['/', '\\']));
            anyhow::ensure!(
                name.is_some(),
                "microVM console pipe is outside //./pipe/openvmm-microvm-<NAME>"
            );
        }
        "tcp" => {}
        "provider" => {}
        "disconnected" => {}
        kind => anyhow::bail!("unsupported microVM console identity kind '{kind}'"),
    }
    Ok(())
}

pub(crate) fn microvm_console_attachment_from_snapshot(
    attachment: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    requested: Option<&SerialConfigCli>,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    anyhow::ensure!(
        attachment.stable_id == MICROVM_CONSOLE_STABLE_ID
            && attachment.kind == MICROVM_CONSOLE_ATTACHMENT_KIND
            && attachment.length == 0,
        "snapshot has an invalid microVM console attachment"
    );
    let identity = std::str::from_utf8(&attachment.identity)
        .context("snapshot microVM console identity is not valid UTF-8")?;
    if attachment.reconnect_policy == "reconnect-client" {
        anyhow::ensure!(
            requested.is_some(),
            "snapshot requires an explicitly approved restore-time client attachment"
        );
    }
    let config = match (
        attachment.reconnect_policy.as_str(),
        attachment.identity_kind.as_str(),
    ) {
        ("recreate-listener", "unix-socket" | "named-pipe") => {
            SerialConfigCli::Pipe(PathBuf::from(identity))
        }
        ("recreate-listener", "tcp") => SerialConfigCli::Tcp(
            identity
                .parse()
                .context("snapshot microVM console TCP identity is invalid")?,
        ),
        ("reconnect-client", "unix-socket" | "named-pipe") => {
            SerialConfigCli::ConnectPipe(PathBuf::from(identity))
        }
        ("reconnect-client", "tcp") => SerialConfigCli::ConnectTcp(
            identity
                .parse()
                .context("snapshot microVM console TCP identity is invalid")?,
        ),
        ("require-inherited-attachment", "provider") => requested
            .cloned()
            .context("snapshot requires --virtio-console console as a replacement attachment")?,
        ("discard-while-disconnected", "disconnected") => SerialConfigCli::None,
        (policy, kind) => anyhow::bail!(
            "snapshot microVM console policy '{policy}' and backend kind '{kind}' are unsupported"
        ),
    };
    let reconstructed = microvm_console_attachment_from_cli(&config)?;
    anyhow::ensure!(
        reconstructed.2 == *attachment,
        "snapshot microVM console identity is not canonical"
    );
    if let Some(requested) = requested {
        let requested = microvm_console_attachment_from_cli(requested)?;
        anyhow::ensure!(
            requested.2 == *attachment,
            "restore-time virtio-console does not match the snapshot attachment"
        );
    }

    Ok(reconstructed)
}

pub(super) fn effective_microvm_console(
    requested: Option<&SerialConfigCli>,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<
    Option<(
        SerialConfigCli,
        virtio_resources::console::attachment::VirtioConsoleAttachment,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )>,
> {
    let Some(restore) = restore else {
        return requested
            .map(microvm_console_attachment_from_cli)
            .transpose();
    };
    let has_console = restore
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_CONSOLE_STABLE_ID);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_CONSOLE_STABLE_ID);
    anyhow::ensure!(
        has_console == saved_attachment.is_some(),
        "snapshot microVM console device and attachment inventories disagree"
    );
    let Some(saved_attachment) = saved_attachment else {
        anyhow::ensure!(
            requested.is_none(),
            "a restore-time virtio-console cannot be added to a snapshot without one"
        );
        return Ok(None);
    };
    Ok(Some(microvm_console_attachment_from_snapshot(
        saved_attachment,
        requested,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

    fn socket_attachment(path: &Path) -> openvmm_helpers::snapshot::microvm::SnapshotAttachment {
        openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: "virtio-console".to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "unix-socket".to_owned(),
            identity: path.to_string_lossy().into_owned().into_bytes(),
            length: 0,
            reconnect_timeout_ms: 0,
        }
    }

    #[test]
    fn socket_attachment_must_stay_in_snapshot_parent() {
        let allowed = tempfile::tempdir().unwrap();
        let allowed = fs_err::canonicalize(allowed.path()).unwrap();
        let snapshot = allowed.join("snapshot");
        let attachment = socket_attachment(&allowed.join("console.sock"));
        validate_microvm_console_attachment_namespace(&attachment, &snapshot).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let outside = fs_err::canonicalize(outside.path()).unwrap();
        let attachment = socket_attachment(&outside.join("console.sock"));
        assert!(validate_microvm_console_attachment_namespace(&attachment, &snapshot).is_err());
    }

    #[test]
    fn named_pipe_attachment_uses_dedicated_namespace() {
        let mut attachment = socket_attachment(Path::new("unused"));
        attachment.identity_kind = "named-pipe".to_owned();
        attachment.identity = b"//./pipe/openvmm-microvm-console0".to_vec();
        validate_microvm_console_attachment_namespace(&attachment, Path::new("snapshot")).unwrap();

        attachment.identity = b"//./pipe/unrelated".to_vec();
        assert!(
            validate_microvm_console_attachment_namespace(&attachment, Path::new("snapshot"))
                .is_err()
        );
    }

    #[test]
    fn client_attachment_is_required_and_bounded() {
        let requested = SerialConfigCli::ConnectTcp("127.0.0.1:5555".parse().unwrap());
        let (_, resource, snapshot) = microvm_console_attachment_from_cli(&requested).unwrap();
        assert!(snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "reconnect-client");
        assert_eq!(
            snapshot.reconnect_timeout_ms,
            openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS
        );
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::ReconnectClient
        );

        assert!(microvm_console_attachment_from_snapshot(&snapshot, None).is_err());
        let (restored, _, restored_snapshot) =
            microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).unwrap();
        assert!(matches!(restored, SerialConfigCli::ConnectTcp(_)));
        assert_eq!(restored_snapshot, snapshot);
    }

    #[test]
    fn tcp_attachment_rejects_non_loopback_addresses() {
        assert!(
            microvm_console_attachment_from_cli(&SerialConfigCli::Tcp(
                "0.0.0.0:5555".parse().unwrap()
            ))
            .is_err()
        );
        assert!(
            microvm_console_attachment_from_cli(&SerialConfigCli::ConnectTcp(
                "192.0.2.1:5555".parse().unwrap()
            ))
            .is_err()
        );
    }

    #[test]
    fn inherited_attachment_requires_matching_replacement() {
        let (_, _, snapshot) =
            microvm_console_attachment_from_cli(&SerialConfigCli::Console).unwrap();
        assert!(snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "require-inherited-attachment");
        assert!(microvm_console_attachment_from_snapshot(&snapshot, None).is_err());
        assert!(
            microvm_console_attachment_from_snapshot(&snapshot, Some(&SerialConfigCli::Console))
                .is_ok()
        );
        assert!(
            microvm_console_attachment_from_snapshot(
                &snapshot,
                Some(&SerialConfigCli::ConnectTcp(
                    "127.0.0.1:5555".parse().unwrap()
                ))
            )
            .is_err()
        );
    }

    #[test]
    fn disconnected_attachment_preserves_discard_policy() {
        let (config, resource, snapshot) =
            microvm_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        assert!(matches!(config, SerialConfigCli::None));
        assert!(!snapshot.required);
        assert_eq!(snapshot.reconnect_policy, "discard-while-disconnected");
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
        );

        let (restored, _, restored_snapshot) =
            microvm_console_attachment_from_snapshot(&snapshot, None).unwrap();
        assert!(matches!(restored, SerialConfigCli::None));
        assert_eq!(restored_snapshot, snapshot);
    }

    #[cfg(unix)]
    #[test]
    fn restore_bind_does_not_unlink_existing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let _existing = unix_socket::UnixListener::bind(&path).unwrap();
        assert!(crate::serial_io::connect::bind_serial_without_cleanup(&path).is_err());
        assert!(fs_err::symlink_metadata(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_guard_does_not_remove_replaced_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let listener = unix_socket::UnixListener::bind(&path).unwrap();
        let cleanup = MicrovmConsoleSocketCleanup::new(path.clone()).unwrap();
        fs_err::remove_file(&path).unwrap();
        let replacement = unix_socket::UnixListener::bind(&path).unwrap();

        assert!(cleanup.remove_if_owned().is_err());
        assert!(fs_err::symlink_metadata(&path).is_ok());
        drop((cleanup, replacement, listener));
    }
}
