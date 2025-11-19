// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[macro_export]
/// Generates a function that calls the given symbol saving and restoring general purpose registers around the call.
macro_rules! create_function_with_restore {
    ($func_name:ident, $symbol:ident) => {
        #[inline(never)]
        // avoiding inline for debuggability in release builds.
        fn $func_name() {
            // SAFETY: we are calling a function pointer and restoring general purpose registers.
            unsafe {
                asm!("
                    push rax
                    push rbx
                    push rcx
                    push rdx
                    push rsi
                    push rdi
                    push rbp
                    push r8
                    push r9
                    push r10
                    push r11
                    push r12
                    push r13
                    push r14
                    push r15
                    call {}
                    pop r15
                    pop r14
                    pop r13
                    pop r12
                    pop r11
                    pop r10
                    pop r9
                    pop r8
                    pop rbp
                    pop rdi
                    pop rsi
                    pop rdx
                    pop rcx
                    pop rbx
                    pop rax
                ", sym $symbol);
            }
        }
    };
}
