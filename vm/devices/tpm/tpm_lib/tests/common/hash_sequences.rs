// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sequence lifecycle and loaded-object slot reuse across TPM command orders.

use super::create_tpm_engine_helper;
use super::rsa_srk_template;
use crate::test_tpm_backend::TestTpmPlatform;
use crate::tpm_lib::TpmEngineHelper;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::CommandCodeEnum;
use tpm_protocol::tpm20proto::ReservedHandle;
use tpm_protocol::tpm20proto::ResponseCode;
use tpm_protocol::tpm20proto::SessionTagEnum;
use tpm_protocol::tpm20proto::TPM20_HT_PERMANENT;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::TPM20_RS_PW;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use tpm_protocol::tpm20proto::protocol::common::CmdAuth;
use tpm_protocol::tpm20proto::protocol::common::CmdHeader;
use tpm_protocol::tpm20proto::protocol::common::ReplyHeader;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub(super) const RH_NULL: ReservedHandle = ReservedHandle::new(TPM20_HT_PERMANENT, 7);
pub(super) const ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

// Known digests of "abc", independent of the TPM's crypto backend.
pub(super) const HASHES: [(u16, &[u8]); 3] = [
    (
        AlgIdEnum::SHA as u16,
        &[
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ],
    ),
    (
        AlgIdEnum::SHA256 as u16,
        &[
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ],
    ),
    (
        AlgIdEnum::SHA384 as u16,
        &[
            0xcb, 0x00, 0x75, 0x3f, 0x45, 0xa3, 0x5e, 0x8b, 0xb5, 0xa0, 0x3d, 0x69, 0x9a, 0xc6,
            0x50, 0x07, 0x27, 0x2c, 0x32, 0xab, 0x0e, 0xde, 0xd1, 0x63, 0x1a, 0x8b, 0x60, 0x5a,
            0x43, 0xff, 0x5b, 0xed, 0x80, 0x86, 0x07, 0x2b, 0xa1, 0xe7, 0xcc, 0x23, 0x58, 0xba,
            0xec, 0xa1, 0x34, 0xc8, 0x25, 0xa7,
        ],
    ),
];

pub(super) fn execute_command(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    tag: SessionTagEnum,
    code: CommandCodeEnum,
    payload: &[u8],
    expected_status: ResponseCode,
) -> Vec<u8> {
    let (status, reply) = execute_command_status(tpm, tag, code, payload);
    assert_eq!(
        status, expected_status as u32,
        "{code:?} returned {status:#x}"
    );
    reply
}

pub(super) fn execute_command_status(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    tag: SessionTagEnum,
    code: CommandCodeEnum,
    payload: &[u8],
) -> (u32, Vec<u8>) {
    let mut header = CmdHeader::new::<CmdHeader>(tag.into(), code.into());
    header.size = u32::try_from(size_of::<CmdHeader>() + payload.len())
        .unwrap()
        .into();
    let mut command = [header.as_bytes(), payload].concat();
    let mut response = [0u8; 4096];
    let response_size = tpm
        .tpm_engine
        .execute_command(&mut command, &mut response)
        .unwrap();
    let (reply, payload) = ReplyHeader::read_from_prefix(&response[..response_size]).unwrap();
    assert_eq!(reply.size.get() as usize, response_size, "{code:?}");
    let expected_tag = if reply.response_code.get() == ResponseCode::Success as u32 {
        header.session_tag.0.get()
    } else {
        SessionTagEnum::NoSessions as u16
    };
    assert_eq!(reply.session_tag.get(), expected_tag, "{code:?}");
    (reply.response_code.get(), payload.to_vec())
}

pub(super) fn start_sequence(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    algorithm: u16,
) -> ReservedHandle {
    let payload = [0u16.to_be_bytes(), algorithm.to_be_bytes()].concat();
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::HashSequenceStart,
        &payload,
        ResponseCode::Success,
    );
    ReservedHandle::read_from_bytes(&reply).unwrap()
}

