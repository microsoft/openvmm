// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzer for RNDIS OID query and OID set operations in NetVSP.
//!
//! ## RNDIS protocol messages tested
//!
//! - `MESSAGE_TYPE_QUERY_MSG` — structured OID queries with arbitrary OID values
//!   and raw queries with arbitrary payloads
//! - `MESSAGE_TYPE_SET_MSG` — structured OID sets with arbitrary OID values
//!   and raw sets with arbitrary payloads
//!
//! ## OIDs targeted
//!
//! - `OID_TCP_OFFLOAD_PARAMETERS` — TCP offload configuration
//! - `OID_OFFLOAD_ENCAPSULATION` — offload encapsulation settings
//! - `OID_GEN_RNDIS_CONFIG_PARAMETER` — RNDIS configuration parameters
//! - `OID_GEN_RECEIVE_SCALE_PARAMETERS` — RSS hash key and indirection table
//! - `OID_GEN_CURRENT_PACKET_FILTER` — packet filter bitmask
//! - Arbitrary `Oid` values via structured query/set actions

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

mod fuzz_helpers;

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use fuzz_helpers::DATA_PAGES;
use fuzz_helpers::PageLayout;
use fuzz_helpers::build_rndis_config_parameter;
use fuzz_helpers::build_rndis_message;
use fuzz_helpers::build_rndis_oid_query;
use fuzz_helpers::build_rndis_oid_set;
use fuzz_helpers::drain_queue_async;
use fuzz_helpers::negotiate_to_ready;
use fuzz_helpers::rndis_initialize;
use fuzz_helpers::run_fuzz_loop;
use fuzz_helpers::send_rndis_control;
use fuzz_helpers::try_read_one_completion;
use guestmem::GuestMemory;
use netvsp::rndisprot;
use vmbus_async::queue::Queue;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;
use zerocopy::IntoBytes;

const LAYOUT: PageLayout = PageLayout {
    send_buf_pages: 1,
    data_pages: DATA_PAGES,
};

