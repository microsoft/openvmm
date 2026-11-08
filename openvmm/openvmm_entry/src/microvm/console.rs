// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM virtio-console and control-console attachments.

use crate::Options;
use crate::cli_args::SerialConfigCli;
#[cfg(any(target_os = "linux", windows, test))]
use crate::serial_io;
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
pub(crate) const MICROVM_CONTROL_CONSOLE_STABLE_ID: &str = "console:microvm-control0";
pub(crate) const MICROVM_CONSOLE_ATTACHMENT_KIND: &str = "virtio-console";
pub(crate) const MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND: &str = "virtio-control-console";

/// Why a microVM console refuses a TCP endpoint. Unless host loopback is
/// denied, a guest's gateway maps the guest's TCP flows onto host loopback,
/// so any guest on the host whose egress policy permits the port could
/// connect to a loopback TCP listener, its own console's or another VM's, or
/// to the listener that a client endpoint connects to. The boot console does
/// not authenticate its client, so that guest would drive the console.
pub(crate) const MICROVM_CONSOLE_TCP_REFUSED: &str = "microVM consoles do not accept TCP endpoints, which guests can reach through the host-loopback mapping of their gateways; use listen=PATH or connect=PATH with a Unix socket or named pipe";

pub(super) fn microvm_console_attachments_share_endpoint(
    left: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    right: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
) -> bool {
    left.identity_kind == right.identity_kind
        && if left.identity_kind == "named-pipe" {
            std::str::from_utf8(&left.identity)
                .ok()
                .zip(std::str::from_utf8(&right.identity).ok())
                .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        } else {
            left.identity == right.identity
        }
}

pub(super) fn microvm_console_listener_replacement_matches(
    saved: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    requested: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
) -> bool {
    matches!(
        saved.reconnect_policy.as_str(),
        "recreate-listener" | "broker-authenticated-listener"
    ) && saved.stable_id == requested.stable_id
        && saved.kind == requested.kind
        && saved.required == requested.required
        && saved.reconnect_policy == requested.reconnect_policy
        && saved.identity_kind == requested.identity_kind
        && saved.length == requested.length
        && saved.reconnect_timeout_ms == requested.reconnect_timeout_ms
}

/// Returns whether `requested` is a listener that a restore attached to
/// `saved`, a disconnected boot console: a Unix socket or named pipe that
/// keeps discarding guest output while no client is connected, so that only
/// the endpoint differs from the saved attachment.
pub(super) fn microvm_console_discarding_listener_matches(
    saved: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    requested: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
) -> bool {
    saved.stable_id == MICROVM_CONSOLE_STABLE_ID
        && saved.kind == MICROVM_CONSOLE_ATTACHMENT_KIND
        && saved.reconnect_policy == "discard-while-disconnected"
        && saved.identity_kind == "disconnected"
        && saved.stable_id == requested.stable_id
        && saved.kind == requested.kind
        && saved.required == requested.required
        && saved.reconnect_policy == requested.reconnect_policy
        && matches!(
            requested.identity_kind.as_str(),
            "unix-socket" | "named-pipe"
        )
        && saved.length == requested.length
        && saved.reconnect_timeout_ms == requested.reconnect_timeout_ms
}

/// Returns the attachment of a listener at `requested`, a Unix socket or named
/// pipe, that a restore attaches to a disconnected boot console. Like the
/// disconnected attachment, it discards guest output while no client is
/// connected, so the guest never waits for a client.
fn microvm_console_discarding_listener(
    requested: &SerialConfigCli,
) -> anyhow::Result<ConsoleEndpoint> {
    use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

    anyhow::ensure!(
        matches!(requested, SerialConfigCli::Pipe(_)),
        "a restore attaches only a socket or pipe listener to a disconnected virtio-console"
    );
    let (effective, mut attachment, mut snapshot) = microvm_console_attachment_from_cli(requested)?;
    attachment.reconnect_policy = VirtioConsoleReconnectPolicy::DiscardWhileDisconnected;
    snapshot.reconnect_policy = "discard-while-disconnected".to_owned();
    Ok((effective, attachment, snapshot))
}

