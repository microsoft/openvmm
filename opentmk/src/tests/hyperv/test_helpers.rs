#[macro_export]
macro_rules! create_function_with_restore {
    ($func_name:ident, $symbol:ident) => {
        #[inline(never)]
        fn $func_name() {
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

#[cfg(target_arch = "x86_64")]
pub fn get_first_ip_len(target: u64) -> usize {
    // SAFETY:  if an invalid address is passed we dont return the len
    unsafe {
        use alloc::string::String;

        use iced_x86::DecoderOptions;
        use iced_x86::NasmFormatter;

        let target_ptr = target as *const u8;
        let code_bytes = core::slice::from_raw_parts(target_ptr, 0x100);
        let mut decoder = iced_x86::Decoder::with_ip(64, code_bytes, target, DecoderOptions::NONE);

        let mut formatter = NasmFormatter::new();
        let mut output = String::new();
        let mut first_ip_len = 0;
        let mut set = false;
        while decoder.can_decode() {
            use iced_x86::Formatter;

            let instr = decoder.decode();
            if !set {
                first_ip_len = instr.len();
                set = true;
            }
            formatter.format(&instr, &mut output);
            log::debug!("READ 0x{:x}:{}", instr.ip(), output);
            output.clear();
        }
        first_ip_len
    }
}
