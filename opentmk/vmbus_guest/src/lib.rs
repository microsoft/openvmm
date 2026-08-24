// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side VMBus client for opentmk-invariant.
//!
//! This crate implements the guest half of the VMBus protocol. It targets
//! `#![no_std] + alloc` and runs inside a UEFI image alongside the rest of
//! the opentmk framework.
//!
//! # Scope
//!
//! * SynIC bring-up on the current VP (SIMP/SIEFP/SCONTROL/SINT2).
//! * `InitiateContact`/`VersionResponse` negotiation with newest-to-oldest
//!   fallback down to `Version::Copper` (6.0).
//! * `RequestOffers`/`OfferChannel`/`AllOffersDelivered` enumeration.
//! * `GpadlHeader`/`GpadlBody`/`GpadlTeardown` create/teardown.
//! * `OpenChannel`/`OpenChannelResult`/`CloseChannel`/`RelIdReleased`.
//! * Ring-buffer send/recv with correct empty→non-empty signalling and
//!   the `pending_send_sz` back-pressure protocol.
//! * Message-page draining with EOM under SINT2.
//! * `Unload`/`UnloadComplete` on shutdown.
//! * A [`devices::keyboard`] driver (Hyper-V synthetic keyboard).
//! * A [`devices::netvsp`] driver (Hyper-V synthetic NIC) with RNDIS
//!   init, Ethernet TX/RX and the `pending_tx` completion tracker.
//! * hvsocket wire structs (`TlConnectRequest[/2]`, `TlConnectResult`,
//!   `HvsockUserDefinedParameters`) only — no high-level socket API.
//!
//! # Getting started
//!
//! The crate is designed as a set of small building blocks — one module
//! per protocol concern. The typical bring-up path is:
//!
//! ```text
//! synic::init_synic  ─┐
//! connection::initiate┼─►  init(ctx)
//!                     │        │
//!                     │        ▼
//!                     │  request_offers(ctx) → Vec<OfferChannel>
//!                     │        │  (pick the offer you want)
//!                     │        ▼
//!                     │  channel::open_channel(...)
//!                     │        │  (or a device-specific `open`, e.g. `Netvsp::open`)
//!                     │        ▼
//!                     │  ring::SendRing / RecvRing  ← per-channel I/O
//!                     │        │
//!                     └────►  unload(ctx)
//! ```
//!
//! All hypercalls flow through a caller-supplied [`HypercallPlatformTrait`]
//! implementation (typically
//! [`HvTestCtx`](opentmk_core::platform::hyperv::ctx::HvTestCtx)); this crate
//! never touches the raw hypercall page itself.
//!
//! ## 1. Initialize the stack
//!
//! [`init`] performs SynIC bring-up on the current VP and completes the
//! version handshake. Call it exactly once per session:
//!
//! ```ignore
//! use opentmk_core::platform::hyperv::ctx::HvTestCtx;
//!
//! let mut ctx = HvTestCtx::new();
//! // ...caller-specific ctx setup (hypercall page, etc.)...
//!
//! vmbus_guest::init(&mut ctx)?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! Under `target_os = "uefi"` this also allocates the SIMP + SIEFP
//! pages. On non-UEFI (host-test) targets [`synic::init_synic`] returns
//! `Error::NotImplemented`; unit-test the pieces via
//! [`synic::program_synic_registers`] and
//! [`connection::negotiate_version`] with a mock ctx instead.
//!
//! ## 2. Enumerate offers
//!
//! Ask the host for the current channel catalogue:
//!
//! ```ignore
//! let offers = vmbus_guest::request_offers(&mut ctx)?;
//! for offer in &offers {
//!     log::info!(
//!         "offer: iid={:?} instance={:?} channel_id={:?}",
//!         offer.interface_id,
//!         offer.interface_instance,
//!         offer.channel_id,
//!     );
//! }
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! Well-known device GUIDs are defined by their vdev module — e.g.
//! [`devices::keyboard::INTERFACE_GUID`] and
//! [`devices::netvsp::INTERFACE_GUID`]. Compare each offer's
//! `interface_id` against the GUIDs you care about.
//!
//! ## 3. Open a channel
//!
//! The lowest-level entry point is [`channel::open_channel`], which
//! allocates ring pages, establishes a GPADL, and posts
//! `OpenChannel[/2]`. Most callers should use the vdev-specific
//! helpers instead — they encapsulate ring sizing + subsequent
//! negotiation.
//!
//! ### Example: the synthetic keyboard
//!
//! ```ignore
//! use vmbus_guest::devices::keyboard;
//!
//! let kbd_offer = offers
//!     .iter()
//!     .find(|o| o.interface_id == keyboard::INTERFACE_GUID)
//!     .ok_or(vmbus_guest::Error::NotFound)?;
//! let mut kbd = keyboard::Keyboard::open(&mut ctx, kbd_offer)?;
//! kbd.negotiate_version(&mut ctx, keyboard::VERSION_WIN8, 1_000_000)?;
//!
//! // Poll for a keystroke (bounded — never blocks indefinitely).
//! let mut buf = [0u8; 128];
//! if let Some(ks) = kbd.poll_keystrokes(&mut buf, 1_000_000)? {
//!     log::info!("keystroke: make_code={:#x}", ks.make_code);
//! }
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! ### Example: the synthetic NIC (netvsp)
//!
//! See [`devices::netvsp`] for the full bring-up sequence. In short:
//!
//! ```ignore
//! use vmbus_guest::devices::netvsp::{self, rndis, Netvsp};
//!
//! let nic_offer = offers
//!     .iter()
//!     .find(|o| o.interface_id == netvsp::INTERFACE_GUID)
//!     .ok_or(vmbus_guest::Error::NotFound)?;
//!
//! let mut nic = Netvsp::open(&mut ctx, nic_offer)?;
//! let ver = nic.negotiate_version(&mut ctx)?;
//! nic.send_ndis_config(&mut ctx, /*mtu=*/ 1500)?;
//! nic.send_ndis_version(&mut ctx)?;
//! nic.establish_recv_buffer(&mut ctx, 16 * 1024 * 1024)?;
//! nic.establish_send_buffer(&mut ctx,  1 * 1024 * 1024)?;
//! nic.rndis_init(&mut ctx)?;
//! nic.set_packet_filter(
//!     &mut ctx,
//!     rndis::NDIS_PACKET_TYPE_DIRECTED
//!         | rndis::NDIS_PACKET_TYPE_BROADCAST
//!         | rndis::NDIS_PACKET_TYPE_ALL_MULTICAST
//!         | rndis::NDIS_PACKET_TYPE_PROMISCUOUS,
//! )?;
//!
//! // Send-and-wait: blocks until the paired completion arrives.
//! nic.send_ethernet(&mut ctx, &arp_frame, /*wait=*/ true)?;
//!
//! // Drain any inbound frames (host → guest).
//! nic.drain_inbound(&mut ctx, /*max_polls=*/ 1_000_000, |frame| {
//!     log::info!("rx {} bytes", frame.len());
//! })?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! ## 4. I/O discipline: ring signalling and TX completions
//!
//! Two subtleties trip up first-time callers of the ring API:
//!
//! * **Signal only on the empty→non-empty transition.**
//!   [`ring::SendRing::write_packet`] (and its wrappers
//!   `write_inband`, `write_completion`, `write_gpa_direct`) return
//!   `Ok(true)` exactly when this write crossed the ring from empty
//!   to non-empty **and** the host hasn't masked interrupts.
//!   Signal via [`channel::Channel::signal`] only when the returned
//!   `bool` is `true`. Signalling on every packet works, but wakes
//!   the host once per packet and risks Hyper-V's DoS throttling.
//!   The vdev helpers do the right thing already.
//!
//! * **Drain the recv ring frequently.** For any TX buffer that
//!   references guest memory (netvsp uses GPA-direct), the host holds
//!   that memory until it delivers a `VM_PKT_COMP` on the recv ring.
//!   If the guest never drains, TX completions pile up, the recv ring
//!   fills, and the host stalls forward progress on both sides.
//!   Bursty callers must interleave [`devices::netvsp::Netvsp::drain_inbound`]
//!   (or [`devices::netvsp::Netvsp::flush_tx`] at the end) with their
//!   sends.
//!
//! ### Example: a bounded fire-and-forget burst
//!
//! ```ignore
//! for i in 0..1024 {
//!     match nic.send_ethernet(&mut ctx, &frame, /*wait=*/ false) {
//!         Ok(()) => {}
//!         Err(vmbus_guest::Error::RingFull) => {
//!             // Ring is full — drain completions and retry.
//!             nic.drain_inbound(&mut ctx, /*max_polls=*/ 100_000, |_| {})?;
//!             nic.send_ethernet(&mut ctx, &frame, /*wait=*/ false)?;
//!         }
//!         Err(e) => return Err(e),
//!     }
//!     if i % 32 == 0 {
//!         // Periodic drain keeps the pending-TX queue bounded and
//!         // the host's transfer-page pool alive.
//!         nic.drain_inbound(&mut ctx, /*non_blocking=*/ 0, |_| {})?;
//!     }
//! }
//! // Flush before returning — waits for every outstanding TX
//! // completion and frees the backing allocations.
//! nic.flush_tx(&mut ctx, /*max_polls=*/ 10_000_000, |_| {})?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! ## 5. Shutdown
//!
//! [`unload`] posts `Unload`, waits for `UnloadComplete`, and clears
//! the process-wide connection state. Any subsequent VMBus operation
//! must go through [`init`] again.
//!
//! ```ignore
//! vmbus_guest::unload(&mut ctx)?;
//! # Ok::<_, vmbus_guest::Error>(())
//! ```
//!
//! Channels do not currently expose an idempotent teardown; per-channel
//! drop leaks the ring GPADL and any device-owned buffers (netvsp
//! specifically leaks its 16 MiB recv + 1 MiB send buffers). This is
//! acceptable for the intended TMK / smoke-test workloads where the
//! whole VM is torn down after the run.
//!
//! # Advanced entry points
//!
//! Every high-level function has a `_with` variant in the same module
//! that takes a [`message::CompletionTable`] and a
//! [`connection::MessagePump`], for callers that want to build a
//! composite pump (e.g. draining multiple SIMP slots or interleaving
//! with a scheduler). The convenience wrappers use the process-wide
//! table + [`interrupt::SimpPump`]. Do not mix the two — a caller that
//! registers a completion with a custom table but uses [`init`]'s
//! implicit pump will never see its completion delivered.
//!
//! # Concurrency model
//!
//! The crate is single-threaded by design: UEFI runs on VP0 only, all
//! rings are single-producer / single-consumer, and the ordering
//! guarantees rely on `SeqCst` on both sides of the ring's
//! `pending_send_sz` Dekker rendezvous. Do not share a
//! [`ring::SendRing`] or [`ring::RecvRing`] across threads.
//!
//! # Testing
//!
//! The crate is fully unit-tested against a mock ctx on the host
//! target. UEFI-specific code (page allocation, real SINT2 delivery)
//! is exercised end-to-end by the `vmbus_e2e_guest` crate against a
//! live Hyper-V lab machine.
//!
//! # Non-goals
//!
//! * A real SINT2 ISR — [`SimpPump`](interrupt::SimpPump) polls with a
//!   bounded retry count.
//! * Reserved channels, `ModifyChannel`, monitor-page signalling, or
//!   confidential VMBus.
//! * A high-level hv-socket / pipe API (only wire helpers ship).
//! * Rescind / channel-teardown accounting beyond acking.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod channel;
pub mod connection;
pub mod devices;
mod error;
pub mod gpadl;
pub mod hvsock;
pub mod hypercalls;
pub mod interrupt;
pub mod message;
pub mod protocol;
pub mod ring;
pub mod synic;

