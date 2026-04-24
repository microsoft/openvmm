// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A pure-Rust build system for building [Resource-only DLL] files containing
//! OpenHCL IGVM files.
//!
//! This DLL is used when packaging up "production" OpenHCL builds, such as
//! those that get shipped out to Azure.
//!
//! The primary benefit of packaging IGVM files into these resource DLLs is that
//! the resulting DLL files can be digitally signed, to ensure machines in
//! production are running verified builds of OpenHCL.
//!
//! # Building
//!
//! > NOTE: it is highly unlikely that you'll need to build this crate manually.
//! > Check the Guide for the most up-to-date guidance on what pipelines / tools
//! > can be used to generate vmfirmewareigvm.dll files.
//!
//! > WARNING: this crate will _not_ automatically sign resulting DLL files!
//!
//! In order to build this crate, several environment variables must be set.
//! These environment variables control the details of what metadata gets set in
//! the resulting DLL file, as well as what IGVM file gets included.
//!
//! For a detailed breakdown of what each environment variable does - see the
//! inline comments in the code below.
//!
//! Once those environment variables are set, a standard `cargo build -p
//! vmfirmwareigvm_dll` invocation should be sufficient to build the DLL. This
//! assumes you're running on Windows (or have Windows cross-compile set up).
//!
//! The resulting DLL will be emitted in the standard Rust output directory
//! (i.e: under target/...), and will be named `vmfirmwareigvm_dll.dll`.
//!
//! > NOTE: The "double-dll" naming is _not_ a bug, and is a natural consequence
//! > of how cargo names output artifacts according the the name of the crate.
//!
//! [Resource-only DLL]:
//!     https://learn.microsoft.com/en-us/cpp/build/creating-a-resource-only-dll?view=msvc-170

fn main() {
    if cfg!(feature = "ci") {
        return;
    }

    println!("cargo:rerun-if-env-changed=UH_DLL_NAME");
    println!("cargo:rerun-if-env-changed=UH_IGVM_PATH");
    println!("cargo:rerun-if-env-changed=UH_MAJOR");
    println!("cargo:rerun-if-env-changed=UH_MINOR");
    println!("cargo:rerun-if-env-changed=UH_PATCH");
    println!("cargo:rerun-if-env-changed=UH_REVISION");

    // If none of our env vars are set, do nothing instead of erroring.
    if std::env::var_os("UH_DLL_NAME").is_none()
        && std::env::var_os("UH_IGVM_PATH").is_none()
        && std::env::var_os("UH_MAJOR").is_none()
        && std::env::var_os("UH_MINOR").is_none()
        && std::env::var_os("UH_PATCH").is_none()
        && std::env::var_os("UH_REVISION").is_none()
    {
        println!(
            "cargo::warning=Attempted to build without setting UH_IGVM_PATH - resulting DLL will be empty!"
        );
        return;
    }

    // (string) corresponding the _internal_ DLL name reported by the DLL.
    //
    // (this does not correspond to the name of the DLL file emitted by cargo)
    let uh_dll_name = std::env::var("UH_DLL_NAME").expect("must set UH_DLL_NAME");

    // (path) absolute path to an IGVM file to package up
    let uh_igvm_path = std::env::var("UH_IGVM_PATH").expect("must set UH_IGVM_PATH");

    // (u16) Major version number of the DLL
    let uh_major = std::env::var("UH_MAJOR")
        .expect("must set UH_MAJOR")
        .parse::<u16>()
        .expect("UH_MAJOR must be a u16");

    // (u16) Minor version number of the DLL
    let uh_minor = std::env::var("UH_MINOR")
        .expect("must set UH_MINOR")
        .parse::<u16>()
        .expect("UH_MINOR must be a u16");

    // (u16) Patch version number of the DLL
    let uh_patch = std::env::var("UH_PATCH")
        .expect("must set UH_PATCH")
        .parse::<u16>()
        .expect("UH_PATCH must be a u16");

    // (u16) Revision version number of the DLL
    let uh_revision = std::env::var("UH_REVISION")
        .expect("must set UH_REVISION")
        .parse::<u16>()
        .expect("UH_REVISION must be a u16");

    // workaround for the fact that openvmm's root-level `.cargo/config.toml`
    // currently sets a bunch of extraneous linker flags via
    //
    // [target.'cfg(all(windows, target_env = "msvc"))']
    if option_env!("RUSTFLAGS")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(true)
    {
        panic!("must compile with RUSTFLAGS=\"\"")
    }

    let uh_version = format!("{uh_major},{uh_minor},{uh_patch},{uh_revision}");
    let uh_version_str = format!(r#""{uh_major}.{uh_minor}.{uh_patch}.{uh_revision}""#);

    assert!(std::path::Path::new(&uh_igvm_path).exists());

    let macros = [
        ("UH_DLL_NAME", format!(r#""{uh_dll_name}""#)),
        ("UH_IGVM_PATH", format!(r#""{uh_igvm_path}""#)),
        ("UH_VERSION", uh_version),
        ("UH_VERSION_STR", uh_version_str),
    ];

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-arg=/NOENTRY"); // resource DLL
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=resources.rc");
        embed_resource::compile("resources.rc", macros.map(|(k, v)| format!("{k}={v}")))
            .manifest_required()
            .unwrap();
    }
}
