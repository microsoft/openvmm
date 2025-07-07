// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test program to verify environment variables are captured

use build_info::BuildInfo;

fn main() {
    let build_info = BuildInfo::new();
    
    println!("Build Info:");
    println!("  Crate Name: {}", build_info.crate_name());
    println!("  Build Profile: {}", build_info.build_profile());
    println!("  Target Arch: {}", build_info.target_arch());
    println!("  Is Debug: {}", build_info.is_debug_build());
    println!("  Is Release: {}", build_info.is_release_build());
    
    println!("\nArbitrary Data:");
    for (key, value) in build_info.arbitrary_data() {
        println!("  {}: {}", key, value);
    }
    
    println!("\nSpecific lookups:");
    println!("  custom_1: {:?}", build_info.get_arbitrary_data("custom_1"));
    println!("  timestamp: {:?}", build_info.get_arbitrary_data("timestamp"));
    println!("  features: {:?}", build_info.get_arbitrary_data("features"));
    println!("  rust_version: {:?}", build_info.get_arbitrary_data("rust_version"));
}