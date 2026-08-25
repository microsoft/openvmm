// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    prost_build::Config::new()
        .type_attribute(".", "#[derive(mesh::MeshPayload)]")
        .type_attribute(".", "#[mesh(prost)]")
        .service_generator(Box::new(mesh_build::MeshServiceGenerator::new()))
        .compile_protos(&["src/vmservice.proto"], &["src"])
        .unwrap();

    // TODO: std::fs::read_dir to (recursively) enumerate all `src/**/*.proto` files
    // Tell cargo to recompile if any of these proto files are changed
    let _ = [
        "src/vmservice.proto",
        "src/vmservice.events.proto",
        "src/vmservice.resource.proto",
        "src/vmservice.scsi.proto",
        "src/vmservice.state.proto",
    ]
    .map(|f| println!("cargo:rerun-if-changed={f}"));
}
