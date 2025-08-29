// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flowey pipelines used by the OpenVMM project

// DEVNOTE: this binary crate includes a `lib.rs` so that out-of-tree,
// closed-source flowey pipelines can reuse certain bits of the open-source
// flowey CLI and shared configuration logic (making it easier to have a
// consistent UX across open and closed-source).

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod pipelines;
pub mod pipelines_shared;

pub fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .into()
}