/// Actions the fuzzer can take to exercise OID query/set handling.
#[derive(Arbitrary, Debug)]
enum OidAction {
    /// Send a structured OID query with a specific OID value.
    OidQuery {
        /// The OID to query.
        oid: rndisprot::Oid,
        /// Extra data to append after the QueryRequest struct.
        extra_data: Vec<u8>,
    },
    /// Send an OID query with a fully fuzzed QueryRequest struct.
    /// This exercises arbitrary request_id, device_vc_handle, and
    /// potentially malformed buffer offset/length values.
    OidQueryFull {
        request: rndisprot::QueryRequest,
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set with a specific OID value and payload.
    OidSet {
        /// The OID to set.
        oid: rndisprot::Oid,
        /// The information buffer payload for the OID set.
        payload: Vec<u8>,
    },
    /// Send an OID set with a fully fuzzed SetRequest struct.
    /// This exercises arbitrary request_id, device_vc_handle, and
    /// potentially malformed buffer offset/length values.
    OidSetFull {
        request: rndisprot::SetRequest,
        payload: Vec<u8>,
    },
    /// Send a structured OID set for OID_TCP_OFFLOAD_PARAMETERS.
    SetOffloadParameters {
        /// Fuzzed offload parameters (includes NdisObjectHeader).
        params: rndisprot::NdisOffloadParameters,
    },
    /// Send a structured OID set for OID_OFFLOAD_ENCAPSULATION.
    SetOffloadEncapsulation {
        /// Fuzzed encapsulation settings (includes NdisObjectHeader).
        encap: rndisprot::NdisOffloadEncapsulation,
    },
    /// Send a structured OID set for OID_GEN_RNDIS_CONFIG_PARAMETER.
    SetRndisConfigParameter {
        /// Fuzzed config parameter info header.
        info: rndisprot::RndisConfigParameterInfo,
        /// Extra data appended after the info struct (name + value data).
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set for OID_GEN_RECEIVE_SCALE_PARAMETERS.
    SetRssParameters {
        /// Fuzzed RSS parameters (includes NdisObjectHeader).
        params: rndisprot::NdisReceiveScaleParameters,
        /// Extra trailing data (hash key, indirection table).
        extra_data: Vec<u8>,
    },
    /// Send a structured OID set for OID_GEN_CURRENT_PACKET_FILTER.
    SetPacketFilter {
        /// The packet filter value.
        filter: u32,
    },
    /// Read one completion/notification from the host.
    ReadCompletion,
    /// Send an RNDIS HALT message to test halt-during-OID-processing.
    SendRndisHalt,
    /// Send a well-formed `OID_GEN_RNDIS_CONFIG_PARAMETER` SET with a known
    /// parameter name (e.g. `*IPChecksumOffloadIPv4`) and fuzzed type/value.
    /// This exercises the named match arms in `oid_set_rndis_config_parameter`
    /// that raw fuzzing cannot easily reach because they require properly
    /// UTF-16LE–encoded parameter names.
    SendKnownConfigParameter {
        /// Index into `KNOWN_CONFIG_PARAM_NAMES` (wrapped mod length).
        name_idx: u8,
        /// Parameter type: 0 = INTEGER, 2 = STRING, other = arbitrary.
        param_type: u32,
        /// Raw value bytes.  For STRING, these are fuzzed UTF-16LE; for
        /// INTEGER, 4 bytes of a u32.
        value_bytes: Vec<u8>,
    },
}

/// Execute one OID fuzz action.
async fn execute_next_action(
    input: &mut Unstructured<'_>,
    queue: &mut Queue<GpadlRingMem>,
    mem: &GuestMemory,
    next_transaction_id: &mut u64,
) -> Result<(), anyhow::Error> {
    let action = input.arbitrary::<OidAction>()?;
    fuzz_eprintln!("action: {action:?}");
    match action {
        OidAction::OidQuery { oid, extra_data } => {
            let rndis_bytes = build_rndis_oid_query(oid, &extra_data);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::OidQueryFull {
            request,
            extra_data,
        } => {
            let mut body = Vec::new();
            body.extend_from_slice(request.as_bytes());
            body.extend_from_slice(&extra_data);
            let rndis_bytes = build_rndis_message(rndisprot::MESSAGE_TYPE_QUERY_MSG, &body);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::OidSet { oid, payload } => {
            let rndis_bytes = build_rndis_oid_set(oid, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::OidSetFull { request, payload } => {
            let mut body = Vec::new();
            body.extend_from_slice(request.as_bytes());
            body.extend_from_slice(&payload);
            let rndis_bytes = build_rndis_message(rndisprot::MESSAGE_TYPE_SET_MSG, &body);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SetOffloadParameters { params } => {
            let rndis_bytes = build_rndis_oid_set(
                rndisprot::Oid::OID_TCP_OFFLOAD_PARAMETERS,
                params.as_bytes(),
            );
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SetOffloadEncapsulation { encap } => {
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_OFFLOAD_ENCAPSULATION, encap.as_bytes());
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SetRndisConfigParameter { info, extra_data } => {
            let mut payload = Vec::new();
            payload.extend_from_slice(info.as_bytes());
            payload.extend_from_slice(&extra_data);
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_GEN_RNDIS_CONFIG_PARAMETER, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SetRssParameters { params, extra_data } => {
            let mut payload = Vec::new();
            payload.extend_from_slice(params.as_bytes());
            payload.extend_from_slice(&extra_data);
            let rndis_bytes =
                build_rndis_oid_set(rndisprot::Oid::OID_GEN_RECEIVE_SCALE_PARAMETERS, &payload);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SetPacketFilter { filter } => {
            let rndis_bytes = build_rndis_oid_set(
                rndisprot::Oid::OID_GEN_CURRENT_PACKET_FILTER,
                filter.as_bytes(),
            );
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::ReadCompletion => {
            let _ = try_read_one_completion(queue);
        }
        OidAction::SendRndisHalt => {
            let rndis_bytes = build_rndis_message(rndisprot::MESSAGE_TYPE_HALT_MSG, &[]);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
        OidAction::SendKnownConfigParameter {
            name_idx,
            param_type,
            value_bytes,
        } => {
            let names = fuzz_helpers::KNOWN_CONFIG_PARAM_NAMES;
            let name = names[name_idx as usize % names.len()];
            let param_type = rndisprot::NdisParameterType(param_type);
            let rndis_bytes = build_rndis_config_parameter(name, param_type, &value_bytes);
            send_rndis_control(queue, mem, &rndis_bytes, &LAYOUT, next_transaction_id).await?;
        }
    }
    Ok(())
}

fuzz_target!(|input: &[u8]| {
    run_fuzz_loop(input, &LAYOUT, |fuzzer_input, setup| {
        Box::pin(async move {
            let mut queue = setup.queue;
            let mem = setup.mem;
            let mut next_transaction_id = 1u64;

            // Negotiate NVSP protocol to the ready state.
            negotiate_to_ready(
                &mut queue,
                &mut next_transaction_id,
                setup.recv_buf_gpadl_id,
                setup.send_buf_gpadl_id,
            )
            .await?;

            // 90% of the time, initialize RNDIS to reach Operational state.
            // The remaining 10% tests OID handling before the initialize handshake.
            if fuzzer_input.ratio(9, 10)? {
                rndis_initialize(
                    &mut queue,
                    &mem,
                    LAYOUT.data_page_start(),
                    LAYOUT.data_pages,
                    &mut next_transaction_id,
                )
                .await?;
            }

            // Run OID fuzz actions until input is exhausted.
            while !fuzzer_input.is_empty() {
                execute_next_action(fuzzer_input, &mut queue, &mem, &mut next_transaction_id)
                    .await?;
                drain_queue_async(&mut queue).await;
            }
            Ok(())
        })
    });
});
