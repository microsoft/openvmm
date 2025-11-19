// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Returns the number of the processor the current thread was running on during the call to this function.
pub fn get_cpu_number() -> u32 {
    // SAFETY: Function has no preconditions.
    unsafe { winapi::um::processthreadsapi::GetCurrentProcessorNumber() }
}
