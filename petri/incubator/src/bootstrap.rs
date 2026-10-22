// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Incubator guest policy, shared by both VM launchers.

use crate::GUEST_SHARE_ROOT;
use crate::IncubatorConfig;
use crate::profile::IncubatorBackend;
use crate::profile::IncubatorBoot;
use anyhow::Context;
use petri::PetriInitrd;
use petri::initrd::InitrdFile;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const INIT_SCRIPT_NAME: &str = "incubator-init.sh";
const CA_CERTIFICATES_NAME: &str = "incubator-ca-certificates.crt";

#[derive(Clone, Copy)]
enum GuestNetwork {
    QemuUser,
    Consomme,
}

#[derive(Clone, Copy)]
enum AgentLocation<'a> {
    Initrd,
    Share(&'a str),
}

pub(crate) fn prepare_initrd(config: &IncubatorConfig) -> anyhow::Result<PetriInitrd> {
    let (network, agent) = match config.profile.incubator {
        IncubatorBackend::QemuTcg(_) => (
            GuestNetwork::QemuUser,
            AgentLocation::Share(&config.guest_pipette_path),
        ),
        IncubatorBackend::Openvmm(_) => (GuestNetwork::Consomme, AgentLocation::Initrd),
    };
    let pipette = if matches!(agent, AgentLocation::Initrd) {
        let relative = std::path::Path::new(&config.guest_pipette_path)
            .strip_prefix(GUEST_SHARE_ROOT)
            .context("embedded Pipette must be under the guest share")?;
        anyhow::ensure!(
            relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Pipette path must not escape the guest share"
        );
        Some(fs_err::read(config.share_dir.join(relative)).context("reading guest Pipette")?)
    } else {
        None
    };
    let ca_path = host_ca_certificates_path()?;
    let certificates = fs_err::read(&ca_path)
        .with_context(|| format!("reading host CA bundle {}", ca_path.display()))?;
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("host clock is before the Unix epoch")?
        .as_secs();
    let script = build_init_script(
        network,
        agent,
        matches!(config.profile.boot, IncubatorBoot::Mshv(_)),
        epoch,
    );
    let mut files = vec![
        InitrdFile {
            name: INIT_SCRIPT_NAME,
            contents: script.as_bytes(),
            permissions: 0o755,
        },
        InitrdFile {
            name: CA_CERTIFICATES_NAME,
            contents: &certificates,
            permissions: 0o644,
        },
    ];
    if let Some(pipette) = &pipette {
        files.push(InitrdFile {
            name: "pipette",
            contents: pipette,
            permissions: 0o755,
        });
    }
    PetriInitrd::prepare(
        &config.initrd,
        &files,
        &format!("/{INIT_SCRIPT_NAME}"),
        Some(&config.output_dir),
    )
}

fn build_init_script(
    network: GuestNetwork,
    agent: AgentLocation<'_>,
    require_mshv: bool,
    epoch: u64,
) -> String {
    let (address, gateway, dns) = match network {
        GuestNetwork::Consomme => ("10.0.0.2", "10.0.0.1", "10.0.0.1"),
        GuestNetwork::QemuUser => ("10.0.2.15", "10.0.2.2", "10.0.2.3"),
    };
    let mut script = String::from("#!/bin/sh\n");
    if matches!(agent, AgentLocation::Initrd) {
        script.push_str("set -eu\n");
    }
    script.push_str(&format!(
        "/bin/busybox --install /bin\n\
         mount -t devtmpfs none /dev\nmount -t proc none /proc\n\
         mount -t sysfs none /sys\nmkdir -p /dev/pts /root /tmp /etc\n\
         mount -t devpts devpts /dev/pts\ndate -u -s @{epoch}\n"
    ));
    if require_mshv {
        script.push_str("test -c /dev/mshv\nuname -a\n");
    }
    let pipette = match agent {
        AgentLocation::Initrd => "/pipette",
        AgentLocation::Share(path) => {
            // A shared agent needs its filesystem mounted before it can start.
            script.push_str(&format!(
                "mkdir -p {GUEST_SHARE_ROOT}\n\
             mount -t 9p -o trans=virtio,version=9p2000.L hostshare {GUEST_SHARE_ROOT}\n\
             cd {GUEST_SHARE_ROOT}\n"
            ));
            path
        }
    };
    let pipette = format!("'{}'", pipette.replace('\'', "'\\''"));
    script.push_str(&format!(
        "ip link set eth0 up\nip addr add {address}/24 dev eth0\n\
         ip route add default via {gateway}\n\
         echo 'nameserver {dns}' > /etc/resolv.conf\n\
         export HOME=/root\n\
         export SSL_CERT_FILE=/{CA_CERTIFICATES_NAME}\n\
         exec {pipette} --transport tcp\n"
    ));
    script
}

fn host_ca_certificates_path() -> anyhow::Result<PathBuf> {
    select_ca_bundle(
        std::env::var_os("SSL_CERT_FILE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
        &[
            "/etc/ssl/certs/ca-certificates.crt".into(),
            "/etc/pki/tls/certs/ca-bundle.crt".into(),
            "/etc/ssl/ca-bundle.pem".into(),
        ],
    )
}

fn select_ca_bundle(explicit: Option<PathBuf>, defaults: &[PathBuf]) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        anyhow::ensure!(
            path.is_file(),
            "SSL_CERT_FILE is not a file: {}",
            path.display()
        );
        return Ok(path);
    }
    defaults
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .context("could not find a host CA certificate bundle for the incubator")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn both_guests_receive_trust_and_clock_setup() {
        for (network, agent, mshv) in [
            (
                GuestNetwork::QemuUser,
                AgentLocation::Share("/share/agent's pipette"),
                false,
            ),
            (GuestNetwork::Consomme, AgentLocation::Initrd, false),
            (GuestNetwork::Consomme, AgentLocation::Initrd, true),
            (GuestNetwork::QemuUser, AgentLocation::Initrd, true),
        ] {
            let script = build_init_script(network, agent, mshv, 123456789);
            assert!(script.contains("export SSL_CERT_FILE=/incubator-ca-certificates.crt\n"));
            assert!(script.contains("date -u -s @123456789\n"));
            assert_eq!(script.contains("test -c /dev/mshv\n"), mshv);
            if matches!(agent, AgentLocation::Initrd) {
                assert!(script.contains("exec '/pipette' --transport tcp"));
                assert!(!script.contains("mount -t 9p"));
            } else {
                assert!(script.contains("exec '/share/agent'\\''s pipette' --transport tcp"));
                assert!(script.contains("mount -t 9p"));
            }
            assert_eq!(
                script.contains("ip addr add 10.0.0.2/24"),
                matches!(network, GuestNetwork::Consomme)
            );
            assert_eq!(
                script.contains("ip addr add 10.0.2.15/24"),
                matches!(network, GuestNetwork::QemuUser)
            );
        }
    }

    #[test]
    fn ca_selection_honors_explicit_bundle_and_reports_errors() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let explicit = temp.path().join("explicit");
        let default = temp.path().join("default");
        fs_err::write(&explicit, b"explicit")?;
        fs_err::write(&default, b"default")?;
        let defaults = [default.clone()];
        assert_eq!(
            select_ca_bundle(Some(explicit.clone()), &defaults)?,
            explicit
        );
        assert_eq!(select_ca_bundle(None, &defaults)?, default);
        assert!(select_ca_bundle(Some(temp.path().join("missing")), &defaults).is_err());
        assert!(select_ca_bundle(None, &[]).is_err());
        Ok(())
    }
}
