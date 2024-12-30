// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for running in smaller Windows editions such as Win1.

#![cfg(windows)]
// UNSAFETY: needed to call internal Windows functions and to export unmangled
// functions.
#![expect(unsafe_code)]
#![warn(missing_docs)]

/// Links in crypto stubs to workaround rust stdlib's usage during hashmap
/// initialization. This is to avoid the bcrypt.dll and advapi32.dll
/// dependencies to allow binaries to run if a Windows SKU doesn't provide
/// these deps.
///
/// Usage:
/// ```
/// win_prng_support::use_win10_prng_apis!(bcrypt, advapi32);
/// ```
#[macro_export]
macro_rules! use_win10_prng_apis {
    () => {};
    ($($lib:ident),+ $(,)?) => {
        $($crate::use_win10_prng_apis!(@x $lib);)*
    };
    (@x advapi32) => {
        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn SystemFunction036(data: *mut u8, len: u32) -> u8 {
            // SAFETY: passing through guarantees.
            unsafe { $crate::private::SystemFunction036(data, len) }
        }

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        /// If a call to SystemFunction036 is marked as a dllimport, then it may be an indirect call
        /// through __imp_SystemFunction036 instead.
        pub static __imp_SystemFunction036: unsafe extern "system" fn(*mut u8, u32) -> u8 =
            SystemFunction036;
    };
    (@x bcrypt) => {
        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn BCryptOpenAlgorithmProvider(
            handle: *mut ::core::ffi::c_void,
            psz_alg_id: *mut u16,
            psz_implementation: *mut u16,
            flags: u32,
        ) -> u32 {
            // SAFETY: passing through guarantees.
            unsafe {
                $crate::private::BCryptOpenAlgorithmProvider(
                    handle,
                    psz_alg_id,
                    psz_implementation,
                    flags,
                )
            }
        }

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn BCryptCloseAlgorithmProvider(
            handle: *mut ::core::ffi::c_void,
            flags: u32,
        ) -> u32 {
            // SAFETY: passing through guarantees.
            unsafe { $crate::private::BCryptCloseAlgorithmProvider(handle, flags) }
        }

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn BCryptGenRandom(
            algorithm: usize,
            data: *mut u8,
            len: u32,
            flags: u32,
        ) -> u32 {
            // SAFETY: passing through guarantees.
            unsafe { $crate::private::BCryptGenRandom(algorithm, data, len, flags) }
        }

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        /// If a call to BCryptGenRandom is marked as a dllimport, then it may be an indirect call
        /// through __imp_BCryptGenRandom instead.
        pub static __imp_BCryptGenRandom: unsafe extern "system" fn(
            usize,
            *mut u8,
            u32,
            u32,
        ) -> u32 = BCryptGenRandom;

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub static __imp_BCryptOpenAlgorithmProvider: unsafe extern "system" fn(
            *mut ::core::ffi::c_void,
            *mut u16,
            *mut u16,
            u32,
        ) -> u32 = BCryptOpenAlgorithmProvider;

        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        pub static __imp_BCryptCloseAlgorithmProvider: unsafe extern "system" fn(
            *mut ::core::ffi::c_void,
            u32,
        ) -> u32 = BCryptCloseAlgorithmProvider;
    };
}

#[doc(hidden)]
pub mod private {
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use widestring::u16cstr;
    use widestring::U16CStr;

    const BCRYPT_RNG_USE_ENTROPY_IN_BUFFER: u32 = 1; // ignored in Win8+
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;
    const BCRYPT_MAGIC_ALGORITHM_HANDLE: usize = 0x1234abcd;

    #[link(name = "ext-ms-win-cng-rng-l1-1-0")]
    unsafe extern "C" {
        /// The lowest-level PRNG API in Windows.
        fn ProcessPrng(data: *mut u8, len: usize) -> u32;
    }

