// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = &[
        "src/vtl2_settings.proto",
        "src/vtl2_settings.namespaces.proto",
    ];

    // Tell cargo to recompile if any of these proto files are changed
    for proto_file in proto_files {
        println!("cargo:rerun-if-changed={proto_file}");
    }

    let descriptor_path =
        PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("proto_descriptor.bin");

    prost_build::Config::new()
        // Save descriptors to file
        .file_descriptor_set_path(&descriptor_path)
        // Override prost-types with pbjson-types
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        // Generate prost structs
        .compile_protos(proto_files, &["src"])?;

    let descriptor_set = std::fs::read(descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)?
        .build(&[".underhill"])?;

    Ok(())
}
