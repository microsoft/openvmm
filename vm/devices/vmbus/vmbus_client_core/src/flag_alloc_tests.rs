// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[test]
fn allocate_starts_at_1() {
    let mut a = FlagAllocator::default();
    assert_eq!(a.allocate().unwrap(), 1);
    assert_eq!(a.allocate().unwrap(), 2);
}

#[test]
fn free_reuses_slot() {
    let mut a = FlagAllocator::default();
    let a1 = a.allocate().unwrap();
    let a2 = a.allocate().unwrap();
    a.free(a1);
    assert_eq!(a.allocate().unwrap(), a1);
    assert_ne!(a2, a1);
}

#[test]
fn reserve_specific_then_allocate_skips_reserved() {
    let mut a = FlagAllocator::default();
    a.reserve(5).unwrap();
    assert_eq!(a.allocate().unwrap(), 1);
    assert_eq!(a.allocate().unwrap(), 2);
    assert_eq!(a.allocate().unwrap(), 3);
    assert_eq!(a.allocate().unwrap(), 4);
    assert_eq!(a.allocate().unwrap(), 6);
}

#[test]
fn reserve_zero_is_invalid() {
    let mut a = FlagAllocator::default();
    assert_eq!(a.reserve(0), Err(FlagAllocError::InvalidFlag(0)));
}

#[test]
fn double_reserve_fails() {
    let mut a = FlagAllocator::default();
    a.reserve(3).unwrap();
    assert_eq!(a.reserve(3), Err(FlagAllocError::AlreadyInUse(3)));
}

#[test]
fn exhaust_flags() {
    let mut a = FlagAllocator::default();
    for _ in 0..FlagAllocator::MAX_FLAGS {
        a.allocate().unwrap();
    }
    assert_eq!(a.allocate(), Err(FlagAllocError::Exhausted));
}
