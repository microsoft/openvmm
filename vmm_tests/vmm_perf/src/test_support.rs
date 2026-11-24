// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) fn tempdir(name: &str) -> anyhow::Result<tempfile::TempDir> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("unit-tests");
    fs_err::create_dir_all(&root)?;
    tempfile::Builder::new()
        .prefix(&format!("{name}-"))
        .tempdir_in(root)
        .map_err(Into::into)
}
