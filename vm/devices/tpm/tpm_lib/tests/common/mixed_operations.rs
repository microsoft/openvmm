// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mix NV, PCR, RNG, RSA, and event-sequence commands with hash sequences.

use super::create_tpm_engine_helper;
use super::generate_rsa;
use super::hash_sequences::HASHES;
use super::hash_sequences::ORDERS;
use super::hash_sequences::RH_NULL;
use super::hash_sequences::complete_sequence;
use super::hash_sequences::execute_command;
use super::hash_sequences::execute_command_status;
use super::hash_sequences::start_sequence;
use super::hash_sequences::update_sequence;
use super::rsa_srk_template;
use crate::test_tpm_backend::TestTpmPlatform;
use crate::tpm_lib::TpmEngineHelper;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::CommandCodeEnum;
use tpm_protocol::tpm20proto::ReservedHandle;
use tpm_protocol::tpm20proto::ResponseCode;
use tpm_protocol::tpm20proto::SessionTagEnum;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::TPM20_RS_PW;
use tpm_protocol::tpm20proto::protocol::PcrSelection;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmlPcrSelection;
use tpm_protocol::tpm20proto::protocol::common::CmdAuth;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const PCR0: ReservedHandle = ReservedHandle::new(0, 0);
const OWNER_NV_INDEX: u32 = 0x0100_0042;
const TPM_CAP_TPM_PROPERTIES: u32 = 6;
const TPM_PT_MANUFACTURER: u32 = 0x105;

fn start_event_sequence(tpm: &mut TpmEngineHelper<TestTpmPlatform>) -> ReservedHandle {
    start_sequence(tpm, AlgIdEnum::NULL as u16)
}

fn session_parameters(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    code: CommandCodeEnum,
    handles: &[&[u8]],
    parameters: &[u8],
) -> Vec<u8> {
    let auth = CmdAuth::new(TPM20_RS_PW, 0, 0, 0);
    let auth_size = (handles.len() * size_of::<CmdAuth>()) as u32;
    let mut payload = Vec::new();
    for handle in handles {
        payload.extend_from_slice(handle);
    }
    payload.extend_from_slice(&auth_size.to_be_bytes());
    for _ in handles {
        payload.extend_from_slice(auth.as_bytes());
    }
    payload.extend_from_slice(parameters);
    let reply = execute_command(
        tpm,
        SessionTagEnum::Sessions,
        code,
        &payload,
        ResponseCode::Success,
    );
    let parameter_size = u32::from_be_bytes(reply[..4].try_into().unwrap()) as usize;
    reply[4..4 + parameter_size].to_vec()
}

fn complete_event_sequence(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    sequence: ReservedHandle,
    data: &[u8],
) -> Vec<u8> {
    let auth = CmdAuth::new(TPM20_RS_PW, 0, 0, 0);
    let mut payload = Vec::new();
    payload.extend_from_slice(PCR0.as_bytes());
    payload.extend_from_slice(sequence.as_bytes());
    payload.extend_from_slice(&((2 * size_of::<CmdAuth>()) as u32).to_be_bytes());
    payload.extend_from_slice(auth.as_bytes());
    payload.extend_from_slice(auth.as_bytes());
    payload.extend_from_slice(&Tpm2bBuffer::new(data).unwrap().serialize());
    let reply = execute_command(
        tpm,
        SessionTagEnum::Sessions,
        CommandCodeEnum::EventSequenceComplete,
        &payload,
        ResponseCode::Success,
    );
    let parameter_size = u32::from_be_bytes(reply[..4].try_into().unwrap()) as usize;
    let results = reply[4..4 + parameter_size].to_vec();
    assert!(u32::from_be_bytes(results[..4].try_into().unwrap()) >= 1);
    results
}

fn one_shot_hash(
    tpm: &mut TpmEngineHelper<TestTpmPlatform>,
    algorithm: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut payload = Tpm2bBuffer::new(data).unwrap().serialize();
    payload.extend_from_slice(&algorithm.to_be_bytes());
    payload.extend_from_slice(RH_NULL.as_bytes());
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::Hash,
        &payload,
        ResponseCode::Success,
    );
    let digest = Tpm2bBuffer::deserialize(&reply).unwrap();
    digest.buffer[..digest.size.get() as usize].to_vec()
}

fn get_random(tpm: &mut TpmEngineHelper<TestTpmPlatform>, bytes: u16) -> Vec<u8> {
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::GetRandom,
        &bytes.to_be_bytes(),
        ResponseCode::Success,
    );
    let random = Tpm2bBuffer::deserialize(&reply).unwrap();
    assert_eq!(random.size.get(), bytes);
    random.buffer[..bytes as usize].to_vec()
}

