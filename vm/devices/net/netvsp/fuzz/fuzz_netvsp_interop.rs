// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interop fuzzer that combines control, OID, TX, RX, link status, and
//! device lifecycle actions. The fuzzer performs NVSP/RNDIS negotiation first,
//! then runs interleaved actions from all domains.
//!
//! This can find bugs that only manifest when different subsystems interact —
//! for example, sending data packets while OID sets are in flight, injecting
//! link status changes during TX processing, or closing the VMBus channel
//! mid-transfer.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use crate::fuzz_helpers::RingFullError;
use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::FuzzGuestOsId;
use fuzz_helpers::KNOWN_CONFIG_PARAM_NAMES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::SWITCH_DATA_PATH_TRANSACTION_ID;
use fuzz_helpers::StructuredPpiEntry;
use fuzz_helpers::StructuredRndisMessage;
use fuzz_helpers::StructuredRndisPacketMessage;
use fuzz_helpers::VF_ASSOCIATION_TRANSACTION_ID;
use fuzz_helpers::build_checksum_ppi_entry;
use fuzz_helpers::build_concatenated_rndis_messages;
use fuzz_helpers::build_lso_ppi_entry;
use fuzz_helpers::build_rndis_config_parameter;
use fuzz_helpers::build_rndis_message;
use fuzz_helpers::build_rndis_oid_query;
use fuzz_helpers::build_rndis_oid_set;
use fuzz_helpers::build_rss_oid_set;
use fuzz_helpers::build_structured_rndis_packet;
use fuzz_helpers::endpoint::FuzzRxMetadata;
use fuzz_helpers::negotiate_to_ready;
use fuzz_helpers::nic_setup::FuzzNicConfig;
use fuzz_helpers::nic_setup::NicSetupHandle;
use fuzz_helpers::nic_setup::create_nic_with_channel;
use fuzz_helpers::page_boundary_frame_size;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::rndis_set_packet_filter;
use fuzz_helpers::send_completion_packet;
use fuzz_helpers::send_inband_nvsp;
use fuzz_helpers::send_rndis_control;
use fuzz_helpers::send_rndis_gpadirect;
use fuzz_helpers::send_rndis_via_direct_path;
use fuzz_helpers::send_rndis_via_send_buffer;
use fuzz_helpers::send_tx_rndis_completion;
use fuzz_helpers::serialize_ppi_chain;
use fuzz_helpers::serialize_structured_rndis_packet_message;
use fuzz_helpers::try_read_one_completion;
use fuzz_helpers::write_packet;
use guestmem::GuestMemory;
use inspect::InspectionBuilder;
use net_backend::EndpointAction;
use netvsp::protocol;
use netvsp::rndisprot;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::PAGE_SIZE;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

/// Use the most demanding layout: send buffer for the send-buffer path,
/// data pages for GpaDirect RNDIS messages.
const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 4,
    data_pages: DATA_PAGES,
};

// ---- Combined interop actions ----

