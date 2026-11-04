// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVSP + RNDIS wire types shared between host and guest netvsp drivers.
//!
//! # Modules
//!
//! - [`protocol`]: NVSP messages (`InitiateContact`, `SendReceiveBuffer`,
//!   `Nvsp1MsgSendRndisPacket`, version enums, feature bits, etc.).
//! - [`rndisprot`]: RNDIS messages carried inside `Nvsp1MsgSendRndisPacket`
//!   (`RndisMessageHeader`, `RndisPacket`, `RndisInitializeRequest`,
//!   `RndisSetRequest`, `NDIS_PACKET_TYPE_*`, `OID_*`, status codes).
//!
//! Both modules are `no_std + alloc`-friendly.

#![cfg_attr(not(test), no_std)]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod protocol;
pub mod rndisprot;