fn stir_random(tpm: &mut TpmEngineHelper<TestTpmPlatform>, data: &[u8]) {
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::StirRandom,
        &Tpm2bBuffer::new(data).unwrap().serialize(),
        ResponseCode::Success,
    );
    assert!(reply.is_empty());
}

fn read_clock(tpm: &mut TpmEngineHelper<TestTpmPlatform>) {
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::ReadClock,
        &[],
        ResponseCode::Success,
    );
    assert!(reply.len() > 16);
}

fn get_manufacturer(tpm: &mut TpmEngineHelper<TestTpmPlatform>) {
    let payload = [
        TPM_CAP_TPM_PROPERTIES.to_be_bytes(),
        TPM_PT_MANUFACTURER.to_be_bytes(),
        1u32.to_be_bytes(),
    ]
    .concat();
    let reply = execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::GetCapability,
        &payload,
        ResponseCode::Success,
    );
    assert!(reply.len() > 8);
}

fn pcr_read(tpm: &mut TpmEngineHelper<TestTpmPlatform>) -> Vec<u8> {
    let selection = TpmlPcrSelection::new(&[PcrSelection {
        hash: AlgIdEnum::SHA256.into(),
        size_of_select: 3,
        bitmap: [1, 0, 0],
    }])
    .unwrap();
    execute_command(
        tpm,
        SessionTagEnum::NoSessions,
        CommandCodeEnum::PCR_Read,
        &selection.serialize(),
        ResponseCode::Success,
    )
}

fn pcr_event(tpm: &mut TpmEngineHelper<TestTpmPlatform>, data: &[u8]) -> Vec<u8> {
    session_parameters(
        tpm,
        CommandCodeEnum::PCR_Event,
        &[PCR0.as_bytes()],
        &Tpm2bBuffer::new(data).unwrap().serialize(),
    )
}

fn nv_roundtrip(tpm: &mut TpmEngineHelper<TestTpmPlatform>, data: &[u8]) {
    tpm.nv_define_space(TPM20_RH_OWNER, 0, OWNER_NV_INDEX, data.len() as u16)
        .unwrap();
    tpm.nv_write(TPM20_RH_OWNER, None, OWNER_NV_INDEX, data)
        .unwrap();
    let mut read_back = vec![0u8; data.len()];
    tpm.nv_read(
        TPM20_RH_OWNER,
        OWNER_NV_INDEX,
        data.len() as u16,
        &mut read_back,
    )
    .unwrap();
    assert_eq!(read_back, data);
    tpm.nv_undefine_space(TPM20_RH_OWNER, OWNER_NV_INDEX)
        .unwrap();
}

fn background_ops(tpm: &mut TpmEngineHelper<TestTpmPlatform>, salt: &[u8]) {
    stir_random(tpm, salt);
    let random = get_random(tpm, 16);
    assert_ne!(random, [0; 16]);
    read_clock(tpm);
    get_manufacturer(tpm);
    assert_eq!(one_shot_hash(tpm, HASHES[0].0, b"abc"), HASHES[0].1);
    pcr_read(tpm);
}

fn flush_all(tpm: &mut TpmEngineHelper<TestTpmPlatform>) {
    for slot in 0..3 {
        tpm.flush_context(ReservedHandle::new(0x80, slot)).ok();
    }
}

#[test]
fn test_mixed_nv_pcr_rng_with_hash_and_event_sequences() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for order in ORDERS {
        tracing::debug!(?order);
        background_ops(&mut tpm, b"start");

        let mut handles = [ReservedHandle::new(0, 0); 3];
        for (tpm_slot, kind) in order.into_iter().enumerate() {
            handles[kind] = if kind == 2 {
                start_event_sequence(&mut tpm)
            } else {
                start_sequence(&mut tpm, HASHES[kind].0)
            };
            assert_eq!(handles[kind], ReservedHandle::new(0x80, tpm_slot as u32));
        }

        nv_roundtrip(&mut tpm, b"nv-payload");
        pcr_event(&mut tpm, b"pcr-event");
        background_ops(&mut tpm, b"mid");

        update_sequence(&mut tpm, handles[0], b"ab");
        update_sequence(&mut tpm, handles[2], b"event");
        update_sequence(&mut tpm, handles[1], b"ab");

        let mut complete_payload = Tpm2bBuffer::new(b"").unwrap().serialize();
        complete_payload.extend_from_slice(RH_NULL.as_bytes());
        let auth = CmdAuth::new(TPM20_RS_PW, 0, 0, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(handles[2].as_bytes());
        payload.extend_from_slice(&(size_of::<CmdAuth>() as u32).to_be_bytes());
        payload.extend_from_slice(auth.as_bytes());
        payload.extend_from_slice(&complete_payload);
        let (status, _) = execute_command_status(
            &mut tpm,
            SessionTagEnum::Sessions,
            CommandCodeEnum::SequenceComplete,
            &payload,
        );
        assert_ne!(status, ResponseCode::Success as u32);

        for slot in order.into_iter().rev() {
            if slot == 2 {
                complete_event_sequence(&mut tpm, handles[2], b"-data");
            } else {
                complete_sequence(&mut tpm, handles[slot], b"c", HASHES[slot].1);
            }
        }

        background_ops(&mut tpm, b"end");
    }
}