#[cfg(test)]
mod tests;

pub use error::Error;
pub use error::Result;

use alloc::vec::Vec;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::HyperVHypercallConfig;

/// Translate a pointer (guest virtual address) to a guest-physical
/// address (GPA).
///
/// This crate assumes all memory it allocates is **identity-mapped**
/// — the guest-virtual address returned by the global allocator is
/// numerically equal to the guest-physical address the hypervisor
/// will see when it reads that page. Under this invariant
/// `virt_to_phys` is a zero-cost cast.
///
/// # Identity-map invariant
///
/// The UEFI specification (§2.3) requires that all memory described
/// in the UEFI memory map be identity-mapped during boot services,
/// on both x86_64 and aarch64. This is the environment this crate
/// runs in (`target_os = "uefi"`).
///
/// After `exit_boot_services` the UEFI-installed page tables remain
/// active until someone reprograms CR3 / TTBR; opentmk never does
/// this, so the mapping persists for the lifetime of the guest. The
/// post-EBS allocator (opentmk's static heap) hands out pages that
/// were identity-mapped at boot time.
///
/// # Load-bearing across the crate
///
/// Every place in this crate that hands a guest-physical address to
/// the hypervisor — SIMP/SIEFP register writes in
/// [`synic::init_synic`], GPADL PFN lists in
/// [`gpadl::establish_gpadl`], and GPA-direct external buffer
/// descriptors in [`ring::SendRing::write_gpa_direct`] — routes the
/// address through this function. If the identity-map invariant
/// ever fails (e.g. isolating opentmk under its own CR3, or a
/// paravisor VTL1 layout that remaps guest memory), replace the
/// body with a real translation and every consumer picks it up
/// automatically:
///
/// * `HvCallTranslateVirtualAddress` (hypercall `0x52`) — asks the
///   hypervisor to walk the guest's own page tables and return the
///   GPA. Preferred if available.
/// * Walk the guest's page tables directly — requires reading
///   CR3/TTBR and matching the paging mode.
///
/// # Not for host-supplied addresses
///
/// Do **not** call this on pointers or offsets that don't come from
/// the guest's own allocator — e.g. host-supplied recv-buffer
/// offsets or foreign GPAs delivered over the wire. Those are
/// already GPAs; passing them through this function is a no-op
/// today but conceptually wrong and would break under a
/// non-identity translation.
#[inline]
pub fn virt_to_phys<T>(ptr: *const T) -> u64 {
    ptr as u64
}