/// Returns what the boot console's device does with guest output while no
/// client is connected to its host endpoint: a disconnected attachment, or a
/// listener that a restore attached to one, discards it, and every other
/// attachment retains it until a client connects.
pub(crate) fn microvm_console_disconnect_policy(
    attachment: &virtio_resources::console::attachment::VirtioConsoleAttachment,
) -> virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy {
    use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
    use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;

    if attachment.reconnect_policy == VirtioConsoleReconnectPolicy::DiscardWhileDisconnected {
        VirtioConsoleDisconnectPolicy::Discard
    } else {
        VirtioConsoleDisconnectPolicy::Retain
    }
}

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
        SerialConfigCli::Tcp(_) | SerialConfigCli::ConnectTcp(_) => {
            anyhow::bail!("{MICROVM_CONSOLE_TCP_REFUSED}")
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
            "microVM virtio-console requires listen=PATH, connect=PATH, console, or none"
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

fn microvm_control_console_attachment_from_cli(
    config: &SerialConfigCli,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    anyhow::ensure!(
        matches!(config, SerialConfigCli::Pipe(_) | SerialConfigCli::None),
        "microVM control console requires listen=... or none"
    );
    let config = match config {
        #[cfg(unix)]
        SerialConfigCli::Pipe(path) => {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;

            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()
                    .context("failed to resolve current directory for control endpoint")?
                    .join(path)
            };
            let parent = absolute
                .parent()
                .context("microVM control endpoint has no parent directory")?;
            let metadata = fs_err::symlink_metadata(parent).with_context(|| {
                format!(
                    "failed to inspect microVM control endpoint parent {}",
                    parent.display()
                )
            })?;
            anyhow::ensure!(
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == pal::unix::effective_user_id()
                    && metadata.permissions().mode() & 0o7777 == 0o700,
                "microVM control endpoint parent must be a non-symlink directory owned by OpenVMM with mode 0700"
            );
            SerialConfigCli::Pipe(canonical_microvm_console_path(&absolute)?)
        }
        _ => config.clone(),
    };
    let (effective, attachment, mut snapshot) = microvm_console_attachment_from_cli_with_identity(
        &config,
        MICROVM_CONTROL_CONSOLE_STABLE_ID,
        MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
    )?;
    snapshot.reconnect_policy = match config {
        SerialConfigCli::Pipe(_) => "broker-authenticated-listener",
        SerialConfigCli::None => "broker-disconnected",
        _ => unreachable!("control-console backend was validated"),
    }
    .to_owned();
    Ok((effective, attachment, snapshot))
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
        "provider" => {}
        "disconnected" => {}
        kind => anyhow::bail!("unsupported microVM console identity kind '{kind}'"),
    }
    Ok(())
}

