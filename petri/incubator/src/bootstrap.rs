// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolve standalone guest inputs for Petri's shared Linux bootstrap.

use crate::GUEST_SHARE_ROOT;
use crate::IncubatorConfig;
use crate::profile::IncubatorBackend;
use crate::profile::IncubatorBoot;
use anyhow::Context;
use petri::PetriInitrd;
use petri::linux_bootstrap::AgentLocation;
use petri::linux_bootstrap::GuestNetwork;
use std::path::Path;

pub(crate) fn prepare_initrd(config: &IncubatorConfig) -> anyhow::Result<PetriInitrd> {
    let pipette;
    let (network, agent) = match config.profile.incubator {
        IncubatorBackend::QemuTcg(_) => (
            GuestNetwork::QemuUser,
            AgentLocation::Share(&config.guest_pipette_path),
        ),
        IncubatorBackend::Openvmm(_) => {
            let relative = Path::new(&config.guest_pipette_path)
                .strip_prefix(GUEST_SHARE_ROOT)
                .context("embedded Pipette must be under the guest share")?;
            anyhow::ensure!(
                relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
                "Pipette path must not escape the guest share"
            );
            pipette = config.share_dir.join(relative);
            (GuestNetwork::Consomme, AgentLocation::Initrd(&pipette))
        }
    };
    petri::linux_bootstrap::prepare_initrd(
        &config.initrd,
        network,
        agent,
        matches!(config.profile.boot, IncubatorBoot::Mshv(_)),
        Some(&config.output_dir),
    )
}
