// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use ucs2::Ucs2LeSlice;
use ucs2::Ucs2LeVec;
use xtask_fuzz::fuzz_target;

#[derive(Debug, Arbitrary)]
enum InputKind {
    String(String),
    Raw(Vec<u8>),
}

fn do_fuzz(input: InputKind) {
    // construct a new ucs2 string, testing both construction paths
    let s = match input {
        InputKind::String(s) => Ucs2LeVec::from(s),
        InputKind::Raw(v) => match Ucs2LeVec::from_vec_with_nul(v) {
            Ok(s) => s,
            Err(_) => return,
        },
    };
    let s: &Ucs2LeSlice = s.as_ref();

    // run some sanity checks on it
    let _s = format!("{}", s); // check display impl
    let _s = format!("{:?}", s); // check debug impl
    let _b = s.as_bytes_without_nul(); // ensure this won't panic
}

fuzz_target!(|input: InputKind| do_fuzz(input));