fn microvm_console_attachment_from_snapshot_with_identity(
    attachment: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    requested: Option<&SerialConfigCli>,
    stable_id: &'static str,
    attachment_kind: &'static str,
    control_console: bool,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    anyhow::ensure!(
        attachment.stable_id == stable_id
            && attachment.kind == attachment_kind
            && attachment.length == 0,
        "snapshot has an invalid microVM console attachment"
    );
    // Older captures could record a TCP endpoint, which a restore would
    // expose again.
    anyhow::ensure!(
        attachment.identity_kind != "tcp",
        "snapshot microVM console has a TCP endpoint, which guests can reach through the host-loopback mapping of their gateways; capture the snapshot again with a Unix socket or named pipe"
    );
    let identity = std::str::from_utf8(&attachment.identity)
        .context("snapshot microVM console identity is not valid UTF-8")?;
    if attachment.reconnect_policy == "reconnect-client" {
        anyhow::ensure!(
            requested.is_some(),
            "snapshot requires an explicitly approved restore-time client attachment"
        );
    }
    if control_console
        && matches!(
            attachment.reconnect_policy.as_str(),
            "broker-authenticated-listener"
        )
    {
        anyhow::ensure!(
            requested.is_some(),
            "snapshot requires an explicitly approved restore-time control attachment"
        );
    }
    if matches!(
        attachment.reconnect_policy.as_str(),
        "recreate-listener" | "broker-authenticated-listener"
    ) {
        if let Some(requested) = requested {
            let requested = if control_console {
                microvm_control_console_attachment_from_cli(requested)?
            } else {
                microvm_console_attachment_from_cli(requested)?
            };
            anyhow::ensure!(
                microvm_console_listener_replacement_matches(attachment, &requested.2),
                "restore-time virtio-console listener does not match the snapshot attachment contract"
            );
            return Ok(requested);
        }
    }
    let config = match (
        attachment.reconnect_policy.as_str(),
        attachment.identity_kind.as_str(),
    ) {
        ("recreate-listener", "unix-socket" | "named-pipe")
        | ("broker-authenticated-listener", "unix-socket" | "named-pipe")
            if !control_console
                || attachment.reconnect_policy == "broker-authenticated-listener" =>
        {
            SerialConfigCli::Pipe(PathBuf::from(identity))
        }
        ("reconnect-client", "unix-socket" | "named-pipe") if !control_console => {
            SerialConfigCli::ConnectPipe(PathBuf::from(identity))
        }
        ("require-inherited-attachment", "provider") => requested
            .cloned()
            .context("snapshot requires --virtio-console console as a replacement attachment")?,
        ("discard-while-disconnected", "disconnected")
        | ("broker-disconnected", "disconnected")
            if !control_console || attachment.reconnect_policy == "broker-disconnected" =>
        {
            SerialConfigCli::None
        }
        (policy, kind) => anyhow::bail!(
            "snapshot microVM console policy '{policy}' and backend kind '{kind}' are unsupported"
        ),
    };
    let reconstructed = if control_console {
        microvm_control_console_attachment_from_cli(&config)?
    } else {
        microvm_console_attachment_from_cli(&config)?
    };
    anyhow::ensure!(
        reconstructed.2 == *attachment,
        "snapshot microVM console identity is not canonical"
    );
    if let Some(requested) = requested {
        // A restore may attach a socket or pipe listener to a disconnected
        // boot console, which then keeps discarding guest output while no
        // client is connected.
        if !control_console
            && attachment.reconnect_policy == "discard-while-disconnected"
            && matches!(requested, SerialConfigCli::Pipe(_))
        {
            let listener = microvm_console_discarding_listener(requested)?;
            anyhow::ensure!(
                microvm_console_discarding_listener_matches(attachment, &listener.2),
                "restore-time virtio-console listener does not match the snapshot attachment contract"
            );
            return Ok(listener);
        }
        let requested = if control_console {
            microvm_control_console_attachment_from_cli(requested)?
        } else {
            microvm_console_attachment_from_cli(requested)?
        };
        anyhow::ensure!(
            requested.2 == *attachment,
            "restore-time virtio-console does not match the snapshot attachment"
        );
    }

    Ok(reconstructed)
}

#[cfg(test)]
pub(crate) fn microvm_console_attachment_from_snapshot(
    attachment: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    requested: Option<&SerialConfigCli>,
) -> anyhow::Result<(
    SerialConfigCli,
    virtio_resources::console::attachment::VirtioConsoleAttachment,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    microvm_console_attachment_from_snapshot_with_identity(
        attachment,
        requested,
        MICROVM_CONSOLE_STABLE_ID,
        MICROVM_CONSOLE_ATTACHMENT_KIND,
        false,
    )
}

fn effective_microvm_console_with_identity(
    requested: Option<&SerialConfigCli>,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
    stable_id: &'static str,
    attachment_kind: &'static str,
    control_console: bool,
) -> anyhow::Result<
    Option<(
        SerialConfigCli,
        virtio_resources::console::attachment::VirtioConsoleAttachment,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )>,
> {
    let Some(restore) = restore else {
        return requested
            .map(|requested| {
                if control_console {
                    microvm_control_console_attachment_from_cli(requested)
                } else {
                    microvm_console_attachment_from_cli(requested)
                }
            })
            .transpose();
    };
    let has_console = restore
        .devices
        .iter()
        .any(|device| device.stable_id == stable_id);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == stable_id);
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
    Ok(Some(
        microvm_console_attachment_from_snapshot_with_identity(
            saved_attachment,
            requested,
            stable_id,
            attachment_kind,
            control_console,
        )?,
    ))
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
    effective_microvm_console_with_identity(
        requested,
        restore,
        MICROVM_CONSOLE_STABLE_ID,
        MICROVM_CONSOLE_ATTACHMENT_KIND,
        false,
    )
}

