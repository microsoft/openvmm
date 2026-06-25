// SPDX-License-Identifier: GPL-2.0
/*
 * hvpost_hook: force KVM (L0) to handle the nested L2 guest's
 * HvPostMessage(conn 1) VMCALL instead of reflecting it to L1 (hvix64).
 *
 * Mechanism: ftrace hook (FL_SAVE_REGS | FL_IPMODIFY) on
 * nested_vmx_reflect_vmexit(struct kvm_vcpu *vcpu) in kvm_intel.
 * When the current L2 exit is VMCALL (basic exit reason 18) AND the
 * guest RCX low 16 bits == 0x5c (HvPostMessage call code), we make the
 * function return false WITHOUT running its body, by:
 *   - setting regs->ax = 0  (return value = false = "L0 handles it")
 *   - redirecting regs->ip to a bare `ret` stub. At the __fentry__ hook
 *     site the original `call nested_vmx_reflect_vmexit` return address
 *     is on top of stack, so the stub's `ret` returns straight to the
 *     caller (__vmx_handle_exit) with rax=0. This is the canonical x86
 *     override-function-with-return pattern.
 * For every other exit we do nothing and let the original body run.
 *
 * Verified offsets for kernel 7.0.2-7 x86_64 (version-specific, re-derive for yours):
 *   RCX             = vcpu->arch.regs[VCPU_REGS_RCX=1]
 *                   = off(arch)=304 + 1*8 = 312 (0x138)
 *                     (pahole kvm_vcpu: arch @304; kvm_vcpu_arch: regs[] @0)
 *   exit_reason.full= vcpu_vmx.vt(@6848) + vcpu_vt.exit_reason(@80) = 6928 (0x1B10)
 *                     (confirmed in disasm: `mov 0x1b10(%rdi),%r12d`)
 *                     vcpu_vmx.vcpu @0, so vmx ptr == vcpu ptr.
 *   basic exit reason = low 16 bits of exit_reason.full
 *   EXIT_REASON_VMCALL = 18
 *   HvPostMessage call code = 0x5c (low 16 bits of guest RCX control word)
 */
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/ftrace.h>
#include <linux/kprobes.h>
#include <linux/ptrace.h>
#include <linux/sched.h>
#include <linux/pid.h>
#include <linux/ratelimit.h>
#include <linux/objtool.h>
#include <asm/linkage.h>

#define OFF_VCPU_RCX        0x138   /* vcpu->arch.regs[VCPU_REGS_RCX] */
#define OFF_VMX_EXITREASON  0x1B10  /* vcpu_vmx.vt.exit_reason.full   */
#define EXIT_REASON_VMCALL  18
#define HV_POST_MESSAGE     0x5c
#define HV_SIGNAL_EVENT     0x5d

static unsigned long reflect_addr;
static int relay_pid;
module_param(relay_pid, int, 0644);
MODULE_PARM_DESC(relay_pid, "Only relay for the VM whose owning userspace PID matches (0=off, intercept nothing)");

/*
 * Return stub the IP is redirected to. At the __fentry__ hook site the
 * original `call nested_vmx_reflect_vmexit` return address is on top of
 * stack, so a single return here pops it and returns to the caller. We set
 * regs->ax beforehand, so this body only needs to return.
 *
 * Hand-written naked asm (not a C function) so there is NO __fentry__ call
 * and NO frame setup at the entry we jump to: the very first byte is the
 * return. It uses ASM_RET, which expands to `jmp __x86_return_thunk` under
 * CONFIG_MITIGATION_RETHUNK, so it is mitigation-correct and objtool-clean.
 * STACK_FRAME_NON_STANDARD tells objtool this is intentional.
 */
extern void just_return_stub(void);
asm(
	".pushsection .text\n"
	".align 16\n"
	".type just_return_stub, @function\n"
	"just_return_stub:\n"
	ASM_RET
	".size just_return_stub, .-just_return_stub\n"
	".popsection\n"
);
STACK_FRAME_NON_STANDARD(just_return_stub);

