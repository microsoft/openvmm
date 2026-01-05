// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sidecar initialization code. This code runs once, on the BSP, before the
//! main kernel boots.

use super::AFTER_INIT;
use super::CommandErrorWriter;
use super::ENABLE_LOG;
use super::VSM_CAPABILITIES;
use super::VTL_RETURN_OFFSET;
use super::VpGlobals;
use super::addr_space;
use super::temporary_map;
use crate::arch::x86_64::get_hv_vp_register;
use crate::arch::x86_64::hypercall;
use crate::arch::x86_64::log;
use arrayvec::ArrayVec;
use core::fmt::Display;
use core::fmt::Write;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::ptr::addr_of;
use core::ptr::addr_of_mut;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Acquire;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::Ordering::Release;
use hvdef::HvError;
use hvdef::HvRegisterVsmCodePageOffsets;
use hvdef::HvX64RegisterName;
use hvdef::HvX64SegmentRegister;
use hvdef::HypercallCode;
use hvdef::hypercall::EnableVpVtlX64;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::StartVirtualProcessorX64;
use memory_range::AlignedSubranges;
use memory_range::MemoryRange;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::enlightened_panic;
use sidecar_defs::ControlPage;
use sidecar_defs::CpuStatus;
use sidecar_defs::PAGE_SIZE;
use sidecar_defs::PER_VP_PAGES;
use sidecar_defs::PER_VP_SHMEM_PAGES;
use sidecar_defs::SidecarNodeOutput;
use sidecar_defs::SidecarNodeParams;
use sidecar_defs::SidecarOutput;
use sidecar_defs::SidecarParams;
use sidecar_defs::required_memory;
use x86defs::Exception;
use x86defs::GdtEntry;
use x86defs::IdtAttributes;
use x86defs::IdtEntry64;
use x86defs::Pte;
use zerocopy::FromZeros;

unsafe extern "C" {
    static IMAGE_PDE: Pte;
    fn irq_entry();
    fn exc_gpf();
    fn exc_pf();
}

static GDT: [GdtEntry; 4] = {
    let default_data_attributes = x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.as_bits();
    let default_code_attributes = x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.as_bits();
    let zero = GdtEntry {
        limit_low: 0,
        base_low: 0,
        base_middle: 0,
        attr_low: 0,
        attr_high: 0,
        base_high: 0,
    };

    [
        zero,
        zero,
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_code_attributes as u8,
            attr_high: (default_code_attributes >> 8) as u8,
            ..zero
        },
        GdtEntry {
            limit_low: 0xffff,
            attr_low: default_data_attributes as u8,
            attr_high: (default_data_attributes >> 8) as u8,
            ..zero
        },
    ]
};

const IRQ: u8 = 0x20;

static mut IDT: [IdtEntry64; IRQ as usize + 1] = {
    let zero = IdtEntry64 {
        offset_low: 0,
        selector: 0,
        attributes: IdtAttributes::new(),
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };
    [zero; IRQ as usize + 1]
};

enum InitError {
    RequiredMemory { required: u64, actual: u64 },
    GetVsmCodePageOffset(HvError),
    GetVsmCapabilities(HvError),
}

impl Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InitError::RequiredMemory { required, actual } => {
                write!(
                    f,
                    "failed to provide required memory: {:#x}, actual: {:#x}",
                    required, actual
                )
            }
            InitError::GetVsmCodePageOffset(err) => {
                write!(f, "failed to get vsm code page offset: {err}")
            }
            InitError::GetVsmCapabilities(err) => {
                write!(f, "failed to get vsm capabilities: {err}")
            }
        }
    }
}

enum InitVpError {
    EnableVtl2(HvError),
    StartVp(HvError),
}

impl Display for InitVpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InitVpError::EnableVtl2(err) => write!(f, "failed to enable vtl2: {err}"),
            InitVpError::StartVp(err) => write!(f, "failed to start vp: {err}"),
        }
    }
}

