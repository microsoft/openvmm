// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // TODO: loader shouldn't have any `cfg`s (or typedefs for that matter)!
    //
    // this is only here for expediency during the initial switch over to
    // `guest_arch`. A follow-up change should switch `loader` + the code it
    // depends on to a more sustainable model...
    build_rs_guest_arch::emit_guest_arch()
}