/// Initialise the guest-side VMBus stack: bring up SynIC on the current VP,
/// negotiate a protocol version with the host, and prime the message
/// dispatcher.
///
/// After this call succeeds, use [`request_offers`] to enumerate channels
/// and [`open_channel`](channel::open_channel) to open one.
///
/// Delegates to [`synic::init_synic`] and [`connection::initiate`].
///
/// # Example
///
/// ```ignore
/// let mut ctx = HvTestCtx::new();
/// // ...caller-specific ctx setup...
/// vmbus_guest::init(&mut ctx)?;
/// let offers = vmbus_guest::request_offers(&mut ctx)?;
/// // ...open channels, do I/O...
/// vmbus_guest::unload(&mut ctx)?;
/// # Ok::<_, vmbus_guest::Error>(())
/// ```
pub fn init<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(ctx: &mut C) -> Result<()> {
    synic::init_synic(ctx)?;
    connection::initiate(ctx)?;
    Ok(())
}

/// Post a `RequestOffers` and collect the returned `OfferChannel` messages
/// until `AllOffersDelivered` is received.
///
/// Returns the offers in the order the host delivered them. Compare
/// [`OfferChannel::interface_id`](protocol::OfferChannel::interface_id)
/// against a known device GUID (e.g.
/// [`devices::keyboard::INTERFACE_GUID`] or
/// [`devices::netvsp::INTERFACE_GUID`]) to pick the offer you want.
pub fn request_offers<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<Vec<protocol::OfferChannel>> {
    connection::request_offers(ctx)
}

/// Post an `Unload` and wait for `UnloadComplete`, then clear the
/// process-wide connection state.
///
/// After this returns, any subsequent VMBus operation must go through
/// [`init`] again.
pub fn unload<C: HypercallPlatformTrait<Config = HyperVHypercallConfig>>(
    ctx: &mut C,
) -> Result<()> {
    connection::unload(ctx)
}
