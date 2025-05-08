// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Abstractions for memory-mapped device register(s) access.
//!
//! These lack an RMW implementation.

use core::marker::PhantomData;
use core::ops::Range;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

/// Trait to describe atomic access.
pub trait AtomicAccess<T: Copy> {
    /// Loads the data from the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn load(ptr: *mut T, order: Ordering) -> T;

    /// Stores the data at the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn store(ptr: *mut T, v: T, order: Ordering);

    /// Bitwise "or" with the current value.
    ///
    /// Performs a bitwise "or" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_or(ptr: *mut T, v: T, order: Ordering) -> T;

    /// Bitwise "and" with the current value.
    ///
    /// Performs a bitwise "and" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_and(ptr: *mut T, v: T, order: Ordering) -> T;
}

impl AtomicAccess<u64> for u64 {
    /// Loads the data from the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn load(ptr: *mut u64, order: Ordering) -> u64 {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU64::from_ptr(ptr).load(order) }
    }

    /// Stores the data at the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn store(ptr: *mut u64, v: u64, order: Ordering) {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU64::from_ptr(ptr).store(v, order) };
    }

    /// Bitwise "or" with the current value.
    ///
    /// Performs a bitwise "or" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_or(ptr: *mut u64, v: u64, order: Ordering) -> u64 {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU64::from_ptr(ptr).fetch_or(v, order) }
    }

    /// Bitwise "and" with the current value.
    ///
    /// Performs a bitwise "and" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_and(ptr: *mut u64, v: u64, order: Ordering) -> u64 {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU64::from_ptr(ptr).fetch_and(v, order) }
    }
}

impl AtomicAccess<u32> for u32 {
    /// Loads the data from the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn load(ptr: *mut u32, order: Ordering) -> u32 {
        // SAFETY: atomic access, the address is valid.

        unsafe { AtomicU32::from_ptr(ptr).load(order) }
    }

    /// Stores the data at the address with the spcifies ordering.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn store(ptr: *mut u32, v: u32, order: Ordering) {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU32::from_ptr(ptr).store(v, order) };
    }

    /// Bitwise "or" with the current value.
    ///
    /// Performs a bitwise "or" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_or(ptr: *mut u32, v: u32, order: Ordering) -> u32 {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU32::from_ptr(ptr).fetch_or(v, order) }
    }

    /// Bitwise "and" with the current value.
    ///
    /// Performs a bitwise "and" operation on the current value and the argument `v`, and
    /// sets the new value to the result.
    ///
    /// # Safety
    /// The address is valid.
    unsafe fn fetch_and(ptr: *mut u32, v: u32, order: Ordering) -> u32 {
        // SAFETY: atomic access, the address is valid.
        unsafe { AtomicU32::from_ptr(ptr).fetch_and(v, order) }
    }
}

/// Trait to describe the register access.
pub trait DeviceRegisterSpec {
    /// The raw type used for memory representation.
    type Raw: Copy + From<Self::Value> + AtomicAccess<Self::Raw>;
    /// The value type used in the API.
    type Value: Copy + From<Self::Raw>;
    /// The register offset from the base address.
    const OFFSET: usize;
    /// Mmeory ordering when loading, deafults to the
    /// sequential consistency.
    const ORDERING_LOAD: Ordering = Ordering::SeqCst;
    /// Mmeory ordering when loading, deafults to the
    /// sequential consistency.
    const ORDERING_STORE: Ordering = Ordering::SeqCst;
}

/// A memory-mapped device register.
pub struct DeviceRegister<S: DeviceRegisterSpec> {
    address: *mut S::Raw,
    _spec: PhantomData<S>,
}

impl<S: DeviceRegisterSpec> DeviceRegister<S> {
    /// Create a new MMIO register from a base address.
    ///
    /// Caller must ensure:
    /// * the base address is valid and properly aligned,
    /// * the resulting address (base + OFFSET) points to valid memory,
    /// * the memory has the required access permissions, caching and
    ///   attributes set.
    pub const fn new(base_address: usize) -> Self {
        Self {
            address: (base_address + S::OFFSET) as *mut S::Raw,
            _spec: PhantomData,
        }
    }

