// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(dead_code)]
use spin::Mutex;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::structures::idt::InterruptStackFrame;
use x86_64::structures::idt::PageFaultErrorCode;
static mut COMMON_HANDLER: fn(InterruptStackFrame, u8) = common_handler;
static COMMON_HANDLER_MUTEX: Mutex<()> = Mutex::new(());

#[unsafe(no_mangle)]
fn abstraction_handle(stack_frame: InterruptStackFrame, interrupt: u8) {
    // SAFETY: COMMON_HANDLER is only set via set_common_handler which is protected by a mutex.
    unsafe { (COMMON_HANDLER)(stack_frame, interrupt) };
    log::debug!("Interrupt: {}", interrupt);
}

macro_rules! create_fn {
    ($name:ident, $i: expr) => {
        extern "x86-interrupt" fn $name(stack_frame: InterruptStackFrame) {
            abstraction_handle(stack_frame, $i);
        }
    };
}

macro_rules! create_fn_create_with_errorcode {
    ($name:ident, $i: expr) => {
        extern "x86-interrupt" fn $name(stack_frame: InterruptStackFrame, _error_code: u64) {
            abstraction_handle(stack_frame, $i);
        }
    };
}

macro_rules! create_fn_divergent_create_with_errorcode {
    ($name:ident, $i: expr) => {
        extern "x86-interrupt" fn $name(stack_frame: InterruptStackFrame, _error_code: u64) -> ! {
            abstraction_handle(stack_frame, $i);
            loop {}
        }
    };
}

macro_rules! create_fn_divergent_create {
    ($name:ident, $i: expr) => {
        extern "x86-interrupt" fn $name(stack_frame: InterruptStackFrame) -> ! {
            abstraction_handle(stack_frame, $i);
            loop {}
        }
    };
}

static mut BACKUP_RSP: u64 = 0;

macro_rules! create_page_fault_fn {
    ($name:ident, $i: expr) => {
        extern "x86-interrupt" fn $name(
            stack_frame: InterruptStackFrame,
            _error_code: PageFaultErrorCode,
        ) {
            abstraction_handle(stack_frame, $i);
        }
    };
}

macro_rules! register_interrupt_handler {
    ($idt: expr, $i: expr, $name: ident) => {
        $idt[$i].set_handler_fn($name);
    };
}

fn common_handler(_stack_frame: InterruptStackFrame, interrupt: u8) {
    log::info!("Default interrupt handler fired: {}", interrupt);
}

pub fn set_common_handler(handler: fn(InterruptStackFrame, u8)) {
    let _guard = COMMON_HANDLER_MUTEX.lock();
    // SAFETY: COMMON_HANDLER is only set via this function which is protected by a mutex.
    unsafe {
        COMMON_HANDLER = handler;
    }
}

extern "x86-interrupt" fn no_op(_stack_frame: InterruptStackFrame) {}

