// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The various "backends" for components that get attached to the virtual
//! motherboard.
//!
//! At this time, the only supported backing is `Arc<CloseableMutex<dyn ChipsetDevice>>`,
//! but future backings will include Mesh-based remote `ChipsetDevice`
//! implementations, state-unit backed VMBus devices, etc...

pub mod arc_mutex;