fn sequence_command(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    code: CommandCodeEnum,
    handle: ReservedHandle,
    parameters: &[u8],
) -> Vec<u8> {
    let auth = CmdAuth::new(TPM20_RS_PW, 0, 0, 0);
    let payload = [
        handle.as_bytes(),
        &u32::try_from(size_of::<CmdAuth>()).unwrap().to_be_bytes(),
        auth.as_bytes(),
        parameters,
    ]
    .concat();
    let reply = execute_command(
        tpm,
        SessionTagEnum::Sessions,
        code,
        &payload,
        ResponseCode::Success,
    );
    let parameter_size = u32::from_be_bytes(reply[..4].try_into().unwrap()) as usize;
    let (parameters, auth) = reply[4..].split_at(parameter_size);
    // Password-session replies have empty nonce/HMAC and continueSession set.
    assert_eq!(auth, [0, 0, 1, 0, 0], "{code:?}");
    parameters.to_vec()
}

pub(super) fn update_sequence(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    handle: ReservedHandle,
    data: &[u8],
) {
    let reply = sequence_command(
        tpm,
        CommandCodeEnum::SequenceUpdate,
        handle,
        &Tpm2bBuffer::new(data).unwrap().serialize(),
    );
    assert!(reply.is_empty());
}

pub(super) fn complete_sequence(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    handle: ReservedHandle,
    data: &[u8],
    expected_digest: &[u8],
) {
    let mut parameters = Tpm2bBuffer::new(data).unwrap().serialize();
    parameters.extend_from_slice(RH_NULL.as_bytes());
    let reply = sequence_command(tpm, CommandCodeEnum::SequenceComplete, handle, &parameters);
    let digest = Tpm2bBuffer::deserialize(&reply).unwrap();
    assert_eq!(
        &digest.buffer[..digest.size.get() as usize],
        expected_digest
    );
    let expected_ticket = [
        &(SessionTagEnum::Hashcheck as u16).to_be_bytes(),
        RH_NULL.as_bytes(),
        &0u16.to_be_bytes(),
    ]
    .concat();
    assert_eq!(&reply[digest.payload_size()..], expected_ticket);
}

#[test]
fn test_hash_sequence_start_sha1_multiple_objects() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    // Keep all three live so later object slots are exercised, not just slot zero.
    let handles: [_; 3] = std::array::from_fn(|slot| {
        let handle = start_sequence(&mut tpm, HASHES[0].0);
        assert_eq!(handle, ReservedHandle::new(0x80, slot as u32));
        handle
    });
    let reply = execute_command(
        &mut tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::HashSequenceStart,
        &[0, 0, 0, 4],
        ResponseCode::ObjectMemory,
    );
    assert!(reply.is_empty());

    update_sequence(&mut tpm, handles[0], b"a");
    update_sequence(&mut tpm, handles[2], b"The quick brown fox ");
    update_sequence(&mut tpm, handles[1], b"");
    complete_sequence(
        &mut tpm,
        handles[2],
        b"jumps over the lazy dog",
        &[
            0x2f, 0xd4, 0xe1, 0xc6, 0x7a, 0x2d, 0x28, 0xfc, 0xed, 0x84, 0x9e, 0xe1, 0xbb, 0x76,
            0xe7, 0x39, 0x1b, 0x93, 0xeb, 0x12,
        ],
    );
    complete_sequence(&mut tpm, handles[0], b"bc", HASHES[0].1);
    complete_sequence(
        &mut tpm,
        handles[1],
        b"",
        &[
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ],
    );
}

#[test]
fn test_hash_sequences_mixed_algorithms() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for allocation_order in ORDERS {
        for completion_order in ORDERS {
            tracing::debug!(?allocation_order, ?completion_order);
            let hashes = allocation_order.map(|index| HASHES[index]);
            let handles = hashes.map(|(algorithm, _)| start_sequence(&mut tpm, algorithm));
            for slot in completion_order.into_iter().rev() {
                update_sequence(&mut tpm, handles[slot], b"a");
            }
            for slot in completion_order {
                update_sequence(&mut tpm, handles[slot], b"b");
                complete_sequence(&mut tpm, handles[slot], b"c", hashes[slot].1);
            }
        }
    }
}

