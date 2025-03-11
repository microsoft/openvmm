// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interface to BNIC (Basic NIC) MANA commands.

use crate::gdma_driver::GdmaDriver;
use crate::mana::ResourceArena;
use crate::resources::Resource;
use gdma_defs::bnic::ManaCfgRxSteerReq;
use gdma_defs::bnic::ManaCommandCode;
use gdma_defs::bnic::ManaConfigVportReq;
use gdma_defs::bnic::ManaConfigVportResp;
use gdma_defs::bnic::ManaCreateWqobjReq;
use gdma_defs::bnic::ManaCreateWqobjResp;
use gdma_defs::bnic::ManaDestroyWqobjReq;
use gdma_defs::bnic::ManaMoveFilterVTL2PrivilegedReq;
use gdma_defs::bnic::ManaQueryDeviceCfgReq;
use gdma_defs::bnic::ManaQueryDeviceCfgResp;
use gdma_defs::bnic::ManaQueryFilterStateReq;
use gdma_defs::bnic::ManaQueryFilterStateResponse;
use gdma_defs::bnic::ManaQueryStatisticsRequest;
use gdma_defs::bnic::ManaQueryStatisticsResponse;
use gdma_defs::bnic::ManaQueryVportCfgReq;
use gdma_defs::bnic::ManaQueryVportCfgResp;
use gdma_defs::bnic::ManaSetVportSerialNo;
use gdma_defs::bnic::MANA_VTL2_ASSIGN_SERIAL_NUMBER_REQUEST_V1;
use gdma_defs::bnic::MANA_VTL2_ASSIGN_SERIAL_NUMBER_RESPONSE_V1;
use gdma_defs::bnic::MANA_VTL2_MOVE_FILTER_REQUEST_V2;
use gdma_defs::bnic::MANA_VTL2_MOVE_FILTER_RESPONSE_V1;
use gdma_defs::GdmaDevId;
use gdma_defs::GdmaQueueType;
use gdma_defs::GdmaReqHdr;
use user_driver::DeviceBacking;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub struct BnicDriver<'a, T: DeviceBacking> {
    gdma: &'a mut GdmaDriver<T>,
    dev_id: GdmaDevId,
}

