// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_std]
//! This contains the packet API library used for communication between the inv-agent and opentmk
//! components.
//!
//! In general, a communication channel between the inv-agent and opentmk works first by
//! establishing a three-way-handshake (SYN, SYN-ACK, ACK) which is entirely just a magic number
//! being exchanged between the two parties. Currently we assume that the inv-agent would start the
//! communication.
//!
//! Then after that, packets of type [`OpenTMKPacket`], with corresponding magic header and footer
//! value (defined in [`COMMS_PACKET_HEADER_MAGIC`] and [`COMMS_PACKET_FOOTER_MAGIC`]) will be
//! exchanged as the inv-agent executes test cases. Usually this is done with sending a single
//! configuration packet followed by a back and forth exchange of test case packets with the
//! opentmk ack'ing each packet.
extern crate alloc;

use alloc::{string::String, vec::Vec};

use serde::{Deserialize, Serialize};

/// Magic value used in the SYN packet
pub const COMMS_SYN_MAGIC: u64 = 0x4142434445464748;
/// Magic value used in the SYN-ACK packet (opentmk acknowledging SYN)
pub const COMMS_SYN_ACK_MAGIC: u64 = 0x5152535455565758;
/// Magic value used in the ACK packet (agent acknowledging SYN-ACK)
pub const COMMS_ACK_MAGIC: u64 = 0x6162636465666768;
/// Magic value used as the header of a regular packet
pub const COMMS_PACKET_HEADER_MAGIC: u64 = 0xf0f1f2f3f4f5f6f7;
/// Magic value used as the footer of a regular packet
pub const COMMS_PACKET_FOOTER_MAGIC: u64 = !COMMS_PACKET_HEADER_MAGIC;

/// Describes the specific serialization format used for encoding test cases to the opentmk
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum OpenTMKGrammarDeserializer {
    /// No encoding -- just use raw bytes
    None,
    /// syz-decoder. Used in conjunction of syzkaller fuzzers
    SyzDecoder,
}

/// Packet used to configure the opentmk instance with
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenTMKConfigurationPacket {
    /// Specify the type of serialization format for future test cases
    pub deserializer: OpenTMKGrammarDeserializer,
    /// A serialization-specific mapping that describes how (if any) call codes
    /// will correspond to specific functions
    pub mapping: Vec<u8>,
}

/// Packet used to run a test case
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenTMKFuzzTest {
    /// Specify a timeout (in microseconds) of the entire test case
    pub timeout_usec: u64,
    /// The encoded test case to run on the main thread
    pub testcase_vcpu0: Vec<u8>,
}

/// Packet used to relay error messages out
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenTMKErrorPacket {
    /// The message string
    pub message: String,
}

/// Packet used to acknowledge the completion of a single fuzz test case
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenTMKAckPacket {
    /// The error code (or zero if success)
    pub code: u64,
}

/// Represents any packets that may be transmitted to and from the TMK
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum OpenTMKPacket {
    /// An [`OpenTMKConfigurationPacket`] configuration packet
    Configuration(OpenTMKConfigurationPacket),
    /// An [`OpenTMKFuzzTest`] test case
    FuzzTest(OpenTMKFuzzTest),
    /// An [`OpenTMKAckPacket`] acknowledgement to a test case
    Ack(OpenTMKAckPacket),
    /// An [`OpenTMKErrorPacket`] error message for any other catch-all errors.
    Error(OpenTMKErrorPacket),
}
