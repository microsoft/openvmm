// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test to verify backward compatibility

use build_info;

#[test]
fn test_backward_compatibility() {
    // Test that the existing API still works
    let build_info = build_info::get();
    
    // These are the methods used by underhill_core
    let _crate_name = build_info.crate_name();
    let _scm_revision = build_info.scm_revision();
    
    // Verify basic functionality
    assert_eq!(build_info.crate_name(), "build_info");
    assert!(!build_info.crate_name().is_empty());
    
    // Test new functionality works
    assert!(!build_info.build_profile().is_empty());
    let data = build_info.arbitrary_data();
    assert!(!data.is_empty());
}