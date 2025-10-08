# Copyright (C) Microsoft Corporation. All rights reserved.

BASE = 0xffffff8000000000    # We will relocate ourselves to this base address
PML4_INDEX = 511
STACK_PAGES = 4
STACK_TOP = 0x400000

.globl _start
_start:
# Save registers. The caller must have provided a stack since we `ret`
# back to the caller.
push rbp                            # old stack
push rbx                            # physical to virtual offset
push r12                            # old cr3
push r13                            # old pml4 entry
push r14                            # input parameter
push r15                            # input parameter 2
push rax                            # align the stack

mov r14, rdi                        # Save the input parameters
mov r15, rsi

# Clear BSS
lea rdi, __bss_start[rip]           # Put BSS base in rdi
lea rcx, _end[rip]                  # Put BSS end in rcx
sub rcx, rdi                        # Compute BSS len in rcx
xor eax, eax                        # Clear eax
cld                                 # Clear the direction flag for the string operation
rep stosb                           # Zero BSS: memset(rdi, al, rcx)

mov r12, cr3                        # Save old cr3
mov rbx, BASE                       # Get the base virtual address
lea rax, __ehdr_start[rip]          # Get the base physical address
sub rbx, rax                        # Compute the physical-to-virtual offset

# Perform relocations.
lea rdx, _DYNAMIC[rip]              # The start of the dynamic section, rip-relative
mov rsi, BASE                       # The target load address of the image
lea rdi, __ehdr_start[rip]          # The base address of the image
call {relocate}                     # apply relocations (including to page tables)

# Whoops, the page tables were relocated incorrectly to VAs. Re-relocate
# them to PAs.
lea rdi, pt_start[rip]
lea rcx, pt_end[rip]
2:
sub [rdi], rbx
add rdi, 8
cmp rdi, rcx
jne 2b

# Splice the PDPT into the current page table so that we can run at our
# desired base address.
mov r13, [r12 + PML4_INDEX * 8]     # save old pml4 entry
lea rax, 3 + pdpt[rip]
mov [r12 + PML4_INDEX * 8], rax     # splice in pdpt
mov cr3, r12                        # flush tlb just in case

# Change RIP to virtual addressing.
lea rax, 2f[rip]                    # get the physical address of the label
add rax, rbx                        # convert to virtual
jmp rax                             # Jump to the virtual address mapping
2:

# Set the new startup stack.
mov rbp, rsp                            # save the old stack
lea rsp, STACK_TOP + __ehdr_start[rip]  # set the new startup stack

# Drop the identity map page.
lea rax, pml4[rip]                  # get the new pml4
sub rax, rbx                        # convert to physical
mov cr3, rax                        # set new cr3

# Run the program. Preserve rax to pass back to the caller.
mov rdi, r14
mov rsi, r15
call {start}

# Restore the physical stack and identity map page tables.
mov cr3, r12                        # restore old cr3
mov rsp, rbp                        # restore old stack

# Return to physical RIP
lea rcx, 3f[rip]                    # get the virtual address of the label
sub rcx, rbx                        # convert to physical
jmp rcx                             # jump to physical
3:

# Restore the old page table entry.
mov [r12 + PML4_INDEX * 8], r13     # restore old pml4 entry
mov cr3, r12                        # flush TLB

out:
pop r15
pop r15
pop r14
pop r13
pop r12
pop rbx
pop rbp
ret

.globl irq_entry
irq_entry:
push rax
push rcx
push rdx
push rsi
push rdi
push r8
push r9
push r10
push r11
call {irq_handler}
pop r11
pop r10
pop r9
pop r8
pop rdi
pop rsi
pop rdx
pop rcx
pop rax
iretq

.globl exc_gpf
exc_gpf:
mov rdi, 0xd
mov rsi, rsp
jmp {exception_handler}

.globl exc_pf
exc_pf:
mov rdi, 0xe
mov rsi, rsp
jmp {exception_handler}

# Page tables.
.pushsection .data

PTE_PRESENT = 1
PTE_RW = 2
PTE_LARGE = 0x80
PTE_NX = 0x8000000000000000
PTE_TABLE = PTE_PRESENT | PTE_RW
# The image is mapped RWX because it's just one 2MB mapping.
# FUTURE: break this down into the different sections to allow W/X to be set appropriately.
PTE_IMAGE = PTE_PRESENT | PTE_LARGE | PTE_RW
PTE_DATA = PTE_PRESENT | PTE_RW | PTE_NX

.align 4096
pt_start:
pml4:
.fill 511, 8, 0
.quad pdpt + PTE_TABLE

.align 4096
.globl pdpt
pdpt:
.quad pd + PTE_TABLE

.align 4096
pd:
.global IMAGE_PDE
IMAGE_PDE:
.quad __ehdr_start + PTE_IMAGE  # The image. 2MB should be enough for anyone.
.quad pt + PTE_TABLE            # Runtime data.

.align 4096
# The data page table. The layout must match the `addr_space` module.
pt:
.quad pt + PTE_DATA             # Self map for temporary_map.
.quad hypercall_input + PTE_DATA
.quad hypercall_output + PTE_DATA
.fill 509 - STACK_PAGES, 8, 0
.set OFFSET, 0
.rept STACK_PAGES
.quad stack + OFFSET + PTE_DATA
.set OFFSET, OFFSET + 0x1000
.endr

.align 4096
pt_end:

.align 4096
hypercall_input:
.fill 4096, 1, 0
hypercall_output:
.fill 4096, 1, 0

.popsection

.pushsection .bss
.align 4096
stack:
.fill STACK_PAGES * 0x1000, 1, 0
.popsection
