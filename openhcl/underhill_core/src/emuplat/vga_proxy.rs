// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::partition::OpenhclPartition;
use std::ops::RangeInclusive;
use std::sync::Arc;

pub struct UhRegisterHostIoFastPath(pub Arc<dyn OpenhclPartition>);

impl vga_proxy::RegisterHostIoPortFastPath for UhRegisterHostIoFastPath {
    fn register(&self, range: RangeInclusive<u16>) -> Box<dyn Send> {
        Box::new(self.0.register_host_io_port_fast_path(range))
    }
}

pub struct GetProxyVgaPciCfgAccess(pub guest_emulation_transport::GuestEmulationTransportClient);

#[async_trait::async_trait]
impl vga_proxy::ProxyVgaPciCfgAccess for GetProxyVgaPciCfgAccess {
    async fn vga_proxy_pci_read(&self, offset: u16) -> u32 {
        let val = self.0.vga_proxy_pci_read(offset).await;
        tracing::trace!(?val, "VGA proxy read result");
        val
    }

    async fn vga_proxy_pci_write(&self, offset: u16, value: u32) {
        self.0.vga_proxy_pci_write(offset, value).await
    }
}
