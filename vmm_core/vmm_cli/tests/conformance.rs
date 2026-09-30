// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conformance tests for the `KeyValueArgs` / `KeyValueGroup` derives.
//!
//! These pin the *uniform mechanics* the derive guarantees for every generated
//! parser — unknown/duplicate/empty/missing keys, the boolean flag spellings,
//! tri-state flags, defaults, positional heads, bracket lists, "one of" groups,
//! and struct flattening — so that individual `#[derive(KeyValueArgs)]` call
//! sites don't each have to re-prove them.

use vmm_cli::BracketRangeList;
use vmm_cli::KeyValueArgs;
use vmm_cli::KeyValueGroup;
use vmm_cli::MemorySize;

#[derive(KeyValueArgs, Debug, PartialEq)]
struct Basic {
    /// Required scalar.
    name: String,
    /// Optional scalar.
    count: Option<u32>,
    /// Absent => `Default::default()`.
    #[kv(default)]
    level: u8,
    /// Absent => the given expression.
    #[kv(default = 7)]
    retries: u8,
    /// Plain boolean flag.
    #[kv(flag)]
    fast: bool,
    /// Tri-state boolean flag.
    #[kv(flag)]
    verbose: Option<bool>,
    /// Renamed key + `FromStr` newtype value.
    #[kv(key = "sz")]
    size: Option<MemorySize>,
    /// Bracket list value.
    vps: Option<BracketRangeList>,
}

#[test]
fn basic_happy_path() {
    // Only the required field: everything else takes its default/None.
    let b: Basic = "name=foo".parse().unwrap();
    assert_eq!(
        b,
        Basic {
            name: "foo".into(),
            count: None,
            level: 0,
            retries: 7,
            fast: false,
            verbose: None,
            size: None,
            vps: None,
        }
    );

    // Everything set, order-independent.
    let b: Basic = "vps=[0,2-3],sz=2M,verbose=off,fast,retries=1,level=2,count=3,name=foo"
        .parse()
        .unwrap();
    assert_eq!(
        b,
        Basic {
            name: "foo".into(),
            count: Some(3),
            level: 2,
            retries: 1,
            fast: true,
            verbose: Some(false),
            size: Some(MemorySize(2 * 1024 * 1024)),
            vps: Some(BracketRangeList(vec![0..=0, 2..=3])),
        }
    );
}

#[test]
fn unknown_key_is_rejected() {
    let err = "name=foo,bogus=1".parse::<Basic>().unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown option 'bogus'; expected one of: 'name', 'count', 'level', 'retries', 'fast', 'verbose', 'sz', 'vps'"
    );
    // A bare unknown token is also rejected.
    assert!("name=foo,bogus".parse::<Basic>().is_err());
}

#[test]
fn malformed_options_are_rejected_clearly() {
    assert_eq!(
        "".parse::<Basic>().unwrap_err().to_string(),
        "missing required option 'name'"
    );
    assert_eq!(
        ",name=foo".parse::<Basic>().unwrap_err().to_string(),
        "empty option in ',name=foo'"
    );
    assert_eq!(
        "name=foo,".parse::<Basic>().unwrap_err().to_string(),
        "empty option in 'name=foo,'"
    );
    assert_eq!(
        "name=foo,,count=1"
            .parse::<Basic>()
            .unwrap_err()
            .to_string(),
        "empty option in 'name=foo,,count=1'"
    );
    assert_eq!(
        "=foo".parse::<Basic>().unwrap_err().to_string(),
        "option key cannot be empty"
    );
}

#[test]
fn duplicate_key_is_rejected() {
    assert!("name=foo,name=bar".parse::<Basic>().is_err());
    assert!("name=foo,count=1,count=2".parse::<Basic>().is_err());
}

#[test]
fn empty_value_is_rejected() {
    // An empty value for a scalar is treated as "no value".
    assert!("name=".parse::<Basic>().is_err());
    assert!("name=foo,count=".parse::<Basic>().is_err());
}

#[test]
fn missing_required_is_rejected() {
    assert!("count=3".parse::<Basic>().is_err());
}

#[test]
fn invalid_value_is_rejected() {
    assert!("name=foo,count=abc".parse::<Basic>().is_err());
    assert!("name=foo,sz=notasize".parse::<Basic>().is_err());
    assert!("name=foo,vps=0,1".parse::<Basic>().is_err()); // missing brackets
}

#[test]
fn flag_accepts_bare_on_off() {
    assert!(!"name=x".parse::<Basic>().unwrap().fast);
    assert!("name=x,fast".parse::<Basic>().unwrap().fast);
    assert!("name=x,fast=on".parse::<Basic>().unwrap().fast);
    assert!(!"name=x,fast=off".parse::<Basic>().unwrap().fast);
}

#[test]
fn flag_rejects_garbage_and_duplicates() {
    assert!("name=x,fast=yes".parse::<Basic>().is_err());
    assert!("name=x,fast,fast".parse::<Basic>().is_err());
    assert!("name=x,fast=on,fast=off".parse::<Basic>().is_err());
}