pub fn register_interrupt_handler(idt: &mut InterruptDescriptorTable) {
    idt.divide_error.set_handler_fn(handler_0);
    idt.debug.set_handler_fn(handler_1);
    idt.non_maskable_interrupt.set_handler_fn(handler_2);
    idt.breakpoint.set_handler_fn(handler_3);
    idt.overflow.set_handler_fn(handler_4);
    idt.bound_range_exceeded.set_handler_fn(handler_5);
    idt.invalid_opcode.set_handler_fn(handler_6);
    idt.device_not_available.set_handler_fn(handler_7);
    idt.double_fault.set_handler_fn(handler_8);
    register_interrupt_handler!(idt, 9, handler_9);
    idt.invalid_tss.set_handler_fn(handler_10);
    idt.segment_not_present.set_handler_fn(handler_11);
    idt.stack_segment_fault.set_handler_fn(handler_12);
    idt.general_protection_fault.set_handler_fn(handler_13);
    idt.page_fault.set_handler_fn(handler_14);
    // Vector 15 is reserved
    idt.x87_floating_point.set_handler_fn(handler_16);
    idt.alignment_check.set_handler_fn(handler_17);
    idt.machine_check.set_handler_fn(handler_18);
    idt.simd_floating_point.set_handler_fn(handler_19);
    idt.virtualization.set_handler_fn(handler_20);
    idt.cp_protection_exception.set_handler_fn(handler_21);
    // Vector 22-27 is reserved
    idt.hv_injection_exception.set_handler_fn(handler_28);
    idt.vmm_communication_exception.set_handler_fn(handler_29);
    idt.security_exception.set_handler_fn(handler_30);
    // Vector 31 is reserved

    register_interrupt_handler!(idt, 32, handler_32);
    register_interrupt_handler!(idt, 33, handler_33);
    register_interrupt_handler!(idt, 34, handler_34);
    register_interrupt_handler!(idt, 35, handler_35);
    register_interrupt_handler!(idt, 36, handler_36);
    register_interrupt_handler!(idt, 37, handler_37);
    register_interrupt_handler!(idt, 38, handler_38);
    register_interrupt_handler!(idt, 39, handler_39);
    register_interrupt_handler!(idt, 40, handler_40);
    register_interrupt_handler!(idt, 41, handler_41);
    register_interrupt_handler!(idt, 42, handler_42);
    register_interrupt_handler!(idt, 43, handler_43);
    register_interrupt_handler!(idt, 44, handler_44);
    register_interrupt_handler!(idt, 45, handler_45);
    register_interrupt_handler!(idt, 46, handler_46);
    register_interrupt_handler!(idt, 47, handler_47);
    register_interrupt_handler!(idt, 48, handler_48);
    register_interrupt_handler!(idt, 49, handler_49);
    register_interrupt_handler!(idt, 50, handler_50);
    register_interrupt_handler!(idt, 51, handler_51);
    register_interrupt_handler!(idt, 52, handler_52);
    register_interrupt_handler!(idt, 53, handler_53);
    register_interrupt_handler!(idt, 54, handler_54);
    register_interrupt_handler!(idt, 55, handler_55);
    register_interrupt_handler!(idt, 56, handler_56);
    register_interrupt_handler!(idt, 57, handler_57);
    register_interrupt_handler!(idt, 58, handler_58);
    register_interrupt_handler!(idt, 59, handler_59);
    register_interrupt_handler!(idt, 60, handler_60);
    register_interrupt_handler!(idt, 61, handler_61);
    register_interrupt_handler!(idt, 62, handler_62);
    register_interrupt_handler!(idt, 63, handler_63);
    register_interrupt_handler!(idt, 64, handler_64);
    register_interrupt_handler!(idt, 65, handler_65);
    register_interrupt_handler!(idt, 66, handler_66);
    register_interrupt_handler!(idt, 67, handler_67);
    register_interrupt_handler!(idt, 68, handler_68);
    register_interrupt_handler!(idt, 69, handler_69);
    register_interrupt_handler!(idt, 70, handler_70);
    register_interrupt_handler!(idt, 71, handler_71);
    register_interrupt_handler!(idt, 72, handler_72);
    register_interrupt_handler!(idt, 73, handler_73);
    register_interrupt_handler!(idt, 74, handler_74);
    register_interrupt_handler!(idt, 75, handler_75);
    register_interrupt_handler!(idt, 76, handler_76);
    register_interrupt_handler!(idt, 77, handler_77);
    register_interrupt_handler!(idt, 78, handler_78);
    register_interrupt_handler!(idt, 79, handler_79);
    register_interrupt_handler!(idt, 80, handler_80);
    register_interrupt_handler!(idt, 81, handler_81);
    register_interrupt_handler!(idt, 82, handler_82);
    register_interrupt_handler!(idt, 83, handler_83);
    register_interrupt_handler!(idt, 84, handler_84);
    register_interrupt_handler!(idt, 85, handler_85);
    register_interrupt_handler!(idt, 86, handler_86);
    register_interrupt_handler!(idt, 87, handler_87);
    register_interrupt_handler!(idt, 88, handler_88);
    register_interrupt_handler!(idt, 89, handler_89);
    register_interrupt_handler!(idt, 90, handler_90);
    register_interrupt_handler!(idt, 91, handler_91);
    register_interrupt_handler!(idt, 92, handler_92);
    register_interrupt_handler!(idt, 93, handler_93);
    register_interrupt_handler!(idt, 94, handler_94);
    register_interrupt_handler!(idt, 95, handler_95);
    register_interrupt_handler!(idt, 96, handler_96);
    register_interrupt_handler!(idt, 97, handler_97);
    register_interrupt_handler!(idt, 98, handler_98);
    register_interrupt_handler!(idt, 99, handler_99);
    register_interrupt_handler!(idt, 100, handler_100);
    register_interrupt_handler!(idt, 101, handler_101);
    register_interrupt_handler!(idt, 102, handler_102);
    register_interrupt_handler!(idt, 103, handler_103);
    register_interrupt_handler!(idt, 104, handler_104);
    register_interrupt_handler!(idt, 105, handler_105);
    register_interrupt_handler!(idt, 106, handler_106);
    register_interrupt_handler!(idt, 107, handler_107);
    register_interrupt_handler!(idt, 108, handler_108);
    register_interrupt_handler!(idt, 109, handler_109);
    register_interrupt_handler!(idt, 110, handler_110);
    register_interrupt_handler!(idt, 111, handler_111);
    register_interrupt_handler!(idt, 112, handler_112);
    register_interrupt_handler!(idt, 113, handler_113);
    register_interrupt_handler!(idt, 114, handler_114);
    register_interrupt_handler!(idt, 115, handler_115);
    register_interrupt_handler!(idt, 116, handler_116);
    register_interrupt_handler!(idt, 117, handler_117);
    register_interrupt_handler!(idt, 118, handler_118);
    register_interrupt_handler!(idt, 119, handler_119);
    register_interrupt_handler!(idt, 120, handler_120);
    register_interrupt_handler!(idt, 121, handler_121);
    register_interrupt_handler!(idt, 122, handler_122);
    register_interrupt_handler!(idt, 123, handler_123);
    register_interrupt_handler!(idt, 124, handler_124);
    register_interrupt_handler!(idt, 125, handler_125);
    register_interrupt_handler!(idt, 126, handler_126);
    register_interrupt_handler!(idt, 127, handler_127);
    register_interrupt_handler!(idt, 128, handler_128);
    register_interrupt_handler!(idt, 129, handler_129);
    register_interrupt_handler!(idt, 130, handler_130);
    register_interrupt_handler!(idt, 131, handler_131);
    register_interrupt_handler!(idt, 132, handler_132);
    register_interrupt_handler!(idt, 133, handler_133);
    register_interrupt_handler!(idt, 134, handler_134);
    register_interrupt_handler!(idt, 135, handler_135);
    register_interrupt_handler!(idt, 136, handler_136);
    register_interrupt_handler!(idt, 137, handler_137);
    register_interrupt_handler!(idt, 138, handler_138);
    register_interrupt_handler!(idt, 139, handler_139);
    register_interrupt_handler!(idt, 140, handler_140);
    register_interrupt_handler!(idt, 141, handler_141);
    register_interrupt_handler!(idt, 142, handler_142);
    register_interrupt_handler!(idt, 143, handler_143);
    register_interrupt_handler!(idt, 144, handler_144);
    register_interrupt_handler!(idt, 145, handler_145);
    register_interrupt_handler!(idt, 146, handler_146);
    register_interrupt_handler!(idt, 147, handler_147);
    register_interrupt_handler!(idt, 148, handler_148);
    register_interrupt_handler!(idt, 149, handler_149);
    register_interrupt_handler!(idt, 150, handler_150);
    register_interrupt_handler!(idt, 151, handler_151);
    register_interrupt_handler!(idt, 152, handler_152);
    register_interrupt_handler!(idt, 153, handler_153);
    register_interrupt_handler!(idt, 154, handler_154);
    register_interrupt_handler!(idt, 155, handler_155);
    register_interrupt_handler!(idt, 156, handler_156);
    register_interrupt_handler!(idt, 157, handler_157);
    register_interrupt_handler!(idt, 158, handler_158);
    register_interrupt_handler!(idt, 159, handler_159);
    register_interrupt_handler!(idt, 160, handler_160);
    register_interrupt_handler!(idt, 161, handler_161);
    register_interrupt_handler!(idt, 162, handler_162);
    register_interrupt_handler!(idt, 163, handler_163);
    register_interrupt_handler!(idt, 164, handler_164);
    register_interrupt_handler!(idt, 165, handler_165);
    register_interrupt_handler!(idt, 166, handler_166);
    register_interrupt_handler!(idt, 167, handler_167);
    register_interrupt_handler!(idt, 168, handler_168);
    register_interrupt_handler!(idt, 169, handler_169);
    register_interrupt_handler!(idt, 170, handler_170);
    register_interrupt_handler!(idt, 171, handler_171);
    register_interrupt_handler!(idt, 172, handler_172);
    register_interrupt_handler!(idt, 173, handler_173);
    register_interrupt_handler!(idt, 174, handler_174);
    register_interrupt_handler!(idt, 175, handler_175);
    register_interrupt_handler!(idt, 176, handler_176);
    register_interrupt_handler!(idt, 177, handler_177);
    register_interrupt_handler!(idt, 178, handler_178);
    register_interrupt_handler!(idt, 179, handler_179);
    register_interrupt_handler!(idt, 180, handler_180);
    register_interrupt_handler!(idt, 181, handler_181);
    register_interrupt_handler!(idt, 182, handler_182);
    register_interrupt_handler!(idt, 183, handler_183);
    register_interrupt_handler!(idt, 184, handler_184);
    register_interrupt_handler!(idt, 185, handler_185);
    register_interrupt_handler!(idt, 186, handler_186);
    register_interrupt_handler!(idt, 187, handler_187);
    register_interrupt_handler!(idt, 188, handler_188);
    register_interrupt_handler!(idt, 189, handler_189);
    register_interrupt_handler!(idt, 190, handler_190);
    register_interrupt_handler!(idt, 191, handler_191);
    register_interrupt_handler!(idt, 192, handler_192);
    register_interrupt_handler!(idt, 193, handler_193);
    register_interrupt_handler!(idt, 194, handler_194);
    register_interrupt_handler!(idt, 195, handler_195);
    register_interrupt_handler!(idt, 196, handler_196);
    register_interrupt_handler!(idt, 197, handler_197);
    register_interrupt_handler!(idt, 198, handler_198);
    register_interrupt_handler!(idt, 199, handler_199);
    register_interrupt_handler!(idt, 200, handler_200);
    register_interrupt_handler!(idt, 201, handler_201);
    register_interrupt_handler!(idt, 202, handler_202);
    register_interrupt_handler!(idt, 203, handler_203);
    register_interrupt_handler!(idt, 204, handler_204);
    register_interrupt_handler!(idt, 205, handler_205);
    register_interrupt_handler!(idt, 206, handler_206);
    register_interrupt_handler!(idt, 207, handler_207);
    register_interrupt_handler!(idt, 208, handler_208);
    register_interrupt_handler!(idt, 209, handler_209);
    register_interrupt_handler!(idt, 210, handler_210);
    register_interrupt_handler!(idt, 211, handler_211);
    register_interrupt_handler!(idt, 212, handler_212);
    register_interrupt_handler!(idt, 213, handler_213);
    register_interrupt_handler!(idt, 214, handler_214);
    register_interrupt_handler!(idt, 215, handler_215);
    register_interrupt_handler!(idt, 216, handler_216);
    register_interrupt_handler!(idt, 217, handler_217);
    register_interrupt_handler!(idt, 218, handler_218);
    register_interrupt_handler!(idt, 219, handler_219);
    register_interrupt_handler!(idt, 220, handler_220);
    register_interrupt_handler!(idt, 221, handler_221);
    register_interrupt_handler!(idt, 222, handler_222);
    register_interrupt_handler!(idt, 223, handler_223);
    register_interrupt_handler!(idt, 224, handler_224);
    register_interrupt_handler!(idt, 225, handler_225);
    register_interrupt_handler!(idt, 226, handler_226);
    register_interrupt_handler!(idt, 227, handler_227);
    register_interrupt_handler!(idt, 228, handler_228);
    register_interrupt_handler!(idt, 229, handler_229);
    register_interrupt_handler!(idt, 230, handler_230);
    register_interrupt_handler!(idt, 231, handler_231);
    register_interrupt_handler!(idt, 232, handler_232);
    register_interrupt_handler!(idt, 233, handler_233);
    register_interrupt_handler!(idt, 234, handler_234);
    register_interrupt_handler!(idt, 235, handler_235);
    register_interrupt_handler!(idt, 236, handler_236);
    register_interrupt_handler!(idt, 237, handler_237);
    register_interrupt_handler!(idt, 238, handler_238);
    register_interrupt_handler!(idt, 239, handler_239);
    register_interrupt_handler!(idt, 240, handler_240);
    register_interrupt_handler!(idt, 241, handler_241);
    register_interrupt_handler!(idt, 242, handler_242);
    register_interrupt_handler!(idt, 243, handler_243);
    register_interrupt_handler!(idt, 244, handler_244);
    register_interrupt_handler!(idt, 245, handler_245);
    register_interrupt_handler!(idt, 246, handler_246);
    register_interrupt_handler!(idt, 247, handler_247);
    register_interrupt_handler!(idt, 248, handler_248);
    register_interrupt_handler!(idt, 249, handler_249);
    register_interrupt_handler!(idt, 250, handler_250);
    register_interrupt_handler!(idt, 251, handler_251);
    register_interrupt_handler!(idt, 252, handler_252);
    register_interrupt_handler!(idt, 253, handler_253);
    register_interrupt_handler!(idt, 254, handler_254);
    register_interrupt_handler!(idt, 255, handler_255);
}