pub(super) fn effective_microvm_control_console(
    requested: Option<&SerialConfigCli>,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<
    Option<(
        SerialConfigCli,
        virtio_resources::console::attachment::VirtioConsoleAttachment,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )>,
> {
    effective_microvm_console_with_identity(
        requested,
        restore,
        MICROVM_CONTROL_CONSOLE_STABLE_ID,
        MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
        true,
    )
}

fn random_nonzero_bytes<const N: usize>(description: &'static str) -> anyhow::Result<[u8; N]> {
    loop {
        let mut bytes = [0; N];
        getrandom::fill(&mut bytes).with_context(|| format!("failed to generate {description}"))?;
        if bytes != [0; N] {
            return Ok(bytes);
        }
    }
}

pub(super) fn microvm_control_broker_config(
    opt: &Options,
    endpoint: &SerialConfigCli,
) -> anyhow::Result<virtio_resources::console::control::VirtioControlConsoleBrokerConfig> {
    let capability = if matches!(endpoint, SerialConfigCli::None) {
        random_nonzero_bytes("disconnected control-console capability")?
    } else {
        anyhow::ensure!(
            opt.microvm.microvm_control_auth_stdin,
            "live microVM control console requires --microvm-control-auth-stdin"
        );
        #[cfg(any(target_os = "linux", windows))]
        {
            serial_io::microvm::read_control_capability_from_stdin()
                .context("failed to read control-console authentication capability from stdin")?
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            anyhow::bail!("secure live microVM control consoles are unavailable on this platform")
        }
    };
    #[cfg(target_os = "linux")]
    let expected_peer_identity =
        serial_core::LocalPeerIdentity::UnixUid(pal::unix::effective_user_id());
    #[cfg(windows)]
    let expected_peer_identity = {
        let sid = pal::windows::security::current_process_user_sid()
            .context("failed to resolve the OpenVMM process user SID")?;
        let (bytes, length) = sid.to_fixed_bytes();
        serial_core::LocalPeerIdentity::windows_sid(bytes, length)
            .context("failed to encode the OpenVMM process user SID")?
    };
    #[cfg(not(any(target_os = "linux", windows)))]
    let expected_peer_identity = serial_core::LocalPeerIdentity::Unsupported;

    Ok(
        virtio_resources::console::control::VirtioControlConsoleBrokerConfig {
            instance_id: random_nonzero_bytes("control-console instance ID")?,
            capability,
            expected_peer_identity,
            auth_timeout_ms: opt.microvm.microvm_control_auth_timeout_ms,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use test_with_tracing::test;
    use virtio_resources::console::attachment::VirtioConsoleAttachmentMode;
    use virtio_resources::console::attachment::VirtioConsoleBackendKind;
    use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
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
    fn named_pipe_endpoint_identity_is_case_insensitive() {
        let mut left = socket_attachment(Path::new("unused"));
        left.identity_kind = "named-pipe".to_owned();
        left.identity = b"//./pipe/openvmm-microvm-Control".to_vec();
        let mut right = left.clone();
        right.identity = b"//./pipe/openvmm-microvm-control".to_vec();
        assert!(microvm_console_attachments_share_endpoint(&left, &right));

        left.identity_kind = "unix-socket".to_owned();
        right.identity_kind = "unix-socket".to_owned();
        assert!(!microvm_console_attachments_share_endpoint(&left, &right));
    }

    #[test]
    fn listener_replacement_contract_changes_only_endpoint_identity() {
        let saved = socket_attachment(Path::new("source.sock"));
        let mut requested = saved.clone();
        requested.identity = b"restored.sock".to_vec();
        assert!(microvm_console_listener_replacement_matches(
            &saved, &requested
        ));

        requested.kind = MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND.to_owned();
        assert!(!microvm_console_listener_replacement_matches(
            &saved, &requested
        ));
    }

    #[test]
    fn client_attachment_is_required_and_bounded() {
        let (_directory, requested) = client_endpoint();
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
        assert!(matches!(restored, SerialConfigCli::ConnectPipe(_)));
        assert_eq!(restored_snapshot, snapshot);
    }

    /// Returns a client endpoint of this host's kind, and the directory that
    /// holds a Unix socket's path.
    fn client_endpoint() -> (Option<tempfile::TempDir>, SerialConfigCli) {
        #[cfg(windows)]
        {
            (
                None,
                SerialConfigCli::ConnectPipe(PathBuf::from(
                    "//./pipe/openvmm-microvm-client-console",
                )),
            )
        }
        #[cfg(not(windows))]
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("client.sock");
            (Some(directory), SerialConfigCli::ConnectPipe(path))
        }
    }

    #[cfg(unix)]
    #[test]
    fn listener_attachment_accepts_fresh_restore_identity_after_source_cleanup() {
        let source_directory = tempfile::tempdir().unwrap();
        let source = SerialConfigCli::Pipe(source_directory.path().join("source.sock"));
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&source).unwrap();
        source_directory.close().unwrap();
        let restore_directory = tempfile::tempdir().unwrap();
        let requested = SerialConfigCli::Pipe(restore_directory.path().join("restored.sock"));

        let (restored, resource, restored_attachment) =
            microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).unwrap();

        let SerialConfigCli::Pipe(restored_path) = restored else {
            panic!("restore did not retain a listener");
        };
        assert!(restored_path.ends_with("restored.sock"));
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::RecreateListener
        );
        assert_ne!(restored_attachment.identity, snapshot.identity);
        assert!(microvm_console_listener_replacement_matches(
            &snapshot,
            &restored_attachment
        ));
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_control_listener_accepts_fresh_identity_after_source_cleanup() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            source_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let source = SerialConfigCli::Pipe(source_directory.path().join("source-control.sock"));
        let (_, _, snapshot) = microvm_control_console_attachment_from_cli(&source).unwrap();
        source_directory.close().unwrap();
        let restore_directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            restore_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let requested =
            SerialConfigCli::Pipe(restore_directory.path().join("restored-control.sock"));

        let (restored, resource, restored_attachment) =
            microvm_console_attachment_from_snapshot_with_identity(
                &snapshot,
                Some(&requested),
                MICROVM_CONTROL_CONSOLE_STABLE_ID,
                MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
                true,
            )
            .unwrap();

        let SerialConfigCli::Pipe(restored_path) = restored else {
            panic!("restore did not retain a control listener");
        };
        assert!(restored_path.ends_with("restored-control.sock"));
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::RecreateListener
        );
        assert_eq!(
            restored_attachment.reconnect_policy,
            "broker-authenticated-listener"
        );
        assert_ne!(restored_attachment.identity, snapshot.identity);
        assert!(microvm_console_listener_replacement_matches(
            &snapshot,
            &restored_attachment
        ));
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_listener_accepts_fresh_restore_identity() {
        let source =
            SerialConfigCli::Pipe(PathBuf::from("//./pipe/openvmm-microvm-source-console"));
        let requested =
            SerialConfigCli::Pipe(PathBuf::from("//./pipe/openvmm-microvm-restored-console"));
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&source).unwrap();

        let (restored, resource, restored_attachment) =
            microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).unwrap();

        let SerialConfigCli::Pipe(restored_path) = restored else {
            panic!("restore did not retain a named-pipe listener");
        };
        assert_eq!(
            restored_path,
            PathBuf::from("//./pipe/openvmm-microvm-restored-console")
        );
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::RecreateListener
        );
        assert_ne!(restored_attachment.identity, snapshot.identity);
        assert!(microvm_console_listener_replacement_matches(
            &snapshot,
            &restored_attachment
        ));
    }

    #[cfg(windows)]
    #[test]
    fn authenticated_named_pipe_listener_accepts_fresh_restore_identity() {
        let source =
            SerialConfigCli::Pipe(PathBuf::from("//./pipe/openvmm-microvm-source-control"));
        let requested =
            SerialConfigCli::Pipe(PathBuf::from("//./pipe/openvmm-microvm-restored-control"));
        let (_, _, snapshot) = microvm_control_console_attachment_from_cli(&source).unwrap();

        let (restored, resource, restored_attachment) =
            microvm_console_attachment_from_snapshot_with_identity(
                &snapshot,
                Some(&requested),
                MICROVM_CONTROL_CONSOLE_STABLE_ID,
                MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
                true,
            )
            .unwrap();

        let SerialConfigCli::Pipe(restored_path) = restored else {
            panic!("restore did not retain an authenticated named-pipe listener");
        };
        assert_eq!(
            restored_path,
            PathBuf::from("//./pipe/openvmm-microvm-restored-control")
        );
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::RecreateListener
        );
        assert_eq!(
            restored_attachment.reconnect_policy,
            "broker-authenticated-listener"
        );
        assert_ne!(restored_attachment.identity, snapshot.identity);
        assert!(microvm_console_listener_replacement_matches(
            &snapshot,
            &restored_attachment
        ));
    }

    #[cfg(unix)]
    #[test]
    fn listener_attachment_rejects_restore_client_substitution() {
        let directory = tempfile::tempdir().unwrap();
        let source = SerialConfigCli::Pipe(directory.path().join("source.sock"));
        let requested = SerialConfigCli::ConnectPipe(directory.path().join("restored.sock"));
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&source).unwrap();

        assert!(microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).is_err());
    }

    #[test]
    fn tcp_attachments_are_refused() {
        // A guest reaches host loopback through its gateway, so no address,
        // loopback included, makes a TCP listener or its client safe.
        for address in [
            "127.0.0.1:5555",
            "[::1]:5555",
            "0.0.0.0:5555",
            "192.0.2.1:5555",
        ] {
            let address = address.parse().unwrap();
            for config in [
                SerialConfigCli::Tcp(address),
                SerialConfigCli::ConnectTcp(address),
            ] {
                let error = microvm_console_attachment_from_cli(&config).unwrap_err();
                assert_eq!(error.to_string(), MICROVM_CONSOLE_TCP_REFUSED, "{config:?}");
            }
        }
    }

    #[test]
    fn tcp_snapshot_attachments_are_refused_at_restore() {
        let listener = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: MICROVM_CONSOLE_ATTACHMENT_KIND.to_owned(),
            required: false,
            reconnect_policy: "recreate-listener".to_owned(),
            identity_kind: "tcp".to_owned(),
            identity: b"127.0.0.1:5555".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        };
        let client = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            required: true,
            reconnect_policy: "reconnect-client".to_owned(),
            reconnect_timeout_ms: openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
            ..listener.clone()
        };
        let address = "127.0.0.1:5555".parse().unwrap();
        // Neither the saved endpoint nor a replacement of any kind restores.
        for attachment in [&listener, &client] {
            for requested in [
                None,
                Some(SerialConfigCli::Tcp(address)),
                Some(SerialConfigCli::ConnectTcp(address)),
                Some(SerialConfigCli::None),
            ] {
                let error =
                    microvm_console_attachment_from_snapshot(attachment, requested.as_ref())
                        .unwrap_err();
                assert!(
                    error.to_string().contains("TCP endpoint"),
                    "{}: {error}",
                    attachment.reconnect_policy
                );
            }
            assert!(
                validate_microvm_console_attachment_namespace(attachment, Path::new("snapshot"))
                    .is_err()
            );
        }
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
            microvm_console_attachment_from_snapshot(&snapshot, Some(&SerialConfigCli::None))
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

    /// Asserts that `restored`, what a restore attached to a disconnected
    /// boot console `snapshot`, is a listener at `endpoint` of the backend
    /// `backend_kind`, which discards guest output while no client is
    /// connected, and which differs from the saved attachment in its endpoint
    /// alone.
    fn assert_discarding_listener(
        snapshot: &openvmm_helpers::snapshot::microvm::SnapshotAttachment,
        restored: &ConsoleEndpoint,
        endpoint: &Path,
        backend_kind: VirtioConsoleBackendKind,
    ) {
        let (config, resource, restored_attachment) = restored;
        let SerialConfigCli::Pipe(path) = config else {
            panic!("restore did not attach a listener");
        };
        assert!(path.ends_with(endpoint.file_name().unwrap()));
        assert_eq!(resource.mode, VirtioConsoleAttachmentMode::Listen);
        assert_eq!(resource.backend_kind, backend_kind);
        assert_eq!(
            resource.reconnect_policy,
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
        );
        assert!(!resource.required);
        assert_eq!(resource.reconnect_timeout_ms, 0);
        assert_eq!(
            microvm_console_disconnect_policy(resource),
            VirtioConsoleDisconnectPolicy::Discard
        );
        assert_eq!(
            restored_attachment.reconnect_policy,
            "discard-while-disconnected"
        );
        assert_ne!(restored_attachment.identity, snapshot.identity);
        assert!(microvm_console_discarding_listener_matches(
            snapshot,
            restored_attachment
        ));
        assert!(!microvm_console_listener_replacement_matches(
            snapshot,
            restored_attachment
        ));
    }

    #[cfg(unix)]
    #[test]
    fn disconnected_attachment_takes_a_restore_listener_that_keeps_discarding() {
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("clone.console");

        let restored = microvm_console_attachment_from_snapshot(
            &snapshot,
            Some(&SerialConfigCli::Pipe(endpoint.clone())),
        )
        .unwrap();

        assert_discarding_listener(
            &snapshot,
            &restored,
            &endpoint,
            VirtioConsoleBackendKind::UnixSocket,
        );
        assert_eq!(restored.2.identity_kind, "unix-socket");
        // The listener's socket must be beside the snapshot, as a listener
        // that a snapshot recorded must be.
        validate_microvm_console_attachment_namespace(
            &restored.2,
            &directory.path().join("snapshot"),
        )
        .unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(
            validate_microvm_console_attachment_namespace(
                &restored.2,
                &elsewhere.path().join("snapshot")
            )
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn disconnected_attachment_takes_a_restore_pipe_listener_that_keeps_discarding() {
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        let endpoint = PathBuf::from("//./pipe/openvmm-microvm-clone-console");

        let restored = microvm_console_attachment_from_snapshot(
            &snapshot,
            Some(&SerialConfigCli::Pipe(endpoint.clone())),
        )
        .unwrap();

        assert_discarding_listener(
            &snapshot,
            &restored,
            &endpoint,
            VirtioConsoleBackendKind::NamedPipe,
        );
        assert_eq!(restored.2.identity_kind, "named-pipe");
        assert!(
            microvm_console_attachment_from_snapshot(
                &snapshot,
                Some(&SerialConfigCli::Pipe(PathBuf::from("//./pipe/unrelated")))
            )
            .is_err()
        );
    }

    #[test]
    fn disconnected_attachment_refuses_every_other_restore_attachment() {
        let (_, _, snapshot) = microvm_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        #[cfg(unix)]
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let client = directory.path().join("client.console");
        #[cfg(windows)]
        let client = PathBuf::from("//./pipe/openvmm-microvm-client-console");
        for (requested, what) in [
            (
                SerialConfigCli::Tcp("127.0.0.1:5555".parse().unwrap()),
                "a TCP listener",
            ),
            (
                SerialConfigCli::ConnectTcp("127.0.0.1:5555".parse().unwrap()),
                "a TCP client",
            ),
            (
                SerialConfigCli::ConnectPipe(client),
                "a socket or pipe client",
            ),
            (SerialConfigCli::Console, "the terminal"),
        ] {
            assert!(
                microvm_console_attachment_from_snapshot(&snapshot, Some(&requested)).is_err(),
                "a disconnected boot console took {what} at a restore"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn disconnected_control_console_refuses_a_restore_listener() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_, _, snapshot) =
            microvm_control_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let requested = SerialConfigCli::Pipe(directory.path().join("control.sock"));

        assert!(
            microvm_console_attachment_from_snapshot_with_identity(
                &snapshot,
                Some(&requested),
                MICROVM_CONTROL_CONSOLE_STABLE_ID,
                MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn only_a_disconnected_boot_console_takes_a_discarding_listener() {
        let disconnected = microvm_console_attachment_from_cli(&SerialConfigCli::None)
            .unwrap()
            .2;
        let listener = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            reconnect_policy: "discard-while-disconnected".to_owned(),
            identity_kind: "unix-socket".to_owned(),
            identity: b"/snapshots/clone.console".to_vec(),
            ..disconnected.clone()
        };
        assert!(microvm_console_discarding_listener_matches(
            &disconnected,
            &listener
        ));

        // A listener that retains output, or whose contract changes beyond
        // its endpoint, is no discarding replacement.
        let retaining = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            reconnect_policy: "recreate-listener".to_owned(),
            ..listener.clone()
        };
        let required = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            required: true,
            ..listener.clone()
        };
        let tcp = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            identity_kind: "tcp".to_owned(),
            identity: b"127.0.0.1:5555".to_vec(),
            ..listener.clone()
        };
        for requested in [&retaining, &required, &tcp, &disconnected] {
            assert!(!microvm_console_discarding_listener_matches(
                &disconnected,
                requested
            ));
        }

        // Neither does a saved listener, nor the control console.
        let saved_listener = socket_attachment(Path::new("source.sock"));
        assert!(!microvm_console_discarding_listener_matches(
            &saved_listener,
            &listener
        ));
        let control = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: MICROVM_CONTROL_CONSOLE_STABLE_ID.to_owned(),
            kind: MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND.to_owned(),
            ..disconnected
        };
        let control_listener = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: MICROVM_CONTROL_CONSOLE_STABLE_ID.to_owned(),
            kind: MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND.to_owned(),
            ..listener
        };
        assert!(!microvm_console_discarding_listener_matches(
            &control,
            &control_listener
        ));
    }

    #[test]
    fn control_attachment_has_distinct_identity_and_rejects_tcp() {
        let (_, resource, snapshot) =
            microvm_control_console_attachment_from_cli(&SerialConfigCli::None).unwrap();
        assert_eq!(resource.stable_id, MICROVM_CONTROL_CONSOLE_STABLE_ID);
        assert_eq!(snapshot.stable_id, MICROVM_CONTROL_CONSOLE_STABLE_ID);
        assert_eq!(snapshot.kind, MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND);
        assert_ne!(
            snapshot.stable_id,
            microvm_console_attachment_from_cli(&SerialConfigCli::None)
                .unwrap()
                .2
                .stable_id
        );

        let (_, restored_resource, restored_snapshot) =
            microvm_console_attachment_from_snapshot_with_identity(
                &snapshot,
                None,
                MICROVM_CONTROL_CONSOLE_STABLE_ID,
                MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND,
                true,
            )
            .unwrap();
        assert_eq!(restored_resource.stable_id, resource.stable_id);
        assert_eq!(restored_snapshot, snapshot);

        assert!(
            microvm_control_console_attachment_from_cli(&SerialConfigCli::Tcp(
                "127.0.0.1:5555".parse().unwrap()
            ))
            .is_err()
        );
    }

    #[test]
    fn disconnected_control_console_uses_fresh_nonzero_runtime_secrets() {
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--virtio-console",
            "none",
            "--microvm-control-console",
            "none",
        ])
        .unwrap();
        let first = microvm_control_broker_config(&options, &SerialConfigCli::None).unwrap();
        let second = microvm_control_broker_config(&options, &SerialConfigCli::None).unwrap();
        assert_ne!(first.instance_id, [0; 16]);
        assert_ne!(second.instance_id, [0; 16]);
        assert_ne!(first.instance_id, second.instance_id);
        assert_ne!(first.capability, [0; 32]);
        assert_ne!(second.capability, [0; 32]);
    }

    #[cfg(unix)]
    #[test]
    fn restore_bind_does_not_unlink_existing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.sock");
        let _existing = unix_socket::UnixListener::bind(&path).unwrap();
        assert!(serial_io::connect::bind_serial_without_cleanup(&path).is_err());
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
