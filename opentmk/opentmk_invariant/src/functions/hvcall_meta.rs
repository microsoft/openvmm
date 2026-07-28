// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side decoder for per-hypercall static metadata sent from the
//! host fuzzer.
//!
//! See `hvfuzztest/src/targets/hyperv.rs` on the producer side. The
//! struct definition here **must** stay byte-compatible with the
//! producer's `HvcallMeta` — they're intentionally duplicated to keep
//! `opentmk_invariant`'s dep graph small (TODO: unify in a shared crate
//! later).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Per-call static metadata. Wire-compatible with
/// `hvfuzztest::targets::hyperv::HvcallMeta`.
#[repr(C)]
#[derive(Copy, Clone, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct HvcallMeta {
    /// Hypercall code.
    pub code: u16,
    /// Fixed header size of the input struct, in bytes.
    pub header_size: u16,
    /// Variable-length array element size of the input struct, in
    /// bytes. `0` if there is no trailing array.
    pub element_size: u16,
    /// Reserved; expected to be `0`.
    pub _reserved: u16,
}

const _: () = assert!(size_of::<HvcallMeta>() == 8);

/// Unpacks the `meta` argument value (a 64-bit integer carried in a
/// [`FuzzFunctionVariable::Int`](crate::functions::FuzzFunctionVariable::Int))
/// into an [`HvcallMeta`].
///
/// The 8 bytes of the integer are reinterpreted directly via
/// `zerocopy` — no manual bit-shifts. Returns the decoded struct
/// (always succeeds, since 8 bytes always fit and there are no
/// validation constraints).
pub fn unpack_hvcall_meta(value: u64) -> HvcallMeta {
    // `read_from_bytes` requires exactly 8 bytes for an 8-byte struct,
    // which is what `to_le_bytes` always returns. So this never
    // panics.
    HvcallMeta::read_from_bytes(&value.to_le_bytes())
        .expect("HvcallMeta is 8 bytes (compiler-asserted)")
}
