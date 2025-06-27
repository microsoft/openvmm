// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Implemented in inline asm.
        return;
    }
    if std::env::var_os("SPARSE_MMAP_NO_BUILD").is_some() {
        return;
    }
    let mut build = cc::Build::new();
    build.file("src/trycopy.c").warnings_into_errors(true);

    for (a, b) in std::env::vars() {
        eprintln!("note: {}={}", a, b);
    }
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").unwrap() == "gnu"
    {
        if get_tool_var("CC").is_none() {
            // clang is required for SEH support.
            build.compiler("clang");
        }
        // ms-extensions is required for SEH support.
        build.flag("-fms-extensions");
    }

    build.compile("trycopy");
    println!("cargo:rerun-if-changed=src/trycopy.c");
}

fn get_tool_var(name: &str) -> Option<String> {
    let target = std::env::var("TARGET").unwrap().replace('-', "_");
    let var = format!("{}_{}", name, target);
    println!("cargo:rerun-if-env-changed={}", var);
    std::env::var(var)
        .or_else(|_| {
            println!("cargo:rerun-if-env-changed={}", name);
            std::env::var(name)
        })
        .ok()
}
