// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SINT2 message-page draining.
//!
//! When the host has a vmbus message ready for us, it writes a
//! [`hvdef::HvMessage`] into the SIMP page (slot [`crate::synic::VMBUS_SINT`],
//! byte offset `256 * VMBUS_SINT` into the page) and delivers a SINT2
//! interrupt. This module implements the guest-side drain — the SINT2
//! ISR proper lands in a follow-up patch that installs a real
//! interrupt handler through the opentmk interrupt primitive; today we
//! poll the SIMP slot inside [`SimpPump`] because the scaffold hasn't
//! wired an ISR yet.
//!
//! # Drain protocol
//!
//! Per Linux `drivers/hv/hv.c::vmbus_isr` and Windows minkernel:
//!
//! 1. Read the slot's `HvMessageHeader`.
//! 2. If `typ == HvMessageTypeNone (0)`, no message — stop.
//! 3. Otherwise the payload is a vmbus message; hand
//!    `payload_buffer[..header.len]` off to
//!    [`crate::message::route_message`].
//! 4. Write [`HvMessageType::HvMessageTypeNone`] back into the slot
//!    header to signal the hypervisor we're done with it.
//! 5. If `header.flags.message_pending()` is set, tell the hypervisor
//!    we can now accept another message by writing to
//!    [`HV_REGISTER_EOM`] via `HvCallSetVpRegisters`. If not set, no
//!    further ack is required.

use crate::Error;
use crate::Result;
use crate::hypercalls::set_vp_register;
use crate::message::CompletionHandle;
use crate::message::CompletionTable;
use crate::message::MessageSink;
use crate::message::route_message;
#[cfg_attr(
    not(target_os = "uefi"),
    expect(unused_imports, reason = "used only in the UEFI SIMP-pump impl")
)]
use crate::synic::VMBUS_SINT;
use hvdef::HV_MESSAGE_PAYLOAD_SIZE;
use hvdef::HV_MESSAGE_SIZE;
use hvdef::HvMessageType;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use opentmk::context::HypercallTrait;

/// End-of-message register index used to ack a pending message.
///
/// This is the **virtual register** identifier (`HvRegisterName`)
/// used with `HvCallSetVpRegisters`, NOT the x86 MSR index
/// (`0x40000084`). See `hvdef::HvX64RegisterName::Eom`.
pub const HV_REGISTER_EOM: u32 = 0x000A0014;

/// Byte offset of slot `n` inside a 4 KiB SIMP page.
#[cfg_attr(
    not(target_os = "uefi"),
    expect(dead_code, reason = "used only in the UEFI SIMP-pump impl")
)]
const fn slot_offset(sint: u8) -> usize {
    HV_MESSAGE_SIZE * sint as usize
}

/// Result of inspecting the SIMP slot for [`VMBUS_SINT`].
///
/// Split out from the raw pointer-reading code so the parse logic can
/// be unit-tested by feeding it a raw byte slot.
#[derive(Debug)]
pub struct SlotView<'a> {
    /// The `HvMessageType` in the slot header.
    pub message_type: HvMessageType,
    /// Length of the payload in bytes.
    pub payload_len: u8,
    /// Whether the message-pending flag is set — if so, we owe the
    /// hypervisor an EOM after clearing the slot.
    pub message_pending: bool,
    /// The payload sub-slice (already truncated to `payload_len`).
    pub payload: &'a [u8],
}

/// Parse a raw slot's bytes without touching hardware. `slot` must be
/// at least [`HV_MESSAGE_SIZE`] bytes long.
///
/// Returns `Ok(None)` if the slot is empty (`HvMessageTypeNone`) —
/// callers can stop draining.
///
/// The buffer is not consumed; the caller separately clears the slot
/// header and issues EOM after processing the message.
pub fn read_slot(slot: &[u8]) -> Result<Option<SlotView<'_>>> {
    if slot.len() < HV_MESSAGE_SIZE {
        return Err(Error::Parse {
            ty: None,
            reason: "SIMP slot smaller than HV_MESSAGE_SIZE",
        });
    }
    let typ = HvMessageType(u32::from_le_bytes(slot[0..4].try_into().unwrap()));
    if typ == HvMessageType::HvMessageTypeNone {
        return Ok(None);
    }
    let payload_len = slot[4];
    let flags = slot[5];
    let message_pending = flags & 0x1 != 0;
    let payload_start = 16; // sizeof(HvMessageHeader)
    let payload_end = payload_start + payload_len as usize;
    if payload_end > payload_start + HV_MESSAGE_PAYLOAD_SIZE || payload_end > slot.len() {
        return Err(Error::Parse {
            ty: None,
            reason: "SIMP slot payload length out of range",
        });
    }
    Ok(Some(SlotView {
        message_type: typ,
        payload_len,
        message_pending,
        payload: &slot[payload_start..payload_end],
    }))
}

/// Clear the `HvMessageType` field of a slot in place — the guest's
/// signal to the hypervisor that the message has been consumed.
///
/// Byte-level, host-testable.
pub fn clear_slot(slot: &mut [u8]) {
    assert!(slot.len() >= HV_MESSAGE_SIZE);
    slot[0..4].copy_from_slice(&HvMessageType::HvMessageTypeNone.0.to_le_bytes());
}

/// Issue an end-of-message write via `HvCallSetVpRegisters`.
///
/// Called when the previous message had `message_pending` set — the
/// hypervisor is waiting for us to acknowledge before delivering the
/// next message.
pub fn write_eom<C: HypercallTrait>(ctx: &mut C) -> Result<()> {
    set_vp_register(
        ctx,
        HvRegisterName(HV_REGISTER_EOM),
        HvRegisterValue::from(0u64),
    )
}

