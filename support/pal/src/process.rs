// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Process-related functionality.

/// Terminates the process immediately with the given exit code.
///
/// This is similar to [`std::process::exit`], but it skips calling any cleanup
/// functionality. This means stdout is not flushed, C++ destructors are not
/// called, and Windows DLL DllMain functions are not called.
pub fn terminate(exit_code: i32) -> ! {
    crate::sys::process::terminate(exit_code)
}