/// BSP entry point from entry.S. Called with BSS, stack, and page tables
/// initialized, and relocations applied.
#[cfg_attr(not(minimal_rt), expect(dead_code))]
pub extern "C" fn start(params: u64, output: u64) -> bool {
    enlightened_panic::enable_enlightened_panic();

    let [mut params_mapper, mut output_mapper, mut temp_mapper] = [0, 1, 2].map(|i| {
        // SAFETY: no concurrent accessors to the same index.
        unsafe { temporary_map::Mapper::new(i) }
    });
    // SAFETY: The page is not being concurrently accessed, and it has no
    // invariant requirements.
    let params = unsafe { params_mapper.map::<SidecarParams>(params) };
    // SAFETY: The page is not being concurrently accessed, and it has no
    // invariant requirements.
    let mut output = unsafe { output_mapper.map::<SidecarOutput>(output) };
    match init(&mut temp_mapper, &params, &mut output) {
        Ok(()) => {
            AFTER_INIT.store(true, Release);
            true
        }
        Err(err) => {
            let _ = write!(CommandErrorWriter(&mut output.error), "{err}");
            false
        }
    }
}

/// Called on the BSP to initialize all the APs.
fn init(
    mapper: &mut temporary_map::Mapper,
    params: &SidecarParams,
    output: &mut SidecarOutput,
) -> Result<(), InitError> {
    let &SidecarParams {
        hypercall_page,
        enable_logging,
        node_count,
        ref nodes,
        ref initial_state,
    } = params;

    ENABLE_LOG.store(enable_logging, Relaxed);
    let nodes = &nodes[..node_count as usize];

    // Copy the hypercall page locally since the main kernel will move it after
    // this function returns.
    {
        // SAFETY: The page is not being concurrently accessed, and it has
        // no invariant requirements.
        let hypercall_page = unsafe { mapper.map::<[u8; 4096]>(hypercall_page) };
        // SAFETY: no concurrent accessors to the page.
        unsafe { (&raw mut HYPERCALL_PAGE).copy_from_nonoverlapping(&*hypercall_page, 1) };
    }

    // Initialize the IDT.
    {
        // SAFETY: no concurrent accessors.
        let idt = unsafe { &mut *addr_of_mut!(IDT) };

        let offset = exc_pf as usize as u64;
        idt[Exception::PAGE_FAULT.0 as usize] = IdtEntry64 {
            offset_low: offset as u16,
            selector: 2 * 8,
            attributes: IdtAttributes::new().with_present(false).with_gate_type(0xf),
            offset_middle: (offset >> 16) as u16,
            offset_high: (offset >> 32) as u32,
            reserved: 0,
        };

        let offset = exc_gpf as usize as u64;
        idt[Exception::GENERAL_PROTECTION_FAULT.0 as usize] = IdtEntry64 {
            offset_low: offset as u16,
            selector: 2 * 8,
            attributes: IdtAttributes::new().with_present(false).with_gate_type(0xf),
            offset_middle: (offset >> 16) as u16,
            offset_high: (offset >> 32) as u32,
            reserved: 0,
        };

        let offset = irq_entry as usize as u64;
        idt[IRQ as usize] = IdtEntry64 {
            offset_low: offset as u16,
            selector: 2 * 8,
            attributes: IdtAttributes::new().with_present(true).with_gate_type(0xe),
            offset_middle: (offset >> 16) as u16,
            offset_high: (offset >> 32) as u32,
            reserved: 0,
        };
    }

    // Get the byte offset in the hypercall page of the VTL return function.
    {
        let value = HvRegisterVsmCodePageOffsets::from(
            get_hv_vp_register(
                HvInputVtl::CURRENT_VTL,
                HvX64RegisterName::VsmCodePageOffsets.into(),
            )
            .map_err(InitError::GetVsmCodePageOffset)?
            .as_u64(),
        );
        // SAFETY: no concurrent accessors.
        unsafe { VTL_RETURN_OFFSET = value.return_offset() }
    }

    // Get the reported VSM capabilities.
    {
        let value = get_hv_vp_register(
            HvInputVtl::CURRENT_VTL,
            HvX64RegisterName::VsmCapabilities.into(),
        )
        .map_err(InitError::GetVsmCapabilities)?;
        // SAFETY: no concurrent accessors.
        unsafe { VSM_CAPABILITIES = value.as_u64().into() }
    }

    // SAFETY: no concurrent accesses yet.
    let node_init = unsafe { &mut *addr_of_mut!(NODE_INIT) };

    // Process each node, building the `node_init` array.
    for (node_index, (node, node_output)) in nodes.iter().zip(&mut output.nodes).enumerate() {
        let &SidecarNodeParams {
            memory_base,
            memory_size,
            base_vp,
            vp_count,
        } = node;
        let memory = MemoryRange::new(memory_base..memory_base + memory_size);

        log!("node {node_index}: {vp_count} VPs starting at VP {base_vp}, memory {memory}");

        let required = required_memory(vp_count) as u64;
        if memory_size < required {
            return Err(InitError::RequiredMemory {
                required,
                actual: memory_size,
            });
        }

        let (control_page_range, memory) = memory.split_at_offset(PAGE_SIZE as u64);
        let (shmem_pages, memory) =
            memory.split_at_offset(vp_count as u64 * PER_VP_SHMEM_PAGES as u64 * PAGE_SIZE as u64);

        *node_output = SidecarNodeOutput {
            control_page: control_page_range.start(),
            shmem_pages_base: shmem_pages.start(),
            shmem_pages_size: shmem_pages.len(),
        };

        // Initialize the control page.
        {
            // SAFETY: The page is not being concurrently accessed, and it has
            // no invariant requirements.
            let mut control = unsafe { mapper.map::<ControlPage>(control_page_range.start()) };
            let ControlPage {
                index,
                base_cpu,
                cpu_count,
                request_vector,
                response_cpu,
                response_vector,
                needs_attention,
                reserved,
                cpu_status,
            } = &mut *control;
            *index = (node_index as u32).into();
            *base_cpu = base_vp.into();
            *cpu_count = vp_count.into();
            *request_vector = (IRQ as u32).into();
            *response_cpu = 0.into();
            *response_vector = 0.into();
            *needs_attention = 0.into();
            reserved.fill(0);

            // Default: the base VP is REMOVED (meaning the kernel will start it), the
            // next `vp_count - 1` VPs are RUN, and any remaining VPs are REMOVED.
            cpu_status[0] = CpuStatus::REMOVED.0.into();
            cpu_status[1..vp_count as usize].fill_with(|| CpuStatus::RUN.0.into());
            cpu_status[vp_count as usize..].fill_with(|| CpuStatus::REMOVED.0.into());

            // Override with the initial state from openhcl_boot.
            if initial_state.per_cpu_state_specified {
                for (i, &should_start) in initial_state
                    .sidecar_starts_cpu
                    .iter()
                    .skip(base_vp as usize)
                    .enumerate()
                    .take(vp_count as usize)
                {
                    // Pragmatically, any CPU that has `RUN` should already match
                    // the default above, but set it anyway for clarity.
                    cpu_status[i] = if should_start {
                        CpuStatus::RUN.0.into()
                    } else {
                        CpuStatus::REMOVED.0.into()
                    };
                }
            }
        }

        node_init.push(NodeInit {
            node: NodeDefinition {
                base_vp,
                vp_count,
                control_page_pa: control_page_range.start(),
                shmem_pages,
                memory,
            },
            next_vp: AtomicU32::new(1), // skip the base VP in each node
        });
    }

    // Downgrade the node init array to immutable, then start booting the APs.
    // Each AP that boots will then start helping boot additional APs.
    //
    // SAFETY: no concurrent mutators.
    let node_init = unsafe { &*addr_of!(NODE_INIT) };
    let cpu_status: [AtomicU8; 1] = [0.into()]; // TODO: get the cpu_status for the BSP node
    start_aps(node_init, mapper, &cpu_status);

    // Wait for all the APs to finish starting.
    {
        for (node, output) in nodes.iter().zip(&output.nodes) {
            // SAFETY: The page is not being concurrently accessed, and it has
            // no invariant requirements.
            let control = unsafe { mapper.map::<ControlPage>(output.control_page) };
            for status in &control.cpu_status[0..node.vp_count as usize] {
                while status.load(Acquire) == CpuStatus::RUN.0 {
                    spin_loop();
                }
            }
        }
    }

    Ok(())
}

