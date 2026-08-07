// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    build_rs_guest_arch::emit_guest_arch();
    // Ignore the error: a build from a release tarball or vendored sources has
    // no repository to read, and must still compile. `--version` then reports
    // the crate version alone.
    let _ = build_rs_git_info::emit_git_info();
}
