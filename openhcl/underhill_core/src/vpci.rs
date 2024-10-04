// Copyright (C) Microsoft Corporation. All rights reserved.

//! HCL implementation of the VpciBusControl trait.

use async_trait::async_trait;
use guest_emulation_transport::GuestEmulationTransportClient;
use guid::Guid;
use vpci::bus_control::VpciBusControl;
use vpci::bus_control::VpciBusEvent;

#[derive(Debug)]
pub struct HclVpciBusControl {
    bus_instance_id: Guid,
    get: GuestEmulationTransportClient,
    notifier: mesh::Receiver<VpciBusEvent>,
}

impl HclVpciBusControl {
    pub async fn new(
        get: GuestEmulationTransportClient,
        bus_instance_id: Guid,
    ) -> anyhow::Result<Self> {
        let notifier = get.connect_to_vpci_event_source(bus_instance_id).await;
        Ok(Self {
            bus_instance_id,
            get,
            notifier,
        })
    }

    pub fn instance_id(&self) -> Guid {
        self.bus_instance_id
    }

    pub async fn update_vtl2_device_bind_state(&self, is_bound: bool) -> anyhow::Result<()> {
        self.get
            .report_vpci_device_binding_state(self.bus_instance_id, is_bound)
            .await?;
        Ok(())
    }
}

impl Drop for HclVpciBusControl {
    fn drop(&mut self) {
        self.get
            .disconnect_from_vpci_event_source(self.bus_instance_id);
    }
}

#[async_trait]
impl VpciBusControl for HclVpciBusControl {
    async fn offer_device(&self) -> anyhow::Result<()> {
        self.get.offer_vpci_device(self.bus_instance_id).await?;
        Ok(())
    }

    async fn revoke_device(&self) -> anyhow::Result<()> {
        self.get.revoke_vpci_device(self.bus_instance_id).await?;
        Ok(())
    }

    fn notifier(&mut self) -> &mut mesh::Receiver<VpciBusEvent> {
        &mut self.notifier
    }
}
