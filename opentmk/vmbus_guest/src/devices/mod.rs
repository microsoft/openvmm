// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-side device drivers built on top of [`crate::channel`],
//! [`crate::gpadl`] and [`crate::ring`].
//!
//! Each module here targets a specific `interface_id` GUID from a
//! VMBus offer and speaks that device's channel protocol. These are
//! deliberately *thin* — enough to bring up the channel, drive its
//! handshake, and exercise the send / receive paths so we can smoke-
//! test the framework end-to-end.

pub mod keyboard;