struct NodeInit {
    node: NodeDefinition,
    next_vp: AtomicU32,
}

static mut NODE_INIT: ArrayVec<NodeInit, { sidecar_defs::MAX_NODES }> = ArrayVec::new_const();

fn start_aps(node_init: &[NodeInit], mapper: &mut temporary_map::Mapper, cpu_status: &[AtomicU8]) {
    for node in node_init {
        loop {
            let node_cpu_index = node.next_vp.fetch_add(1, Relaxed);
            assert!(node_cpu_index != u32::MAX);
            if node_cpu_index >= node.node.vp_count {
                break;
            }

            // See the init function for where we set the initial CPU status for why we can end up here with a REMOVED CPU.
            // todo: map the control page PA for this node rather than relying on the passed in cpu_status slice (since that is only for
            // the node that's actually starting something).
            if cpu_status[node_cpu_index as usize].load(Relaxed) == CpuStatus::REMOVED.0.into() {
                log!(
                    "skipping VP {} (node base {}, node idx {}) as REMOVED",
                    node.node.base_vp + node_cpu_index,
                    node.node.base_vp,
                    node_cpu_index
                );
                continue;
            }

            match node.node.start(mapper, node_cpu_index) {
                Ok(()) => {}
                Err(err) => {
                    panic!(
                        "failed to start VP {}: {}",
                        node.node.base_vp + node_cpu_index,
                        err
                    );
                }
            }
        }
    }
}

