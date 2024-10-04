// Copyright (C) Microsoft Corporation. All rights reserved.

//! The Azure Profiler service protocol definitions.

// Crates used by generated code. Reference them explicitly to ensure that
// automated tools do not remove them.
use inspect as _;
use mesh_ttrpc as _;
use prost as _;

include!(concat!(env!("OUT_DIR"), "/profile.rs"));