/// Actions spanning all three fuzzing domains: control, OID, and data path.
#[derive(Arbitrary, Debug)]
enum InteropAction {
    // ==== NVSP Control actions ====
    /// Send an arbitrary packet payload with a fuzzed packet type.
    ControlSendRawPacket {
        #[arbitrary(with = fuzz_helpers::arbitrary_outgoing_packet_type)]
        packet_type: OutgoingPacketType<'static>,
        payload: Vec<u8>,
    },
    /// Send a raw NVSP message with arbitrary type and payload.
    ControlSendRawInBand {
        message_type: u32,
        payload: Vec<u8>,
        with_completion: bool,
    },
    /// Send a well-formed Init message with arbitrary version.
    ControlSendInit { init: protocol::MessageInit },
    /// Send an NDIS version message with arbitrary version numbers.
    ControlSendNdisVersion {
        version: protocol::Message1SendNdisVersion,
    },
    /// Send NDIS config with arbitrary MTU and capabilities.
    ControlSendNdisConfig {
        config: protocol::Message2SendNdisConfig,
    },
    /// Send a receive buffer message.
    ControlSendReceiveBuffer {
        #[arbitrary(with = fuzz_helpers::arbitrary_send_receive_buffer_message)]
        msg: protocol::Message1SendReceiveBuffer,
    },
    /// Send a send buffer message.
    ControlSendSendBuffer {
        #[arbitrary(with = fuzz_helpers::arbitrary_send_send_buffer_message)]
        msg: protocol::Message1SendSendBuffer,
    },
    /// Send a revoke receive buffer message.
    ControlRevokeReceiveBuffer {
        msg: protocol::Message1RevokeReceiveBuffer,
    },
    /// Send a revoke send buffer message.
    ControlRevokeSendBuffer {
        msg: protocol::Message1RevokeSendBuffer,
    },
    /// Send a switch data path message.
    ControlSwitchDataPath {
        msg: protocol::Message4SwitchDataPath,
    },
    /// Send a subchannel request.
    ControlSubChannelRequest {
        request: protocol::Message5SubchannelRequest,
    },
    /// Send an OID query via the NVSP OidQueryEx message.
    ControlOidQueryEx { msg: protocol::Message5OidQueryEx },
    /// Send a raw RNDIS packet payload via GpaDirect on the data channel
    /// without any structured RNDIS header.
    SendRndisPacketDirect { payload: Vec<u8> },
    /// Send a raw RNDIS control payload via GpaDirect on the control channel
    /// without any structured RNDIS header.
    SendRndisControlDirect { payload: Vec<u8> },