impl<'a, T: DeviceBacking> BnicDriver<'a, T> {
    pub fn new(gdma: &'a mut GdmaDriver<T>, dev_id: GdmaDevId) -> Self {
        Self { gdma, dev_id }
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn query_dev_config(&mut self) -> anyhow::Result<ManaQueryDeviceCfgResp> {
        let resp: ManaQueryDeviceCfgResp = self
            .gdma
            .request(
                ManaCommandCode::MANA_QUERY_DEV_CONFIG.0,
                self.dev_id,
                ManaQueryDeviceCfgReq {
                    mn_drv_cap_flags1: 0,
                    mn_drv_cap_flags2: 0,
                    mn_drv_cap_flags3: 0,
                    mn_drv_cap_flags4: 0,
                    proto_major_ver: 1,
                    proto_minor_ver: 0,
                    proto_micro_ver: 0,
                    reserved: 0,
                },
            )
            .await?;
        Ok(resp)
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn config_vport_tx(
        &mut self,
        vport: u64,
        pdid: u32,
        doorbell_pageid: u32,
    ) -> anyhow::Result<ManaConfigVportResp> {
        let resp: ManaConfigVportResp = self
            .gdma
            .request(
                ManaCommandCode::MANA_CONFIG_VPORT_TX.0,
                self.dev_id,
                ManaConfigVportReq {
                    vport,
                    pdid,
                    doorbell_pageid,
                },
            )
            .await?;
        Ok(resp)
    }

    #[tracing::instrument(skip(self, arena, config), level = "debug", err)]
    pub async fn create_wq_obj(
        &mut self,
        arena: &mut ResourceArena,
        vport: u64,
        wq_type: GdmaQueueType,
        config: &WqConfig,
    ) -> anyhow::Result<ManaCreateWqobjResp> {
        let resp: ManaCreateWqobjResp = self
            .gdma
            .request(
                ManaCommandCode::MANA_CREATE_WQ_OBJ.0,
                self.dev_id,
                ManaCreateWqobjReq {
                    vport,
                    wq_type,
                    reserved: 0,
                    wq_gdma_region: config.wq_gdma_region,
                    cq_gdma_region: config.cq_gdma_region,
                    wq_size: config.wq_size,
                    cq_size: config.cq_size,
                    cq_moderation_ctx_id: config.cq_moderation_ctx_id,
                    cq_parent_qid: config.eq_id,
                },
            )
            .await?;

        // The queues take ownership of their DMA regions.
        arena.take_dma_region(config.wq_gdma_region);
        arena.take_dma_region(config.cq_gdma_region);

        arena.push(Resource::BnicQueue {
            dev_id: self.dev_id,
            wq_type,
            wq_obj: resp.wq_obj,
        });
        Ok(resp)
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn destroy_wq_obj(
        &mut self,
        wq_type: GdmaQueueType,
        wq_obj_handle: u64,
    ) -> anyhow::Result<()> {
        self.gdma
            .request(
                ManaCommandCode::MANA_DESTROY_WQ_OBJ.0,
                self.dev_id,
                ManaDestroyWqobjReq {
                    wq_type,
                    reserved: 0,
                    wq_obj_handle,
                },
            )
            .await
    }

    #[tracing::instrument(skip(self, config), level = "debug", err)]
    pub async fn config_vport_rx(
        &mut self,
        vport: u64,
        config: &RxConfig<'_>,
    ) -> anyhow::Result<()> {
        #[repr(C)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        struct Req {
            req: ManaCfgRxSteerReq,
            table: [u64; 128],
        }

        let mut req = Req {
            req: ManaCfgRxSteerReq {
                vport,
                num_indir_entries: config.indirection_table.map_or(0, |t| t.len() as u16),
                indir_tab_offset: (size_of::<GdmaReqHdr>() + size_of::<ManaCfgRxSteerReq>()) as u16,
                rx_enable: config.rx_enable.into(),
                rss_enable: config.rss_enable.into(),
                update_default_rxobj: config.default_rxobj.is_some().into(),
                update_hashkey: config.hash_key.is_some().into(),
                update_indir_tab: config.indirection_table.is_some().into(),
                reserved: 0,
                default_rxobj: config.default_rxobj.unwrap_or(0),
                hashkey: *config.hash_key.unwrap_or(&[0; 40]),
            },
            table: FromZeros::new_zeroed(),
        };
        if let Some(table) = config.indirection_table {
            req.table[..table.len()].copy_from_slice(table);
        }

        self.gdma
            .request(ManaCommandCode::MANA_CONFIG_VPORT_RX.0, self.dev_id, req)
            .await
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn query_vport_config(
        &mut self,
        vport_index: u32,
    ) -> anyhow::Result<ManaQueryVportCfgResp> {
        let resp: ManaQueryVportCfgResp = self
            .gdma
            .request(
                ManaCommandCode::MANA_QUERY_VPORT_CONFIG.0,
                self.dev_id,
                ManaQueryVportCfgReq { vport_index },
            )
            .await?;
        Ok(resp)
    }

    pub async fn query_stats(
        &mut self,
        requested_statistics: u64,
    ) -> anyhow::Result<ManaQueryStatisticsResponse> {
        let resp: ManaQueryStatisticsResponse = self
            .gdma
            .request(
                ManaCommandCode::MANA_QUERY_STATS.0,
                self.dev_id,
                ManaQueryStatisticsRequest {
                    requested_statistics,
                },
            )
            .await?;
        Ok(resp)
    }

    pub async fn query_filter_state(
        &mut self,
        vport: u64,
    ) -> anyhow::Result<ManaQueryFilterStateResponse> {
        let resp: ManaQueryFilterStateResponse = self
            .gdma
            .request(
                ManaCommandCode::MANA_VTL2_QUERY_FILTER_STATE.0,
                self.dev_id,
                ManaQueryFilterStateReq { vport },
            )
            .await?;
        Ok(resp)
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn move_vport_filter(
        &mut self,
        vport: u64,
        direction_to_vtl0: u8,
    ) -> anyhow::Result<u32> {
        let ((), activity_id) = self
            .gdma
            .request_version(
                ManaCommandCode::MANA_VTL2_MOVE_FILTER.0,
                MANA_VTL2_MOVE_FILTER_REQUEST_V2,
                ManaCommandCode::MANA_VTL2_MOVE_FILTER.0,
                MANA_VTL2_MOVE_FILTER_RESPONSE_V1,
                self.dev_id,
                ManaMoveFilterVTL2PrivilegedReq {
                    vport,
                    direction_to_vtl0,
                    reserved: [0, 0, 0],
                    reserved2: 0,
                },
            )
            .await?;
        Ok(activity_id)
    }

    #[tracing::instrument(skip(self), level = "debug", err)]
    pub async fn set_vport_serial_no(&mut self, vport: u64, serial_no: u32) -> anyhow::Result<()> {
        let ((), _) = self
            .gdma
            .request_version(
                ManaCommandCode::MANA_VTL2_ASSIGN_SERIAL_NUMBER.0,
                MANA_VTL2_ASSIGN_SERIAL_NUMBER_REQUEST_V1,
                ManaCommandCode::MANA_VTL2_ASSIGN_SERIAL_NUMBER.0,
                MANA_VTL2_ASSIGN_SERIAL_NUMBER_RESPONSE_V1,
                self.dev_id,
                ManaSetVportSerialNo {
                    vport,
                    serial_no,
                    reserved: 0,
                },
            )
            .await?;
        Ok(())
    }
}

/// Receive configuration for a vport.
pub struct RxConfig<'a> {
    /// Enable receiving packets.
    pub rx_enable: Option<bool>,
    /// Enable RSS.
    pub rss_enable: Option<bool>,
    /// The RSS hash key to set.
    pub hash_key: Option<&'a [u8; 40]>,
    /// The default rx obj for incoming packets that cannot be hashed (I think).
    pub default_rxobj: Option<u64>,
    /// The RSS indirection table.
    pub indirection_table: Option<&'a [u64]>,
}

pub struct WqConfig {
    pub wq_gdma_region: u64,
    pub cq_gdma_region: u64,
    pub wq_size: u32,
    pub cq_size: u32,
    pub cq_moderation_ctx_id: u32,
    pub eq_id: u32,
}