/// # Safety
/// Must be called as an AP entry point.
unsafe fn ap_init() -> ! {
    // Start any other pending APs.
    // mattkur: I'm confused why this works ... start_aps will start at node 0, just like the BSP.
    // what's stopping each node from trampling over each other?
    {
        // SAFETY: `NODE_INIT` is set before this routine is called.
        let node_init = unsafe { &*addr_of!(NODE_INIT) };
        // SAFETY: nothing else on this CPU is using the temporary map.
        let mut mapper = unsafe { temporary_map::Mapper::new(0) };
        start_aps(node_init, &mut mapper, &super::vp::control().cpu_status); // see todo in start_aps for why cpu_status is wrong here.
    }
    // SAFETY: this is an entry point.
    unsafe { super::vp::ap_entry() }
}

struct NodeDefinition {
    base_vp: u32,
    vp_count: u32,
    control_page_pa: u64,
    shmem_pages: MemoryRange,
    memory: MemoryRange,
}

impl NodeDefinition {
    fn start(
        &self,
        mapper: &mut temporary_map::Mapper,
        node_cpu_index: u32,
    ) -> Result<(), InitVpError> {
        let hv_vp_index = self.base_vp + node_cpu_index;

        let shmem_pages = self.shmem_pages.start()
            + node_cpu_index as u64 * PER_VP_SHMEM_PAGES as u64 * PAGE_SIZE as u64;
        let command_page_pa = shmem_pages;
        let reg_page_pa = shmem_pages + PAGE_SIZE as u64;
        let memory_start =
            self.memory.start() + node_cpu_index as u64 * PER_VP_PAGES as u64 * PAGE_SIZE as u64;
        let memory =
            MemoryRange::new(memory_start..memory_start + PER_VP_PAGES as u64 * PAGE_SIZE as u64);

        let mut memory = AlignedSubranges::new(memory)
            .with_max_range_len(PAGE_SIZE as u64)
            .map(|r| r.start());
        let pml4_pa = memory.next().unwrap();
        let pdpt_pa = memory.next().unwrap();
        let pd_pa = memory.next().unwrap();
        let pt_pa = memory.next().unwrap();

        let pte_table = |addr| {
            Pte::new()
                .with_address(addr)
                .with_read_write(true)
                .with_present(true)
        };

        {
            // SAFETY: The page is not being concurrently accessed, and it has no
            // invariant requirements.
            let mut pml4 = unsafe { mapper.map::<[Pte; 512]>(pml4_pa) };
            pml4[511] = pte_table(pdpt_pa);
        }
        {
            // SAFETY: The page is not being concurrently accessed, and it has no
            // invariant requirements.
            let mut pdpt = unsafe { mapper.map::<Pte>(pdpt_pa) };
            *pdpt = pte_table(pd_pa);
        }
        {
            // SAFETY: The page is not being concurrently accessed, and it has no
            // invariant requirements.
            let mut pd = unsafe { mapper.map::<[Pte; 512]>(pd_pa) };
            // SAFETY: the PTE is not being concurrently modified.
            pd[0] = unsafe { IMAGE_PDE };
            pd[1] = pte_table(pt_pa);
        }
        let globals_pa = {
            // SAFETY: The page is not being concurrently accessed, and it has no
            // invariant requirements.
            let mut pt = unsafe { mapper.map::<[Pte; 512]>(pt_pa) };
            addr_space::init_ap(
                &mut pt,
                pt_pa,
                self.control_page_pa,
                command_page_pa,
                reg_page_pa,
                &mut memory,
            )
        };
        {
            // SAFETY: The page is not being concurrently accessed, and it has no
            // invariant requirements.
            let mut globals = unsafe { mapper.map::<MaybeUninit<VpGlobals>>(globals_pa) };
            globals.write(VpGlobals {
                hv_vp_index,
                node_cpu_index,
                overlays_mapped: false,
                register_page_mapped: false,
            });
        }

        let cs = HvX64SegmentRegister {
            base: 0,
            limit: !0,
            selector: 2 * 8,
            attributes: x86defs::X64_DEFAULT_CODE_SEGMENT_ATTRIBUTES.into(),
        };
        let ds = HvX64SegmentRegister {
            base: 0,
            limit: !0,
            selector: 3 * 8,
            attributes: x86defs::X64_DEFAULT_DATA_SEGMENT_ATTRIBUTES.into(),
        };
        let gdtr = hvdef::HvX64TableRegister {
            base: addr_of!(GDT) as u64,
            limit: size_of_val(&GDT) as u16 - 1,
            pad: [0; 3],
        };
        let idtr = hvdef::HvX64TableRegister {
            base: addr_of!(IDT) as u64,
            // SAFETY: just getting the size
            limit: size_of_val(unsafe { &*addr_of!(IDT) }) as u16 - 1,
            pad: [0; 3],
        };
        let context = hvdef::hypercall::InitialVpContextX64 {
            rip: ap_init as usize as u64,
            rsp: addr_space::stack().end() - 8, // start unaligned to match calling convention
            rflags: x86defs::RFlags::at_reset().into(),
            cs,
            ds,
            es: ds,
            fs: ds,
            gs: ds,
            ss: ds,
            tr: HvX64SegmentRegister {
                base: 0,
                limit: 0xffff,
                selector: 0,
                attributes: x86defs::X64_BUSY_TSS_SEGMENT_ATTRIBUTES.into(),
            },
            ldtr: FromZeros::new_zeroed(),
            idtr,
            gdtr,
            efer: x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME | x86defs::X64_EFER_NXE,
            cr0: x86defs::X64_CR0_PG | x86defs::X64_CR0_PE | x86defs::X64_CR0_NE,
            cr3: pml4_pa,
            cr4: x86defs::X64_CR4_PAE | x86defs::X64_CR4_MCE | x86defs::X64_CR4_FXSR,
            msr_cr_pat: x86defs::X86X_MSR_DEFAULT_PAT,
        };

        {
            // SAFETY: no concurrent accessors.
            let input_page = unsafe { &mut *addr_space::hypercall_input().cast() };
            let EnableVpVtlX64 {
                partition_id,
                vp_index,
                target_vtl,
                reserved,
                vp_vtl_context,
            } = input_page;

            *partition_id = hvdef::HV_PARTITION_ID_SELF;
            *vp_index = hv_vp_index;
            *target_vtl = hvdef::Vtl::Vtl2.into();
            *vp_vtl_context = context;
            *reserved = [0; 3];
        }
        match hypercall(HypercallCode::HvCallEnableVpVtl, 0) {
            Ok(()) | Err(HvError::VtlAlreadyEnabled) => {}
            Err(err) => return Err(InitVpError::EnableVtl2(err)),
        }

        {
            // SAFETY: no concurrent accessors.
            let input_page = unsafe { &mut *addr_space::hypercall_input().cast() };
            let StartVirtualProcessorX64 {
                partition_id,
                vp_index,
                target_vtl,
                rsvd0,
                rsvd1,
                vp_context,
            } = input_page;

            *partition_id = hvdef::HV_PARTITION_ID_SELF;
            *vp_index = hv_vp_index;
            *target_vtl = hvdef::Vtl::Vtl2.into();
            *rsvd0 = 0;
            *rsvd1 = 0;
            *vp_context = context;
        }
        hypercall(HypercallCode::HvCallStartVirtualProcessor, 0).map_err(InitVpError::StartVp)?;

        Ok(())
    }
}