/// Drain a single SIMP-slot access into the completion table / sink.
///
/// The routine reads the slot via [`read_slot`], routes the message
/// (if any) via [`route_message`], clears the slot, and issues an EOM
/// via [`write_eom`] when required.
///
/// Returns `true` if a message was processed, `false` if the slot was
/// empty.
pub fn drain_once<C: HypercallTrait, S: MessageSink + ?Sized>(
    ctx: &mut C,
    slot: &mut [u8],
    table: &CompletionTable,
    sink: &mut S,
) -> Result<bool> {
    let Some(view) = read_slot(slot)? else {
        return Ok(false);
    };
    let needs_eom = view.message_pending;
    // Copy the payload out before we touch the slot — `slot` is
    // borrowed mutably below.
    let mut payload_buf: [u8; HV_MESSAGE_PAYLOAD_SIZE] = [0; HV_MESSAGE_PAYLOAD_SIZE];
    let payload_len = view.payload_len as usize;
    payload_buf[..payload_len].copy_from_slice(view.payload);

    // Log every message we see so we can trace routing decisions.
    let vmbus_ty = if payload_len >= 4 {
        u32::from_le_bytes(payload_buf[..4].try_into().unwrap())
    } else {
        u32::MAX
    };
    log::debug!(
        "drain_once: hv_typ={:#x} pending={} payload_len={} vmbus_typ={:#x}",
        view.message_type.0,
        needs_eom,
        payload_len,
        vmbus_ty,
    );

    route_message(&payload_buf[..payload_len], table, sink)?;

    clear_slot(slot);
    if needs_eom {
        write_eom(ctx)?;
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// SimpPump (UEFI-only implementation of [`crate::connection::MessagePump`])
// ---------------------------------------------------------------------------

/// A [`crate::connection::MessagePump`] that reads from the SIMP page
/// allocated by [`crate::synic::init_synic`].
///
/// # Retry policy
///
/// Each `poll_until` call polls the slot up to `max_retries` times,
/// sleeping via `core::hint::spin_loop()` between empty reads. When
/// `max_retries` is exhausted without seeing the awaited completion,
/// [`Error::Timeout`] is returned. This bound avoids the "waiter
/// hangs forever if the host drops a signal" footgun called out in §5
/// of the design doc.
///
/// Constructed with [`SimpPump::new`], which requires a valid SIMP GPA
/// (typically returned by [`crate::synic::synic_pages`]).
pub struct SimpPump {
    #[cfg_attr(
        not(target_os = "uefi"),
        expect(dead_code, reason = "used only in the UEFI SIMP-pump impl")
    )]
    simp_gpa: u64,
    max_retries: usize,
}

impl SimpPump {
    /// Reasonable default retry count for polling. Corresponds to
    /// roughly 100M spin-loop iterations (~5–10 s of wallclock) before
    /// we give up.
    pub const DEFAULT_MAX_RETRIES: usize = 100_000_000;

    /// Construct a pump that reads from the SIMP page at `simp_gpa`.
    pub fn new(simp_gpa: u64) -> Self {
        Self {
            simp_gpa,
            max_retries: Self::DEFAULT_MAX_RETRIES,
        }
    }

    /// Override the default retry count.
    pub fn with_max_retries(mut self, retries: usize) -> Self {
        self.max_retries = retries;
        self
    }
}

#[cfg(target_os = "uefi")]
impl crate::connection::MessagePump for SimpPump {
    fn poll_until<C: HypercallTrait>(
        &mut self,
        ctx: &mut C,
        handle: &CompletionHandle,
        sink: &mut dyn MessageSink,
    ) -> Result<()> {
        let table = crate::message::completion_table();
        let mut peek_count: usize = 0;
        for i in 0..self.max_retries {
            // SAFETY: `simp_gpa` is a live guest page programmed into
            // SIMP by `crate::synic::init_synic`; under UEFI the guest
            // memory is identity-mapped so we can address it as a raw
            // slice.
            #[expect(unsafe_code, reason = "raw SIMP page access")]
            let slot = unsafe {
                core::slice::from_raw_parts_mut(
                    (self.simp_gpa as *mut u8).add(slot_offset(VMBUS_SINT)),
                    HV_MESSAGE_SIZE,
                )
            };
            // Cheap non-empty check before the more expensive read_slot.
            let first_byte = slot[0];
            if first_byte != 0 {
                peek_count = peek_count.saturating_add(1);
                if peek_count <= 4 {
                    log::trace!(
                        "poll_until: iter={i} non-empty first_byte={first_byte:#x}"
                    );
                }
            }
            let drained = drain_once(ctx, slot, table, sink)?;
            if handle.completed() {
                log::debug!(
                    "poll_until: iter={i} handle completed (peek_count={peek_count})"
                );
                return Ok(());
            }
            if !drained {
                core::hint::spin_loop();
            }
        }
        log::warn!("poll_until: max_retries hit (peek_count={peek_count})");
        Err(Error::Timeout)
    }
}

#[cfg(not(target_os = "uefi"))]
impl crate::connection::MessagePump for SimpPump {
    fn poll_until<C: HypercallTrait>(
        &mut self,
        _ctx: &mut C,
        _handle: &CompletionHandle,
        _sink: &mut dyn MessageSink,
    ) -> Result<()> {
        Err(Error::NotImplemented)
    }
}