    // ==== RNDIS OID actions ====
    /// Send a structured OID query with a specific OID value.
    OidQuery {
        oid: rndisprot::Oid,
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set with a specific OID value and payload.
    OidSet {
        oid: rndisprot::Oid,
        payload: Vec<u8>,
    },
    /// Send a structured OID set for OID_TCP_OFFLOAD_PARAMETERS.
    OidSetOffloadParameters {
        params: rndisprot::NdisOffloadParameters,
    },
    /// Send a structured OID set for OID_OFFLOAD_ENCAPSULATION.
    OidSetOffloadEncapsulation {
        encap: rndisprot::NdisOffloadEncapsulation,
    },
    /// Send a structured OID set for OID_GEN_RNDIS_CONFIG_PARAMETER.
    OidSetRndisConfigParameter {
        info: rndisprot::RndisConfigParameterInfo,
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set for OID_GEN_RECEIVE_SCALE_PARAMETERS.
    OidSetRssParameters {
        params: rndisprot::NdisReceiveScaleParameters,
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set for OID_GEN_CURRENT_PACKET_FILTER.
    OidSetPacketFilter { filter: u32 },
    /// Send a well-formed RSS OID set with valid offsets, hash key, and a
    /// properly clamped indirection table (via `build_rss_oid_set()`).
    OidSetWellFormedRss {
        hash_information: u32,
        indirection_entries: Vec<u32>,
        flags: u16,
    },
    /// Send a well-formed RNDIS config parameter OID set with proper
    /// UTF-16LE encoding (via `build_rndis_config_parameter()`).
    OidSetWellFormedConfigParameter {
        /// Index into `KNOWN_CONFIG_PARAM_NAMES` (clamped).
        name_index: u8,
        /// Parameter type (STRING, INTEGER, etc.).
        param_type: rndisprot::NdisParameterType,
        /// Raw value payload.
        value_bytes: Vec<u8>,
    },

    // ==== TX path actions ====
    /// Send a single RNDIS packet message via GpaDirect with fuzzed content.
    DataSendRndisPacket {
        rndis: StructuredRndisPacketMessage,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a well-formed RNDIS packet with fuzzed PPI and data via GpaDirect.
    DataSendStructuredRndisPacket {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a structured RNDIS packet with fuzzed PPI and a mostly valid
    /// Ethernet frame via GpaDirect.
    DataSendStructuredValidEthernetFrame {
        ppi_bytes: Vec<u8>,
        #[arbitrary(with = fuzz_helpers::arbitrary_valid_ethernet_frame)]
        frame_data: Vec<u8>,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a well-formed RNDIS packet with a structured PPI chain
    /// containing properly formatted checksum and/or LSO entries.
    DataSendWithStructuredPpi {
        ppi_entries: Vec<StructuredPpiEntry>,
        frame_data: Vec<u8>,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a packet with a specific LSO PPI entry.
    DataSendLsoPacket {
        mss: u32,
        tcp_header_offset: u16,
        is_ipv6: bool,
        frame_data: Vec<u8>,
    },
    /// Send a packet with a specific checksum PPI entry to exercise
    /// all flag combinations.
    DataSendChecksumEdgeCase {
        checksum_info: u32,
        frame_data: Vec<u8>,
    },
    /// Send multiple concatenated RNDIS packets in one GpaDirect message.
    DataSendMultipleRndisPackets {
        messages: Vec<StructuredRndisMessage>,
    },
    /// Send RNDIS data via the send buffer path.
    DataSendViaSendBuffer {
        rndis: StructuredRndisPacketMessage,
        nvsp_msg: protocol::Message1SendRndisPacket,
    },
    /// Send a TX completion with an arbitrary transaction ID.
    DataSendTxCompletion {
        transaction_id: u64,
        completion: protocol::Message1SendRndisPacketComplete,
    },
    /// Send an RNDIS control message (INITIALIZE, QUERY, SET, etc.) via
    /// GpaDirect on the control channel.
    DataSendRndisControl {
        header: rndisprot::MessageHeader,
        payload: Vec<u8>,
    },
    /// Inject a `TxError::TryRestart` on the next `tx_poll`, then send a
    /// packet to trigger `process_endpoint_tx`. Exercises the restart path.
    InjectTxRestart {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },
    /// Inject a `TxError::Fatal` on the next `tx_poll`, then send a packet
    /// to trigger `process_endpoint_tx`. Exercises the fatal error path.
    InjectTxFatal {
        ppi_bytes: Vec<u8>,
        frame_data: Vec<u8>,
    },

    // ==== Common actions ====
    /// Drain completions from the host.
    ReadCompletion,
    /// Send an RNDIS HALT message to test halt interleaved with other operations.
    SendRndisHalt,
    /// Send a VF association completion (TID 0x8000000000000000).
    SendVfAssociationCompletion,
    /// Send a switch data path completion (TID 0x8000000000000001).
    SendSwitchDataPathCompletion,
    /// Send a completion packet with an arbitrary transaction ID and payload.
    SendRawCompletion { tid: u64, payload: Vec<u8> },
    /// Send multiple concatenated RNDIS packets via GpaDirect (not send buffer).
    DataSendMultipleRndisPacketsDirect {
        messages: Vec<StructuredRndisMessage>,
    },

    // ==== RX path actions ====
    /// Send a burst of TX packets to stress RX buffer handling.
    RxBurstTxForLoopback { frames: Vec<Vec<u8>> },
    /// Send an empty TX frame to exercise RX edge handling.
    RxSendEmptyFrame,
    /// Send an oversized TX frame to exercise RX oversized handling.
    RxSendOversizedFrame { size: u16 },
    /// Send an RNDIS control message while RX is active.
    RxSendRndisControl { payload: Vec<u8>, message_type: u32 },
    /// Send a frame whose size targets page-boundary edge cases in the
    /// RX buffer `write_at()` / `write_header()` code paths.
    RxSendPageBoundaryFrame {
        /// Selects the target size (see RX fuzzer for variant mapping).
        variant: u8,
    },
    /// Send a frame of exactly 1514 bytes (standard Ethernet MTU + header)
    /// to exercise the most common real-world RX hot path.
    RxSendMtuSizedFrame,

    /// Inject one raw Ethernet payload from host/backend into guest RX path.
    InjectHostRxPacket {
        packet: Vec<u8>,
        metadata: FuzzRxMetadata,
    },
    /// Inject a burst of raw packets into guest RX path.
    InjectHostRxBurst {
        packets: Vec<(Vec<u8>, FuzzRxMetadata)>,
    },
    /// Inject a mostly valid Ethernet frame from host/backend.
    InjectHostValidEthernet {
        #[arbitrary(with = fuzz_helpers::arbitrary_valid_ethernet_frame)]
        frame_data: Vec<u8>,
        metadata: FuzzRxMetadata,
    },
    /// Inject a large host/backend RX frame to stress RX buffer handling.
    InjectHostOversized { size: u16, metadata: FuzzRxMetadata },
    /// Trigger endpoint action notifications while interleaving RX traffic.
    NotifyLinkStatus { up: bool },
    /// Inject a rapid sequence of link up/down toggles to stress the link
    /// state machine under concurrent traffic.
    LinkRapidToggle {
        /// Number of toggles (clamped to 1..=20).
        count: u8,
    },
    /// Inject a `RestartRequired` endpoint action.
    NotifyRestartRequired,

    // ==== RNDIS keepalive/reset/init actions ====
    /// Send an RNDIS INITIALIZE message with a fully fuzzed InitializeRequest.
    /// This exercises arbitrary version numbers and max_transfer_size values.
    SendRndisInitialize {
        request: rndisprot::InitializeRequest,
    },
    /// Send an RNDIS keepalive message to exercise keepalive handling.
    SendRndisKeepalive { request_id: u32 },
    /// Send an RNDIS RESET message to exercise the reset code path.
    SendRndisReset { reserved: u32 },

    // ==== Send buffer adversarial section indices ====
    /// Send a raw MESSAGE1_TYPE_SEND_RNDIS_PACKET with adversarial
    /// send_buffer_section_index and send_buffer_section_size values to
    /// exercise the section-index validation and try_subrange() arithmetic
    /// in the product code.
    DataSendRawSendBufferPacket {
        send_buffer_section_index: u32,
        send_buffer_section_size: u32,
        channel_type: u32,
    },

    // ==== Device lifecycle actions ====
    /// Inspect the device state via the inspect infrastructure.
    InspectDevice,
    /// Retarget the primary channel to a different VP.
    RetargetVp { target_vp: u32 },
    /// Close the primary VMBus channel. This is a terminal action — the fuzz
    /// loop must stop after this.
    ClosePrimaryChannel,
}

/// Result of executing one interop action, signaling whether the fuzz loop
/// should continue or must stop.
enum ActionResult {
    /// Continue with the next action.
    Continue,
    /// The primary channel was closed; the fuzz loop must stop.
    ChannelClosed,
}

// ---- Action execution ----

/// Execute one interop fuzz action.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
    rx_packet_sender: &mesh::Sender<(Vec<u8>, FuzzRxMetadata)>,
    endpoint_action_sender: &mesh::Sender<EndpointAction>,
    handle: &NicSetupHandle,
    tx_error_mode: &Option<Arc<AtomicU8>>,
) -> Result<ActionResult, anyhow::Error> {
    let action = input.arbitrary::<InteropAction>()?;
    fuzz_eprintln!("action: {action:?}");
    let tid = next_transaction_id;
    let rx_send = rx_packet_sender;
    let action_send = endpoint_action_sender;
    match action {
        // ==== NVSP Control ====
        InteropAction::ControlSendRawPacket {
            packet_type,
            payload,
        } => {
            write_packet(queue, tid, packet_type, &[&payload]).await?;
        }
        InteropAction::ControlSendRawInBand {
            message_type,
            payload,
            with_completion,
        } => {
            send_inband_nvsp(queue, tid, message_type, &payload, with_completion).await?;
        }
        InteropAction::ControlSendInit { init } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE_TYPE_INIT,
                init.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSendNdisVersion { version } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_SEND_NDIS_VERSION,
                version.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSendNdisConfig { config } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE2_TYPE_SEND_NDIS_CONFIG,
                config.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSendReceiveBuffer { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_SEND_RECEIVE_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSendSendBuffer { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_SEND_SEND_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlRevokeReceiveBuffer { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_REVOKE_RECEIVE_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlRevokeSendBuffer { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_REVOKE_SEND_BUFFER,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSwitchDataPath { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE4_TYPE_SWITCH_DATA_PATH,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlSubChannelRequest { request } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE5_TYPE_SUB_CHANNEL,
                request.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::ControlOidQueryEx { msg } => {
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE5_TYPE_OID_QUERY_EX,
                msg.as_bytes(),
                true,
            )
            .await?;
        }
        InteropAction::SendRndisPacketDirect { payload } => {
            send_rndis_via_direct_path(
                queue,
                mem,
                &payload,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::SendRndisControlDirect { payload } => {
            send_rndis_via_direct_path(
                queue,
                mem,
                &payload,
                protocol::CONTROL_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }

        // ==== RNDIS OID ====
        InteropAction::OidQuery { oid, extra_data } => {
            let rndis_bytes = build_rndis_oid_query(oid, &extra_data);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSet { oid, payload } => {
            let rndis_bytes = build_rndis_oid_set(oid, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetOffloadParameters { params } => {
            let rndis_bytes = build_rndis_oid_set(
                rndisprot::Oid::OID_TCP_OFFLOAD_PARAMETERS,
                params.as_bytes(),
            );
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetOffloadEncapsulation { encap } => {
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_OFFLOAD_ENCAPSULATION, encap.as_bytes());
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetRndisConfigParameter { info, extra_data } => {
            let mut payload = Vec::new();
            payload.extend_from_slice(info.as_bytes());
            payload.extend_from_slice(&extra_data);
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_GEN_RNDIS_CONFIG_PARAMETER, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetRssParameters { params, extra_data } => {
            let mut payload = Vec::new();
            payload.extend_from_slice(params.as_bytes());
            payload.extend_from_slice(&extra_data);
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetPacketFilter { filter } => {
            let rndis_bytes = build_rndis_oid_set(
                rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                filter.as_bytes(),
            );
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetWellFormedRss {
            hash_information,
            indirection_entries,
            flags,
        } => {
            let rndis_bytes = build_rss_oid_set(
                hash_information,
                &indirection_entries,
                1, // max_queues — single queue in this fuzzer
                flags,
            );
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::OidSetWellFormedConfigParameter {
            name_index,
            param_type,
            value_bytes,
        } => {
            let name =
                KNOWN_CONFIG_PARAM_NAMES[name_index as usize % KNOWN_CONFIG_PARAM_NAMES.len()];
            let rndis_bytes = build_rndis_config_parameter(name, param_type, &value_bytes);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }

        // ==== TX path ====
        InteropAction::DataSendRndisPacket {
            mut rndis,
            nvsp_msg,
        } => {
            let rndis_bytes = serialize_structured_rndis_packet_message(&mut rndis);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_bytes,
                nvsp_msg.channel_type,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::DataSendStructuredRndisPacket {
            ppi_bytes,
            frame_data,
            nvsp_msg,
        } => {
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        InteropAction::DataSendStructuredValidEthernetFrame {
            ppi_bytes,
            frame_data,
            nvsp_msg,
        } => {
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        InteropAction::DataSendWithStructuredPpi {
            ppi_entries,
            frame_data,
            nvsp_msg,
        } => {
            let ppi_bytes = serialize_ppi_chain(&ppi_entries);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(queue, mem, &rndis_buf, nvsp_msg.channel_type, &LAYOUT, tid)
                .await?;
        }
        InteropAction::DataSendLsoPacket {
            mss,
            tcp_header_offset,
            is_ipv6,
            frame_data,
        } => {
            let ppi_bytes = build_lso_ppi_entry(mss, tcp_header_offset, is_ipv6);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::DataSendChecksumEdgeCase {
            checksum_info,
            frame_data,
        } => {
            let ppi_bytes = build_checksum_ppi_entry(checksum_info);
            let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::DataSendMultipleRndisPackets { messages } => {
            let rndis_buf = build_concatenated_rndis_messages(&messages);

            if !rndis_buf.is_empty() {
                send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await?;
            }
        }
        InteropAction::DataSendViaSendBuffer {
            mut rndis,
            nvsp_msg,
        } => {
            let rndis_bytes = serialize_structured_rndis_packet_message(&mut rndis);
            send_rndis_via_send_buffer(queue, mem, &rndis_bytes, &nvsp_msg, &LAYOUT, tid).await?;
        }
        InteropAction::DataSendTxCompletion {
            transaction_id,
            completion,
        } => {
            send_tx_rndis_completion(queue, transaction_id, &completion).await?;
        }
        InteropAction::DataSendRndisControl { header, payload } => {
            let rndis_buf = build_rndis_message(header.message_type, &payload);
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_buf,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                tid,
            )
            .await?;
        }
        InteropAction::InjectTxRestart {
            ppi_bytes,
            frame_data,
        } => {
            if let Some(mode) = tx_error_mode {
                mode.store(1, Ordering::Relaxed);
                let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
                let _ = send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await;
                fuzz_helpers::yield_to_executor(20).await;
                fuzz_helpers::drain_queue_async(queue).await;
            }
        }
        InteropAction::InjectTxFatal {
            ppi_bytes,
            frame_data,
        } => {
            if let Some(mode) = tx_error_mode {
                mode.store(2, Ordering::Relaxed);
                let rndis_buf = build_structured_rndis_packet(&ppi_bytes, &frame_data);
                let _ = send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await;
                fuzz_helpers::yield_to_executor(10).await;
            }
        }

        // ==== Common ====
        InteropAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        InteropAction::SendRndisHalt => {
            let rndis_bytes = build_rndis_message(rndisprot::MESSAGE_TYPE_HALT_MSG, &[]);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::SendVfAssociationCompletion => {
            send_completion_packet(queue, VF_ASSOCIATION_TRANSACTION_ID, &[]).await?;
        }
        InteropAction::SendSwitchDataPathCompletion => {
            send_completion_packet(queue, SWITCH_DATA_PATH_TRANSACTION_ID, &[]).await?;
        }
        InteropAction::SendRawCompletion {
            tid: raw_tid,
            payload,
        } => {
            send_completion_packet(queue, raw_tid, &[&payload]).await?;
        }
        InteropAction::DataSendMultipleRndisPacketsDirect { messages } => {
            let rndis_buf = build_concatenated_rndis_messages(&messages);

            if !rndis_buf.is_empty() {
                send_rndis_gpadirect(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    LAYOUT.data_page_start(),
                    LAYOUT.data_pages,
                    tid,
                )
                .await?;
            }
        }

        // ==== RX path ====
        InteropAction::RxBurstTxForLoopback { frames } => {
            for frame_data in &frames {
                let rndis_buf = build_structured_rndis_packet(&[], frame_data);
                send_rndis_via_direct_path(
                    queue,
                    mem,
                    &rndis_buf,
                    protocol::DATA_CHANNEL_TYPE,
                    &LAYOUT,
                    tid,
                )
                .await?;
            }
        }
        InteropAction::RxSendEmptyFrame => {
            let rndis_buf = build_structured_rndis_packet(&[], &[]);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::RxSendOversizedFrame { size } => {
            let actual_size = (size as usize).clamp(1500, DATA_PAGES * PAGE_SIZE);
            let frame_data = vec![0xAB; actual_size];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::RxSendRndisControl {
            payload,
            message_type,
        } => {
            let rndis_buf = build_rndis_message(message_type, &payload);
            send_rndis_gpadirect(
                queue,
                mem,
                &rndis_buf,
                protocol::CONTROL_CHANNEL_TYPE,
                LAYOUT.data_page_start(),
                LAYOUT.data_pages,
                tid,
            )
            .await?;
        }
        InteropAction::InjectHostRxPacket { packet, metadata } => {
            rx_send.send((packet, metadata));
        }
        InteropAction::InjectHostRxBurst { packets } => {
            for (packet, metadata) in packets {
                rx_send.send((packet, metadata));
            }
        }
        InteropAction::InjectHostValidEthernet {
            frame_data,
            metadata,
        } => {
            rx_send.send((frame_data, metadata));
        }
        InteropAction::InjectHostOversized { size, metadata } => {
            let payload_len = (size as usize).clamp(1536, PAGE_SIZE * DATA_PAGES);
            rx_send.send((vec![0xA5; payload_len], metadata));
        }
        InteropAction::NotifyLinkStatus { up } => {
            action_send.send(EndpointAction::LinkStatusNotify(up));
        }
        InteropAction::LinkRapidToggle { count } => {
            let n = (count % 20) + 1;
            for i in 0..n {
                action_send.send(EndpointAction::LinkStatusNotify(i % 2 == 0));
            }
        }
        InteropAction::NotifyRestartRequired => {
            action_send.send(EndpointAction::RestartRequired);
        }
        InteropAction::RxSendPageBoundaryFrame { variant } => {
            let frame_data = vec![0xBB; page_boundary_frame_size(variant)];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }
        InteropAction::RxSendMtuSizedFrame => {
            let frame_data = vec![0xDD; 1514];
            let rndis_buf = build_structured_rndis_packet(&[], &frame_data);
            send_rndis_via_direct_path(
                queue,
                mem,
                &rndis_buf,
                protocol::DATA_CHANNEL_TYPE,
                &LAYOUT,
                tid,
            )
            .await?;
        }

        // ==== RNDIS keepalive/reset/init ====
        InteropAction::SendRndisInitialize { request } => {
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_INITIALIZE_MSG, request.as_bytes());
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::SendRndisKeepalive { request_id } => {
            let keepalive = rndisprot::KeepaliveRequest { request_id };
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_KEEPALIVE_MSG, keepalive.as_bytes());
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }
        InteropAction::SendRndisReset { reserved } => {
            let rndis_bytes =
                build_rndis_message(rndisprot::MESSAGE_TYPE_RESET_MSG, &reserved.to_le_bytes());
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, tid).await?;
        }

        // ==== Send buffer adversarial section indices ====
        InteropAction::DataSendRawSendBufferPacket {
            send_buffer_section_index,
            send_buffer_section_size,
            channel_type,
        } => {
            let msg = protocol::Message1SendRndisPacket {
                channel_type,
                send_buffer_section_index,
                send_buffer_section_size,
            };
            send_inband_nvsp(
                queue,
                tid,
                protocol::MESSAGE1_TYPE_SEND_RNDIS_PACKET,
                msg.as_bytes(),
                true,
            )
            .await?;
        }

        // ==== Device lifecycle ====
        InteropAction::InspectDevice => {
            let mut inspection = InspectionBuilder::new("")
                .depth(Some(2))
                .inspect(&handle.channel);
            inspection.resolve().await;
            let _ = inspection.results();
        }
        InteropAction::RetargetVp { target_vp } => {
            handle.send_retarget_vp(target_vp);
        }
        InteropAction::ClosePrimaryChannel => {
            handle.send_close();
            return Ok(ActionResult::ChannelClosed);
        }
    }
    Ok(ActionResult::Continue)
}

fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();

    // Parse a loopback metadata template from the front of the input so that
    // TX→RX loopback packets exercise varied checksum-flag branches in
    // `write_header()` (48 combinations).
    let mut pre = Unstructured::new(input);
    let loopback_meta = pre.arbitrary::<FuzzRxMetadata>().unwrap_or_default();

    // Fuzz-select the guest OS identity so this target also covers all
    // `can_use_ring_opt` branches.
    let fuzz_os = pre.arbitrary::<FuzzGuestOsId>().ok();
    let remaining_start = input.len() - pre.len();
    let fuzz_input = &input[remaining_start..];

    let (mut endpoint, handles) =
        fuzz_helpers::endpoint::FuzzEndpoint::new(fuzz_helpers::endpoint::FuzzEndpointConfig {
            enable_rx_injection: true,
            enable_action_injection: true,
            enable_tx_error_injection: true,
            enable_async_tx: true,
            ..fuzz_helpers::endpoint::FuzzEndpointConfig::default()
        });
    endpoint.loopback_metadata = loopback_meta;
    let rx_send = handles
        .rx_send
        .expect("rx injection must be enabled for interop fuzzing");
    let action_send = handles
        .action_send
        .expect("action injection must be enabled for interop fuzzing");
    let tx_error_mode = handles.tx_error_mode;
    let mut config = FuzzNicConfig {
        endpoint: Box::new(endpoint),
        virtual_function: None,
        ..FuzzNicConfig::default()
    };
    if let Some(os) = fuzz_os {
        config.get_guest_os_id = os.to_hv_guest_os_id();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        let (handle, setup) = match create_nic_with_channel(&driver, &LAYOUT, config).await {
            Ok(pair) => pair,
            Err(_) => return,
        };

        let fuzz_result = mesh::CancelContext::new()
            .with_timeout(std::time::Duration::from_millis(500))
            .until_cancelled(run_interop_loop(
                fuzz_input,
                handle,
                setup,
                &rx_send,
                &action_send,
                &tx_error_mode,
            ))
            .await;

        match fuzz_result {
            Ok(Ok(())) => {
                fuzz_eprintln!("fuzz: test case exhausted arbitrary data");
            }
            Ok(Err(e)) => {
                if e.downcast_ref::<arbitrary::Error>().is_some() {
                    fuzz_eprintln!("fuzz: arbitrary data exhausted: {e:#}");
                } else if e.downcast_ref::<RingFullError>().is_some() {
                    fuzz_eprintln!("fuzz: ring full (backpressure), stopping");
                } else {
                    panic!("fuzz: action error: {e:#}");
                }
            }
            Err(_) => {
                panic!("fuzz: timed out after 500ms");
            }
        }
    });
});

async fn run_interop_loop(
    fuzz_input: &[u8],
    handle: NicSetupHandle,
    setup: fuzz_helpers::nic_setup::FuzzNicSetup,
    rx_send: &mesh::Sender<(Vec<u8>, FuzzRxMetadata)>,
    action_send: &mesh::Sender<EndpointAction>,
    tx_error_mode: &Option<Arc<AtomicU8>>,
) -> anyhow::Result<()> {
    let mut queue = setup.queue;
    let mem = setup.mem;
    let mut next_transaction_id = 1u64;
    let mut actions_since_drain = 0u32;
    let mut fuzzer_input = Unstructured::new(fuzz_input);

    handle.channel.start();

    negotiate_to_ready(
        &mut queue,
        &mut next_transaction_id,
        setup.recv_buf_gpadl_id,
        setup.send_buf_gpadl_id,
    )
    .await?;

    // Initialize RNDIS 90% of the time to reach
    // Operational state. The remaining 10% tests interactions
    // before RNDIS initialization.
    if fuzzer_input.ratio(9, 10)? {
        rndis_initialize(
            &mut queue,
            &mem,
            LAYOUT.data_page_start(),
            LAYOUT.data_pages,
            &mut next_transaction_id,
        )
        .await?;

        // Set the packet filter so RX packets are actually delivered
        // instead of being silently dropped in process_endpoint_rx.
        rndis_set_packet_filter(&mut queue, &mem, &LAYOUT, &mut next_transaction_id).await?;
    }

    // Run interleaved actions until input is exhausted.
    while !fuzzer_input.is_empty() {
        let result = execute_next_action(
            &mut fuzzer_input,
            &mut queue,
            &mem,
            &mut next_transaction_id,
            rx_send,
            action_send,
            &handle,
            tx_error_mode,
        )
        .await?;

        match result {
            ActionResult::ChannelClosed => break,
            ActionResult::Continue => {}
        }

        // Allow actions to build up in the queue, but drain periodically
        // so all subsequent actions don't hit a full ring and fail.
        // The optimal drain frequency may need tuning.
        actions_since_drain += 1;
        if actions_since_drain >= 4 {
            fuzz_helpers::drain_queue_async(&mut queue).await;
            actions_since_drain = 0;
        }
    }

    handle.cleanup().await;
    Ok(())
}
