// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::io;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator::new()))
        .compile_protos(&["src/vmservice.proto"], &["src"])?;

    // There's an edge case where cargo build does not run if a new proto file is added to `src/`,
    // but it is neither added to the existing proto files as an import, nor to `build.rs` as a proto
    // file to compile.
    // That's acceptable, since its a fairly pathological case.
    rerun_on_proto_files("src".as_ref())?;
    Ok(())
}

fn rerun_on_proto_files(dir: &Path) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::fs::read_dir;

    // Could also filter and flatten `fs::ReadDir` to enumerate all `*.proto` files first.
    if dir.is_dir() {
        for entry in read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                rerun_on_proto_files(&path)?;
            } else if path.extension() == Some(OsStr::new("proto")) {
                println!("cargo::rerun-if-changed={}", path.display());
            }
        }
    }
    Ok(())
}