    /// Read the register value. Might be reorderd by the CPU,
    /// no compiler reordering.
    pub fn read(&self) -> S::Value {
        // SAFETY: volatile access ensures proper hardware interaction: no
        // accesses  will be elided or reordered by the compiler, and the
        // address comes from a trusted place.
        unsafe { core::ptr::read_volatile(self.address).into() }
    }

    /// Write a value to the register. Might be reorderd by the CPU,
    /// no compiler reordering.
    pub fn write(&mut self, value: S::Value) {
        // SAFETY: volatile access ensures proper hardware interaction: no
        // accesses  will be elided or reordered by the compiler, and the
        // address comes from a trusted place.
        unsafe { core::ptr::write_volatile(self.address, value.into()) };
    }

    /// Atomically load the register value using memory ordering
    /// from the specification.
    pub fn load(&self) -> S::Value {
        // SAFETY: atomic access provides a correct way to interact with the
        // hardware, and the address comes from the trusted source.
        unsafe { S::Raw::load(self.address, S::ORDERING_LOAD).into() }
    }

    /// Atoically store a value to the register using memory ordering
    /// from the specification.
    pub fn store(&mut self, value: S::Value) {
        // SAFETY: atomic access provides a correct way to interact with the
        // hardware, and the address comes from the trusted source.
        unsafe {
            S::Raw::store(self.address, value.into(), S::ORDERING_STORE);
        }
    }

    /// Atomically bitise "or" load the register value using memory ordering
    /// from the specification, and return the old value.
    pub fn fetch_or(&mut self, value: S::Value) -> S::Value {
        // SAFETY: atomic access provides a correct way to interact with the
        // hardware, and the address comes from the trusted source.
        unsafe { S::Raw::fetch_or(self.address, value.into(), S::ORDERING_LOAD).into() }
    }

    /// Atomically bitise "and" load the register value using memory ordering
    /// from the specification, and return the old value.
    pub fn fetch_and(&mut self, value: S::Value) -> S::Value {
        // SAFETY: atomic access provides a correct way to interact with the
        // hardware, and the address comes from the trusted source.
        unsafe { S::Raw::fetch_and(self.address, value.into(), S::ORDERING_LOAD).into() }
    }
}

/// Trait defining the specification for an array of device registers
pub trait DeviceRegisterArraySpec: DeviceRegisterSpec {
    /// The stride between consecutive registers in bytes
    const STRIDE: usize = 0;
    /// The number of registers in the array
    const COUNT: usize;
}

/// An array of memory-mapped device registers
pub struct DeviceRegisterArray<S: DeviceRegisterArraySpec> {
    base_address: usize,
    _spec: PhantomData<S>,
}

impl<S: DeviceRegisterArraySpec> DeviceRegisterArray<S> {
    /// Create a new array of MMIO registers from a base address.
    ///
    /// The user must ensure that the base address and the offset are valid,
    /// and that the memory is mapped as required for the device access.
    pub const fn new(base_address: usize) -> Self {
        Self {
            base_address,
            _spec: PhantomData,
        }
    }

    /// Get a reference to a specific register in the array.
    pub fn index(&self, index: usize) -> DeviceRegister<S> {
        assert!(index < S::COUNT, "Register index out of bounds");

        DeviceRegister::<S>::new(self.base_address + index * (S::STRIDE + size_of::<S::Raw>()))
    }

    /// Iterate over all registers in the array.
    pub fn iter(&self) -> impl Iterator<Item = DeviceRegister<S>> + '_ {
        (0..S::COUNT).map(move |i| self.index(i))
    }

    /// Fill the range with some value.
    pub fn fill(&mut self, range: Range<usize>, value: S::Value) {
        self.iter()
            .skip(range.start)
            .take(range.len())
            .for_each(|mut r| r.store(value));
    }
}