#[test]
fn tristate_flag_distinguishes_all_three() {
    assert_eq!("name=x".parse::<Basic>().unwrap().verbose, None);
    assert_eq!(
        "name=x,verbose".parse::<Basic>().unwrap().verbose,
        Some(true)
    );
    assert_eq!(
        "name=x,verbose=on".parse::<Basic>().unwrap().verbose,
        Some(true)
    );
    assert_eq!(
        "name=x,verbose=off".parse::<Basic>().unwrap().verbose,
        Some(false)
    );
}

#[derive(KeyValueArgs, Debug, PartialEq)]
struct Flagged {
    id: String,
    /// A flag that maps presence to a chosen value.
    #[kv(flag, present = 2u8, absent = 0u8)]
    level: u8,
}

#[test]
fn present_absent_flag_maps_values() {
    assert_eq!("id=a".parse::<Flagged>().unwrap().level, 0);
    assert_eq!("id=a,level".parse::<Flagged>().unwrap().level, 2);
    assert_eq!("id=a,level=on".parse::<Flagged>().unwrap().level, 2);
    assert_eq!("id=a,level=off".parse::<Flagged>().unwrap().level, 0);
}

#[derive(KeyValueArgs, Debug, PartialEq)]
struct Positional {
    #[kv(positional)]
    head: String,
    #[kv(flag)]
    flag: bool,
    opt: Option<u32>,
}

#[test]
fn positional_head() {
    let p: Positional = "myhead".parse().unwrap();
    assert_eq!(p.head, "myhead");
    assert!(!p.flag);
    assert_eq!(p.opt, None);

    let p: Positional = "myhead,flag,opt=3".parse().unwrap();
    assert_eq!(p.head, "myhead");
    assert!(p.flag);
    assert_eq!(p.opt, Some(3));

    // The head is taken verbatim (not split on '=').
    let p: Positional = "a:b=c,flag".parse().unwrap();
    assert_eq!(p.head, "a:b=c");

    // A missing/empty head is rejected.
    assert!("".parse::<Positional>().is_err());
    assert!(",flag".parse::<Positional>().is_err());
}

#[derive(KeyValueGroup, Debug, PartialEq)]
enum Transport {
    #[kv(key = "tcp")]
    Tcp(String),
    #[kv(key = "unix")]
    Unix(Option<String>),
    #[kv(key = "none")]
    None,
}

#[derive(KeyValueArgs, Debug, PartialEq)]
struct WithGroup {
    id: String,
    #[kv(flatten)]
    transport: Transport,
}

#[test]
fn group_selects_one_variant() {
    assert_eq!(
        "id=a,tcp=1.2.3.4".parse::<WithGroup>().unwrap().transport,
        Transport::Tcp("1.2.3.4".into())
    );
    // Optional-valued variant: bare and valued forms.
    assert_eq!(
        "id=a,unix".parse::<WithGroup>().unwrap().transport,
        Transport::Unix(None)
    );
    assert_eq!(
        "id=a,unix=/run/s".parse::<WithGroup>().unwrap().transport,
        Transport::Unix(Some("/run/s".into()))
    );
    assert!("id=a,unix=".parse::<WithGroup>().is_err());
    // Unit variant.
    assert_eq!(
        "id=a,none".parse::<WithGroup>().unwrap().transport,
        Transport::None
    );
}

#[test]
fn group_required_and_exclusive() {
    // Required: none of the group keys present.
    assert!("id=a".parse::<WithGroup>().is_err());
    // Mutually exclusive: two group keys present.
    assert!("id=a,tcp=x,none".parse::<WithGroup>().is_err());
    // Unknown key still errors even with a valid group key.
    let err = "id=a,tcp=x,bogus".parse::<WithGroup>().unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown option 'bogus'; expected one of: 'id', 'tcp', 'unix', 'none'"
    );
}

#[derive(KeyValueArgs, Debug, PartialEq)]
struct Shared {
    #[kv(flag)]
    a: bool,
    b: Option<u32>,
}

#[derive(KeyValueArgs, Debug, PartialEq)]
struct Outer {
    name: String,
    #[kv(flatten)]
    shared: Shared,
    c: Option<u32>,
}

#[test]
fn flatten_merges_struct_keys() {
    let o: Outer = "name=x,a,b=1,c=2".parse().unwrap();
    assert_eq!(
        o,
        Outer {
            name: "x".into(),
            shared: Shared {
                a: true,
                b: Some(1)
            },
            c: Some(2),
        }
    );

    // Absent flattened keys take their defaults.
    let o: Outer = "name=x".parse().unwrap();
    assert_eq!(o.shared, Shared { a: false, b: None });

    // Unknown keys still error at the top level.
    let err = "name=x,bogus=1".parse::<Outer>().unwrap_err();
    assert_eq!(
        err.to_string(),
        "unknown option 'bogus'; expected one of: 'name', 'a', 'b', 'c'"
    );
}