create_fn!(handler_0, 0);
create_fn!(handler_1, 1);
create_fn!(handler_2, 2);
create_fn!(handler_3, 3);
create_fn!(handler_4, 4);
create_fn!(handler_5, 5);
create_fn!(handler_6, 6);
create_fn!(handler_7, 7);
create_fn_divergent_create_with_errorcode!(handler_8, 8);
create_fn!(handler_9, 9);
create_fn_create_with_errorcode!(handler_10, 10);
create_fn_create_with_errorcode!(handler_11, 11);
create_fn_create_with_errorcode!(handler_12, 12);
create_fn_create_with_errorcode!(handler_13, 13);
create_page_fault_fn!(handler_14, 14);
create_fn!(handler_15, 15);
create_fn!(handler_16, 16);
create_fn_create_with_errorcode!(handler_17, 17);
create_fn_divergent_create!(handler_18, 18);
create_fn!(handler_19, 19);
create_fn!(handler_20, 20);
create_fn_create_with_errorcode!(handler_21, 21);
create_fn!(handler_22, 22);
create_fn!(handler_23, 23);
create_fn!(handler_24, 24);
create_fn!(handler_25, 25);
create_fn!(handler_26, 26);
create_fn!(handler_27, 27);
create_fn!(handler_28, 28);
create_fn_create_with_errorcode!(handler_29, 29);
create_fn_create_with_errorcode!(handler_30, 30);
create_fn!(handler_31, 31);
create_fn!(handler_32, 32);
create_fn!(handler_33, 33);
create_fn!(handler_34, 34);
create_fn!(handler_35, 35);
create_fn!(handler_36, 36);
create_fn!(handler_37, 37);
create_fn!(handler_38, 38);
create_fn!(handler_39, 39);
create_fn!(handler_40, 40);
create_fn!(handler_41, 41);
create_fn!(handler_42, 42);
create_fn!(handler_43, 43);
create_fn!(handler_44, 44);
create_fn!(handler_45, 45);
create_fn!(handler_46, 46);
create_fn!(handler_47, 47);
create_fn!(handler_48, 48);
create_fn!(handler_49, 49);
create_fn!(handler_50, 50);
create_fn!(handler_51, 51);
create_fn!(handler_52, 52);
create_fn!(handler_53, 53);
create_fn!(handler_54, 54);
create_fn!(handler_55, 55);
create_fn!(handler_56, 56);
create_fn!(handler_57, 57);
create_fn!(handler_58, 58);
create_fn!(handler_59, 59);
create_fn!(handler_60, 60);
create_fn!(handler_61, 61);
create_fn!(handler_62, 62);
create_fn!(handler_63, 63);
create_fn!(handler_64, 64);
create_fn!(handler_65, 65);
create_fn!(handler_66, 66);
create_fn!(handler_67, 67);
create_fn!(handler_68, 68);
create_fn!(handler_69, 69);
create_fn!(handler_70, 70);
create_fn!(handler_71, 71);
create_fn!(handler_72, 72);
create_fn!(handler_73, 73);
create_fn!(handler_74, 74);
create_fn!(handler_75, 75);
create_fn!(handler_76, 76);
create_fn!(handler_77, 77);
create_fn!(handler_78, 78);
create_fn!(handler_79, 79);
create_fn!(handler_80, 80);
create_fn!(handler_81, 81);
create_fn!(handler_82, 82);
create_fn!(handler_83, 83);
create_fn!(handler_84, 84);
create_fn!(handler_85, 85);
create_fn!(handler_86, 86);
create_fn!(handler_87, 87);
create_fn!(handler_88, 88);
create_fn!(handler_89, 89);
create_fn!(handler_90, 90);
create_fn!(handler_91, 91);
create_fn!(handler_92, 92);
create_fn!(handler_93, 93);
create_fn!(handler_94, 94);
create_fn!(handler_95, 95);
create_fn!(handler_96, 96);
create_fn!(handler_97, 97);
create_fn!(handler_98, 98);
create_fn!(handler_99, 99);
create_fn!(handler_100, 100);
create_fn!(handler_101, 101);
create_fn!(handler_102, 102);
create_fn!(handler_103, 103);
create_fn!(handler_104, 104);
create_fn!(handler_105, 105);
create_fn!(handler_106, 106);
create_fn!(handler_107, 107);
create_fn!(handler_108, 108);
create_fn!(handler_109, 109);
create_fn!(handler_110, 110);
create_fn!(handler_111, 111);
create_fn!(handler_112, 112);
create_fn!(handler_113, 113);
create_fn!(handler_114, 114);
create_fn!(handler_115, 115);
create_fn!(handler_116, 116);
create_fn!(handler_117, 117);
create_fn!(handler_118, 118);
create_fn!(handler_119, 119);
create_fn!(handler_120, 120);
create_fn!(handler_121, 121);
create_fn!(handler_122, 122);
create_fn!(handler_123, 123);
create_fn!(handler_124, 124);
create_fn!(handler_125, 125);
create_fn!(handler_126, 126);
create_fn!(handler_127, 127);
create_fn!(handler_128, 128);
create_fn!(handler_129, 129);
create_fn!(handler_130, 130);
create_fn!(handler_131, 131);
create_fn!(handler_132, 132);
create_fn!(handler_133, 133);
create_fn!(handler_134, 134);
create_fn!(handler_135, 135);
create_fn!(handler_136, 136);
create_fn!(handler_137, 137);
create_fn!(handler_138, 138);
create_fn!(handler_139, 139);
create_fn!(handler_140, 140);
create_fn!(handler_141, 141);
create_fn!(handler_142, 142);
create_fn!(handler_143, 143);
create_fn!(handler_144, 144);
create_fn!(handler_145, 145);
create_fn!(handler_146, 146);
create_fn!(handler_147, 147);
create_fn!(handler_148, 148);
create_fn!(handler_149, 149);
create_fn!(handler_150, 150);
create_fn!(handler_151, 151);
create_fn!(handler_152, 152);
create_fn!(handler_153, 153);
create_fn!(handler_154, 154);
create_fn!(handler_155, 155);
create_fn!(handler_156, 156);
create_fn!(handler_157, 157);
create_fn!(handler_158, 158);
create_fn!(handler_159, 159);
create_fn!(handler_160, 160);
create_fn!(handler_161, 161);
create_fn!(handler_162, 162);
create_fn!(handler_163, 163);
create_fn!(handler_164, 164);
create_fn!(handler_165, 165);
create_fn!(handler_166, 166);
create_fn!(handler_167, 167);
create_fn!(handler_168, 168);
create_fn!(handler_169, 169);
create_fn!(handler_170, 170);
create_fn!(handler_171, 171);
create_fn!(handler_172, 172);
create_fn!(handler_173, 173);
create_fn!(handler_174, 174);
create_fn!(handler_175, 175);
create_fn!(handler_176, 176);
create_fn!(handler_177, 177);
create_fn!(handler_178, 178);
create_fn!(handler_179, 179);
create_fn!(handler_180, 180);
create_fn!(handler_181, 181);
create_fn!(handler_182, 182);
create_fn!(handler_183, 183);
create_fn!(handler_184, 184);
create_fn!(handler_185, 185);
create_fn!(handler_186, 186);
create_fn!(handler_187, 187);
create_fn!(handler_188, 188);
create_fn!(handler_189, 189);
create_fn!(handler_190, 190);
create_fn!(handler_191, 191);
create_fn!(handler_192, 192);
create_fn!(handler_193, 193);
create_fn!(handler_194, 194);
create_fn!(handler_195, 195);
create_fn!(handler_196, 196);
create_fn!(handler_197, 197);
create_fn!(handler_198, 198);
create_fn!(handler_199, 199);
create_fn!(handler_200, 200);
create_fn!(handler_201, 201);
create_fn!(handler_202, 202);
create_fn!(handler_203, 203);
create_fn!(handler_204, 204);
create_fn!(handler_205, 205);
create_fn!(handler_206, 206);
create_fn!(handler_207, 207);
create_fn!(handler_208, 208);
create_fn!(handler_209, 209);
create_fn!(handler_210, 210);
create_fn!(handler_211, 211);
create_fn!(handler_212, 212);
create_fn!(handler_213, 213);
create_fn!(handler_214, 214);
create_fn!(handler_215, 215);
create_fn!(handler_216, 216);
create_fn!(handler_217, 217);
create_fn!(handler_218, 218);
create_fn!(handler_219, 219);
create_fn!(handler_220, 220);
create_fn!(handler_221, 221);
create_fn!(handler_222, 222);
create_fn!(handler_223, 223);
create_fn!(handler_224, 224);
create_fn!(handler_225, 225);
create_fn!(handler_226, 226);
create_fn!(handler_227, 227);
create_fn!(handler_228, 228);
create_fn!(handler_229, 229);
create_fn!(handler_230, 230);
create_fn!(handler_231, 231);
create_fn!(handler_232, 232);
create_fn!(handler_233, 233);
create_fn!(handler_234, 234);
create_fn!(handler_235, 235);
create_fn!(handler_236, 236);
create_fn!(handler_237, 237);
create_fn!(handler_238, 238);
create_fn!(handler_239, 239);
create_fn!(handler_240, 240);
create_fn!(handler_241, 241);
create_fn!(handler_242, 242);
create_fn!(handler_243, 243);
create_fn!(handler_244, 244);
create_fn!(handler_245, 245);
create_fn!(handler_246, 246);
create_fn!(handler_247, 247);
create_fn!(handler_248, 248);
create_fn!(handler_249, 249);
create_fn!(handler_250, 250);
create_fn!(handler_251, 251);
create_fn!(handler_252, 252);
create_fn!(handler_253, 253);
create_fn!(handler_254, 254);
create_fn!(handler_255, 255);