/* resolve a non-exported symbol via kprobe (kallsyms_lookup_name is no
 * longer exported on this kernel). */
static unsigned long lookup_name(const char *name)
{
	struct kprobe kp = { .symbol_name = name };
	unsigned long addr = 0;
	if (register_kprobe(&kp) == 0) {
		addr = (unsigned long)kp.addr;
		unregister_kprobe(&kp);
	}
	return addr;
}

static void notrace hvpost_handler(unsigned long ip, unsigned long parent_ip,
				   struct ftrace_ops *op, struct ftrace_regs *fregs)
{
	struct pt_regs *regs = arch_ftrace_get_regs(fregs);
	unsigned long vcpu;
	u32 exit_full;
	u16 basic;
	u64 rcx;

	if (!regs)
		return;                 /* SAVE_REGS not honored: do nothing */

	vcpu = regs->di;                /* first arg = struct kvm_vcpu * */
	if (!vcpu)
		return;

	exit_full = *(u32 *)(vcpu + OFF_VMX_EXITREASON);
	basic = (u16)(exit_full & 0xffff);
	if (basic != EXIT_REASON_VMCALL)
		return;                 /* not a VMCALL exit: let original run */

	rcx = *(u64 *)(vcpu + OFF_VCPU_RCX);
	if ((rcx & 0xffff) != HV_POST_MESSAGE && (rcx & 0xffff) != HV_SIGNAL_EVENT)
		return;                 /* not HvPostMessage: let original run */

	/* Force "return false": L0 keeps the exit, does not reflect to L1. */
	if (!relay_pid || current->tgid != relay_pid)
		return;
	*(u64 *)(vcpu + OFF_VCPU_RCX) = rcx & ~0x80000000ULL; /* strip nested bit(31) so KVM accepts the hypercall */
	regs->ax = 0;
	ftrace_regs_set_instruction_pointer(fregs, (unsigned long)just_return_stub);

	{
		static DEFINE_RATELIMIT_STATE(rs, HZ, 5);
		if (__ratelimit(&rs))
			pr_info("hvpost_hook: caught HvPostMessage VMCALL (rcx=0x%llx), forcing L0 handling\n",
				rcx);
	}
}

static struct ftrace_ops ops = {
	.func  = hvpost_handler,
	.flags = FTRACE_OPS_FL_SAVE_REGS | FTRACE_OPS_FL_IPMODIFY |
		 FTRACE_OPS_FL_RECURSION,
};

static int __init hvpost_init(void)
{
	int ret;

	reflect_addr = lookup_name("nested_vmx_reflect_vmexit");
	if (!reflect_addr) {
		pr_err("hvpost_hook: cannot resolve nested_vmx_reflect_vmexit\n");
		return -ENOENT;
	}
	pr_info("hvpost_hook: nested_vmx_reflect_vmexit @ %px\n", (void *)reflect_addr);

	ret = ftrace_set_filter_ip(&ops, reflect_addr, 0, 0);
	if (ret) {
		pr_err("hvpost_hook: ftrace_set_filter_ip failed: %d\n", ret);
		return ret;
	}
	ret = register_ftrace_function(&ops);
	if (ret) {
		pr_err("hvpost_hook: register_ftrace_function failed: %d\n", ret);
		ftrace_set_filter_ip(&ops, reflect_addr, 1, 0);
		return ret;
	}
	pr_info("hvpost_hook: armed (VMCALL=%d, HvPostMessage=0x%x, RCX off=0x%x, exit off=0x%x)\n",
		EXIT_REASON_VMCALL, HV_POST_MESSAGE, OFF_VCPU_RCX, OFF_VMX_EXITREASON);
	return 0;
}

static void __exit hvpost_exit(void)
{
	unregister_ftrace_function(&ops);
	ftrace_set_filter_ip(&ops, reflect_addr, 1, 0);
	pr_info("hvpost_hook: unarmed\n");
}

module_init(hvpost_init);
module_exit(hvpost_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Force L0/KVM to handle nested L2 HvPostMessage VMCALL instead of reflecting to L1");