    /// Rust calls RtlGenRandom (also known as SystemFunction036) as part of hashmap
    /// initialization. Redirect this to the CRNG of our choice to avoid a dependency
    /// on advapi32.dll.
    ///
    /// # Safety
    ///
    /// - `data` must point to a buffer at least `len` bytes in size.
    pub unsafe extern "system" fn SystemFunction036(data: *mut u8, len: u32) -> u8 {
        // SAFETY: the caller guarantees that `data..data + len` is valid.
        if unsafe { ProcessPrng(data, len as usize) } == 0 {
            panic!("ProcessPrng failed");
        }
        1
    }

    /// Rust calls BCryptOpenAlgorithmProvider as part of hashmap initialization in Rust versions 1.65 onward.
    ///
    /// Implement this function by returning a magic handle value used in later calls to BCryptGenRandom to use the PRNG of
    /// our choice to avoid a dependency on bcrypt.dll.
    ///
    /// # Safety
    /// - `handle` must be a valid out pointer to a HANDLE (usize).
    /// - `psz_alg_id` must be a valid null terminated C string that contains `"RNG\0"`
    pub unsafe extern "system" fn BCryptOpenAlgorithmProvider(
        handle: *mut c_void,
        psz_alg_id: *mut u16,
        psz_implementation: *mut u16,
        flags: u32,
    ) -> u32 {
        if handle.is_null() || !psz_implementation.is_null() || flags != 0 {
            unimplemented!("unsupported values passed to BCryptOpenAlgorithmProvider")
        }

        // Validate that psz_alg_id matches what we expect.
        assert_eq!(
            // SAFETY: The caller guarantees that `psz_alg_id` is a null terminated string.
            unsafe { U16CStr::from_ptr_str(psz_alg_id) },
            u16cstr!("RNG"),
            "psz_alg_id must match"
        );

        let handle = handle.cast::<usize>();
        // SAFETY: The caller guarantees that handle is a valid pointer to store a handle.
        unsafe {
            *handle = BCRYPT_MAGIC_ALGORITHM_HANDLE;
        }
        0
    }

    /// Rust may call BCryptCloseAlgorithmProvider as part of hashmap initialization in Rust versions 1.65 onward.
    ///
    /// Stub this function out to avoid a dependency on bcrypt.dll.
    ///
    /// # Safety
    /// - `handle` must be the magic HvLite value.
    pub unsafe extern "system" fn BCryptCloseAlgorithmProvider(
        handle: *mut c_void,
        flags: u32,
    ) -> u32 {
        if handle as usize != BCRYPT_MAGIC_ALGORITHM_HANDLE || flags != 0 {
            unimplemented!("unsupported call to BCryptCloseAlgorithmProvider")
        }

        0
    }

    /// Rust calls BCryptGenRandom as part of hashmap initialization. Redirect this
    /// to the CRNG of our choice to avoid a dependency on bcrypt.dll.
    ///
    /// # Safety
    ///
    /// - `data` must point to a buffer at least `len` bytes in size.
    pub unsafe extern "system" fn BCryptGenRandom(
        algorithm: usize,
        data: *mut u8,
        len: u32,
        flags: u32,
    ) -> u32 {
        #[allow(clippy::if_same_then_else)]
        if algorithm == BCRYPT_MAGIC_ALGORITHM_HANDLE && flags == 0 {
            // Rust 1.65 calls BCryptGenRandom this way with the magic handle we return from our implementation of
            // BCryptOpenAlgorithmProvider.
        } else if algorithm == 0
            && (flags & !BCRYPT_RNG_USE_ENTROPY_IN_BUFFER == BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        {
            // Rust pre 1.65 calls BCryptGenRandom this way.
        } else {
            unimplemented!(
                "algorithm {:x} flags {:x} unsupported options passed to BCryptGenRandom",
                algorithm,
                flags,
            );
        }
        // SAFETY: the caller guarantees that `data..data + len` is valid.
        if unsafe { ProcessPrng(data, len as usize) } == 0 {
            panic!("ProcessPrng failed");
        }
        0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_prng() {
        crate::use_win10_prng_apis!(advapi32, bcrypt);
    }
}
