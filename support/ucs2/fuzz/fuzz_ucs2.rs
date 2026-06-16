// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

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
    let bytes = s.as_bytes(); // includes trailing NUL
    let no_nul = s.as_bytes_without_nul(); // ensure this won't panic
    debug_assert!(bytes.len() >= no_nul.len() + 2);

    // ToOwned round-trip back to a Ucs2LeVec, then back to a slice.
    let round_tripped: Ucs2LeVec = s.to_owned();
    assert_eq!(round_tripped.as_ref(), s);

    // into_inner exposes the raw backing Vec.
    let raw = round_tripped.into_inner();
    assert!(raw.ends_with(&[0, 0]));
}

fuzz_target!(|input: InputKind| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