#[test]
fn test_hash_sequences_flush_and_reuse_slots() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for order in ORDERS {
        tracing::debug!(?order);
        let handles: [_; 3] = std::array::from_fn(|_| start_sequence(&mut tpm, HASHES[1].0));
        for handle in handles {
            update_sequence(&mut tpm, handle, b"a");
        }
        for slot in order {
            tpm.flush_context(handles[slot]).unwrap();
            let replacement = start_sequence(&mut tpm, HASHES[0].0);
            assert_eq!(replacement, handles[slot]);
            update_sequence(&mut tpm, replacement, b"ab");
        }
        for slot in order.into_iter().rev() {
            complete_sequence(&mut tpm, handles[slot], b"c", HASHES[0].1);
        }
    }
}

#[test]
fn test_hash_sequences_complete_and_reuse_slots() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for order in ORDERS {
        tracing::debug!(?order);
        let handles: [_; 3] = std::array::from_fn(|_| start_sequence(&mut tpm, HASHES[0].0));
        for handle in handles {
            update_sequence(&mut tpm, handle, b"a");
        }
        for slot in order {
            complete_sequence(&mut tpm, handles[slot], b"bc", HASHES[0].1);
            let replacement = start_sequence(&mut tpm, HASHES[2].0);
            assert_eq!(replacement, handles[slot]);
            update_sequence(&mut tpm, replacement, b"ab");
        }
        for slot in order.into_iter().rev() {
            complete_sequence(&mut tpm, handles[slot], b"c", HASHES[2].1);
        }
    }
}

#[test]
fn test_hash_sequences_with_primary_objects() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for primary_slot in 0..3 {
        for order in ORDERS {
            tracing::debug!(primary_slot, ?order);
            let handles: [_; 3] = std::array::from_fn(|slot| {
                let handle = if slot == primary_slot {
                    tpm.create_primary(TPM20_RH_OWNER, rsa_srk_template().unwrap())
                        .unwrap()
                        .object_handle
                } else {
                    start_sequence(&mut tpm, HASHES[0].0)
                };
                assert_eq!(handle, ReservedHandle::new(0x80, slot as u32));
                handle
            });
            let primary = handles[primary_slot];
            let public = tpm.read_public(primary).unwrap().out_public.serialize();
            for slot in order {
                if slot != primary_slot {
                    update_sequence(&mut tpm, handles[slot], b"ab");
                }
            }
            assert_eq!(
                tpm.read_public(primary).unwrap().out_public.serialize(),
                public
            );
            for slot in order {
                if slot == primary_slot {
                    tpm.flush_context(primary).unwrap();
                } else {
                    complete_sequence(&mut tpm, handles[slot], b"c", HASHES[0].1);
                }
                let replacement = start_sequence(&mut tpm, HASHES[0].0);
                assert_eq!(replacement, handles[slot]);
                update_sequence(&mut tpm, replacement, b"ab");
            }
            for slot in order.into_iter().rev() {
                complete_sequence(&mut tpm, handles[slot], b"c", HASHES[0].1);
            }
        }
    }
}

#[test]
fn test_hash_sequences_context_roundtrip_different_slots() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for order in ORDERS {
        tracing::debug!(?order);
        let hashes = HASHES;
        let handles = hashes.map(|(algorithm, _)| start_sequence(&mut tpm, algorithm));
        let prefixes: [&[u8]; 3] = [b"a", b"ab", b""];
        let suffixes: [&[u8]; 3] = [b"bc", b"c", b"abc"];
        for slot in order {
            update_sequence(&mut tpm, handles[slot], prefixes[slot]);
        }
        let contexts = handles.map(|handle| {
            execute_command(
                &mut tpm,
                SessionTagEnum::NoSessions,
                CommandCodeEnum::ContextSave,
                handle.as_bytes(),
                ResponseCode::Success,
            )
        });
        for slot in order {
            tpm.flush_context(handles[slot]).unwrap();
        }
        let restored = order.map(|slot| {
            let reply = execute_command(
                &mut tpm,
                SessionTagEnum::NoSessions,
                CommandCodeEnum::ContextLoad,
                &contexts[slot],
                ResponseCode::Success,
            );
            ReservedHandle::read_from_bytes(&reply).unwrap()
        });
        for index in (0..3).rev() {
            let slot = order[index];
            assert_eq!(restored[index], ReservedHandle::new(0x80, index as u32));
            complete_sequence(&mut tpm, restored[index], suffixes[slot], hashes[slot].1);
        }
    }
}
