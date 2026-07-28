// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SINT2 interrupt handling: message-page drain + event-flag drain.
//!
//! Called from the interrupt handler installed by [`crate::synic`] on
//! `VMBUS_INTERRUPT_VECTOR`. The routine must:
//! 1. Walk the SIEFP event flags page, clearing bits and waking any
//!    channel whose event flag fired.
//! 2. Drain the SIMP message page: if the current slot has a non-`INVALID`
//!    type, hand the payload off to [`crate::message`] and then EOI the
//!    slot by writing `MessageType::INVALID` and, if the message-pending
//!    bit is set, writing `HV_REGISTER_EOM` (see §4 of the design doc).
//!
//! **Not implemented in the scaffold.**

use crate::Error;
use crate::Result;

/// Register index used for end-of-message acknowledgement.
pub const HV_REGISTER_EOM: u32 = 0x40000084;

/// ISR entry point installed by [`crate::synic::init_synic`].
///
/// **Not implemented in the scaffold.** The follow-up patch:
/// - takes a static reference to the synic pages,
/// - drains the SIEFP flags with `AtomicU64::fetch_and(!bit, Acquire)`,
///   dispatching each set bit as a channel signal,
/// - drains the SIMP slot with an `Acquire` load of `MessageType`, hands
///   off to the message dispatcher, then does a `Release` store of
///   `MessageType::INVALID` before EOM.
pub fn vmbus_isr() -> Result<()> {
    Err(Error::NotImplemented)
}