#[test]
fn test_mixed_rsa_import_load_with_hash_and_event_slots() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for load_slot in [1, 2] {
        tracing::debug!(load_slot);
        let srk = tpm
            .create_primary(TPM20_RH_OWNER, rsa_srk_template().unwrap())
            .unwrap();
        assert_eq!(srk.object_handle, ReservedHandle::new(0x80, 0));
        let srk_public = tpm
            .read_public(srk.object_handle)
            .unwrap()
            .out_public
            .serialize();

        let (rsa_public, rsa_private) = generate_rsa();
        let object_public = Tpm2bPublic::new(rsa_public);
        let duplicate = Tpm2bBuffer::new(&rsa_private.serialize()).unwrap();
        let imported = tpm
            .import(
                srk.object_handle,
                &object_public,
                &duplicate,
                &Tpm2bBuffer::new_zeroed(),
            )
            .unwrap();

        let hash = start_sequence(&mut tpm, HASHES[0].0);
        assert_eq!(hash, ReservedHandle::new(0x80, 1));
        let event = start_event_sequence(&mut tpm);
        assert_eq!(event, ReservedHandle::new(0x80, 2));

        background_ops(&mut tpm, b"rsa");
        nv_roundtrip(&mut tpm, &[1, 2, 3, 4]);
        pcr_event(&mut tpm, b"with-keys");
        assert_eq!(
            tpm.read_public(srk.object_handle)
                .unwrap()
                .out_public
                .serialize(),
            srk_public
        );

        update_sequence(&mut tpm, hash, b"ab");
        update_sequence(&mut tpm, event, b"evt");

        if load_slot == 1 {
            complete_sequence(&mut tpm, hash, b"c", HASHES[0].1);
        } else {
            complete_event_sequence(&mut tpm, event, b"");
        }

        let loaded = tpm
            .load(srk.object_handle, &imported.out_private, &object_public)
            .unwrap()
            .object_handle;
        assert_eq!(loaded, ReservedHandle::new(0x80, load_slot as u32));
        tpm.read_public(loaded).unwrap();
        tpm.flush_context(loaded).unwrap();

        if load_slot == 1 {
            complete_event_sequence(&mut tpm, event, b"");
        } else {
            complete_sequence(&mut tpm, hash, b"c", HASHES[0].1);
        }
        tpm.flush_context(srk.object_handle).unwrap();
        flush_all(&mut tpm);
    }
}

#[test]
fn test_mixed_slot_kinds_hash_event_and_primary() {
    let mut tpm = create_tpm_engine_helper();
    tpm.initialize_tpm_engine().unwrap();

    for kind_order in ORDERS {
        for op_order in ORDERS {
            tracing::debug!(?kind_order, ?op_order);
            let handles: [_; 3] = std::array::from_fn(|slot| {
                let handle = match kind_order[slot] {
                    0 => start_sequence(&mut tpm, HASHES[0].0),
                    1 => start_event_sequence(&mut tpm),
                    _ => {
                        tpm.create_primary(TPM20_RH_OWNER, rsa_srk_template().unwrap())
                            .unwrap()
                            .object_handle
                    }
                };
                assert_eq!(handle, ReservedHandle::new(0x80, slot as u32));
                handle
            });

            background_ops(&mut tpm, b"kinds");

            for slot in op_order {
                match kind_order[slot] {
                    0 => update_sequence(&mut tpm, handles[slot], b"ab"),
                    1 => update_sequence(&mut tpm, handles[slot], b"ev"),
                    _ => {
                        tpm.read_public(handles[slot]).unwrap();
                    }
                }
            }

            nv_roundtrip(&mut tpm, b"slot-kinds");
            pcr_event(&mut tpm, b"kinds");

            for slot in op_order.into_iter().rev() {
                match kind_order[slot] {
                    0 => complete_sequence(&mut tpm, handles[slot], b"c", HASHES[0].1),
                    1 => {
                        complete_event_sequence(&mut tpm, handles[slot], b"t");
                    }
                    _ => tpm.flush_context(handles[slot]).unwrap(),
                }
            }
        }
    }
}
