// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The openhcl boot loader, which loads before the kernel to set up the
//! kernel's boot parameters.

// See build.rs.
#![cfg_attr(minimal_rt, no_std, no_main)]
// UNSAFETY: Interacting with low level hardware and bootloader primitives.
#![expect(unsafe_code)]
// Allow the allocator api when compiling with `RUSTFLAGS="--cfg nightly"`. This
// is used for some miri tests for testing the bump allocator.
//
// Do not use a normal feature, as that shows errors with rust-analyzer since
// most people are using stable and enable all features. We could remove this
// once the allocator_api feature is stable.
#![cfg_attr(nightly, feature(allocator_api))]

mod arch;
mod boot_logger;
mod cmdline;
mod dt;
mod host_params;
mod hypercall;
mod memory;
mod rt;
mod sidecar;
mod single_threaded;

use crate::arch::setup_vtl2_memory;
use crate::arch::setup_vtl2_vp;
#[cfg(target_arch = "x86_64")]
use crate::arch::tdx::get_tdx_tsc_reftime;
use crate::arch::verify_imported_regions_hash;
use crate::boot_logger::boot_logger_memory_init;
use crate::boot_logger::boot_logger_runtime_init;
use crate::boot_logger::boot_logger_write_memory_log_to_runtime;
use crate::hypercall::hvcall;
use crate::memory::AddressSpaceManager;
use crate::single_threaded::OffStackRef;
use crate::single_threaded::off_stack;
use arrayvec::ArrayString;
use arrayvec::ArrayVec;
use cmdline::BootCommandLineOptions;
use core::fmt::Write;
use dt::BootTimes;
use dt::write_dt;
use host_params::COMMAND_LINE_SIZE;
use host_params::PartitionInfo;
use host_params::shim_params::IsolationType;
use host_params::shim_params::ShimParams;
use hvdef::Vtl;
use loader_defs::linux::SETUP_DTB;
use loader_defs::linux::setup_data;
use loader_defs::shim::ShimParamsRaw;
use memory_range::RangeWalkResult;
use memory_range::walk_ranges;
use minimal_rt::enlightened_panic::enable_enlightened_panic;
use sidecar::SidecarConfig;
use sidecar_defs::SidecarOutput;
use sidecar_defs::SidecarParams;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Debug)]
struct CommandLineTooLong;

impl From<core::fmt::Error> for CommandLineTooLong {
    fn from(_: core::fmt::Error) -> Self {
        Self
    }
}

struct BuildKernelCommandLineParams<'a> {
    params: &'a ShimParams,
    cmdline: &'a mut ArrayString<COMMAND_LINE_SIZE>,
    partition_info: &'a PartitionInfo,
    can_trust_host: bool,
    is_confidential_debug: bool,
    sidecar: Option<&'a SidecarConfig<'a>>,
    vtl2_pool_supported: bool,
}

/// Read and setup the underhill kernel command line into the specified buffer.
fn build_kernel_command_line(
    fn_params: BuildKernelCommandLineParams<'_>,
) -> Result<(), CommandLineTooLong> {
    let BuildKernelCommandLineParams {
        params,
        cmdline,
        partition_info,
        can_trust_host,
        is_confidential_debug,
        sidecar,
        vtl2_pool_supported,
    } = fn_params;

    // For reference:
    // https://www.kernel.org/doc/html/v5.15/admin-guide/kernel-parameters.html
    const KERNEL_PARAMETERS: &[&str] = &[
        // If a console is specified, then write everything to it.
        "loglevel=8",
        // Use a fixed 128KB log buffer by default.
        "log_buf_len=128K",
        // Enable time output on console for ohcldiag-dev.
        "printk.time=1",
        // Enable facility and level output on console for ohcldiag-dev.
        "console_msg_format=syslog",
        // Set uio parameter to configure vmbus ring buffer behavior.
        "uio_hv_generic.no_mask=1",
        // RELIABILITY: Dump anonymous pages and ELF headers only. Skip over
        // huge pages and the shared pages.
        "coredump_filter=0x33",
        // PERF: No processor frequency governing.
        "cpufreq.off=1",
        // PERF: Disable the CPU idle time management entirely. It does not
        // prevent the idle loop from running on idle CPUs, but it prevents
        // the CPU idle time governors and drivers from being invoked.
        "cpuidle.off=1",
        // PERF: No perf checks for crypto algorithms to boot faster.
        // Would have to evaluate the perf wins on the crypto manager vs
        // delaying the boot up.
        "cryptomgr.notests",
        // PERF: Idle threads use HLT on x64 if there is no work.
        // Believed to be a compromise between waking up the processor
        // and the power consumption.
        "idle=halt",
        // WORKAROUND: Avoid init calls that assume presence of CMOS (Simple
        // Boot Flag) or allocate the real-mode trampoline for APs.
        "initcall_blacklist=init_real_mode,sbf_init",
        // CONFIG-STATIC, PERF: Static loops-per-jiffy value to save time on boot.
        "lpj=3000000",
        // PERF: No broken timer check to boot faster.
        "no_timer_check",
        // CONFIG-STATIC, PERF: Using xsave makes VTL transitions being
        // much slower. The xsave state is shared between VTLs, and we don't
        // context switch it in the kernel when leaving/entering VTL2.
        // Removing this will lead to corrupting register state and the
        // undefined behaviour.
        "noxsave",
        // RELIABILITY: Panic on MCEs and faults in the kernel.
        "oops=panic",
        // RELIABILITY: Don't panic on kernel warnings.
        "panic_on_warn=0",
        // PERF, RELIABILITY: Don't print detailed information about the failing
        // processes (memory maps, threads).
        "panic_print=0",
        // RELIABILITY: Reboot immediately on panic, no timeout.
        "panic=-1",
        // RELIABILITY: Don't print processor context information on a fatal
        // signal. Our crash dump collection infrastructure seems reliable, and
        // this information doesn't seem useful without a dump anyways.
        // Additionally it may push important logs off the end of the kmsg
        // page logged by the host.
        //"print_fatal_signals=0",
        // RELIABILITY: Unlimited logging to /dev/kmsg from userspace.
        "printk.devkmsg=on",
        // RELIABILITY: Reboot using a triple fault as the fastest method.
        // That is also the method used for compatibility with earlier versions
        // of the Microsoft HCL.
        "reboot=t",
        // CONFIG-STATIC: Type of the root file system.
        "rootfstype=tmpfs",
        // PERF: Deactivate kcompactd kernel thread, otherwise it will queue a
        // scheduler timer periodically, which introduces jitters for VTL0.
        "sysctl.vm.compaction_proactiveness=0",
        // PERF: No TSC stability check when booting up to boot faster,
        // also no validation during runtime.
        "tsc=reliable",
        // RELIABILITY: Panic on receiving an NMI.
        "unknown_nmi_panic=1",
        // Use vfio for MANA devices.
        "vfio_pci.ids=1414:00ba",
        // WORKAROUND: Enable no-IOMMU mode. This mode provides no device isolation,
        // and no DMA translation.
        "vfio.enable_unsafe_noiommu_mode=1",
        // Specify the init path.
        "rdinit=/underhill-init",
        // Default to user-mode NVMe driver.
        "OPENHCL_NVME_VFIO=1",
        // The next three items reduce the memory overhead of the storvsc driver.
        // Since it is only used for DVD, performance is not critical.
        "hv_storvsc.storvsc_vcpus_per_sub_channel=2048",
        // Fix number of hardware queues at 2.
        "hv_storvsc.storvsc_max_hw_queues=2",
        // Reduce the ring buffer size to 32K.
        "hv_storvsc.storvsc_ringbuffer_size=0x8000",
        // Disable eager mimalloc commit to prevent core dumps from being overly large
        "MIMALLOC_ARENA_EAGER_COMMIT=0",
        // Disable acpi runtime support. Unused in underhill, but some support
        // is compiled in for the kernel (ie TDX mailbox protocol).
        "acpi=off",
    ];

    const X86_KERNEL_PARAMETERS: &[&str] = &[
        // Disable pcid support. This is a temporary fix to allow
        // Underhill to run nested inside AMD VMs. Otherwise, the
        // Underhill kernel tries to start APs with PCID bits set in CR3
        // without the PCIDE bit set in CR4, which is an invalid
        // VP state (according to the mshv nested implementation).
        //
        // TODO: remove this once we figure out the root cause and apply
        // a workaround/fix elsewhere.
        "clearcpuid=pcid",
        // Disable all attempts to use an IOMMU, including swiotlb.
        "iommu=off",
        // Don't probe for a PCI bus. PCI devices currently come from VPCI. When
        // this changes, we will explicitly enumerate a PCI bus via devicetree.
        "pci=off",
    ];

    const AARCH64_KERNEL_PARAMETERS: &[&str] = &[];

    for p in KERNEL_PARAMETERS {
        write!(cmdline, "{p} ")?;
    }

    let arch_parameters = if cfg!(target_arch = "x86_64") {
        X86_KERNEL_PARAMETERS
    } else {
        AARCH64_KERNEL_PARAMETERS
    };
    for p in arch_parameters {
        write!(cmdline, "{p} ")?;
    }

    const HARDWARE_ISOLATED_KERNEL_PARAMETERS: &[&str] = &[
        // Even with iommu=off, the SWIOTLB is still allocated on AARCH64
        // (iommu=off ignored entirely), and CVMs (memory encryption forces it
        // on). Set it to a single area in 8MB. The first parameter controls the
        // area size in slabs (2KB per slab), the second controls the number of
        // areas (default is # of CPUs).
        //
        // This is set to 8MB on hardware isolated VMs since there are some
        // scenarios, such as provisioning over DVD, which require a larger size
        // since the buffer is being used.
        "swiotlb=4096,1",
    ];

    const NON_HARDWARE_ISOLATED_KERNEL_PARAMETERS: &[&str] = &[
        // Even with iommu=off, the SWIOTLB is still allocated on AARCH64
        // (iommu=off ignored entirely). Set it to the minimum, saving ~63 MiB.
        // The first parameter controls the area size, the second controls the
        // number of areas (default is # of CPUs). Set them both to the minimum.
        "swiotlb=1,1",
    ];

    if params.isolation_type.is_hardware_isolated() {
        for p in HARDWARE_ISOLATED_KERNEL_PARAMETERS {
            write!(cmdline, "{p} ")?;
        }
    } else {
        for p in NON_HARDWARE_ISOLATED_KERNEL_PARAMETERS {
            write!(cmdline, "{p} ")?;
        }
    }

    // Enable the com3 console by default if it's available and we're not
    // isolated, or if we are isolated but also have debugging enabled.
    //
    // Otherwise, set the console to ttynull so the kernel does not default to
    // com1. This is overridden by any user customizations in the static or
    // dynamic command line, as this console argument provided by the bootloader
    // comes first.
    let console = if partition_info.com3_serial_available && can_trust_host {
        "ttyS2,115200"
    } else {
        "ttynull"
    };
    write!(cmdline, "console={console} ")?;

    if params.isolation_type != IsolationType::None {
        write!(
            cmdline,
            "{}=1 ",
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_ENV_VAR_NAME
        )?;
    }

    if is_confidential_debug {
        write!(
            cmdline,
            "{}=1 ",
            underhill_confidentiality::OPENHCL_CONFIDENTIAL_DEBUG_ENV_VAR_NAME
        )?;
    }

    // Generate the NVMe keep alive command line which should look something
    // like: OPENHCL_NVME_KEEP_ALIVE=disabled,host,privatepool
    // TODO: Move from command line to device tree when stabilized.
    write!(cmdline, "OPENHCL_NVME_KEEP_ALIVE=")?;

    if partition_info.boot_options.disable_nvme_keep_alive {
        write!(cmdline, "disabled,")?;
    }

    if partition_info.nvme_keepalive {
        write!(cmdline, "host,")?;
    } else {
        write!(cmdline, "nohost,")?;
    }

    if vtl2_pool_supported {
        write!(cmdline, "privatepool ")?;
    } else {
        write!(cmdline, "noprivatepool ")?;
    }

    if let Some(sidecar) = sidecar {
        write!(cmdline, "{} ", sidecar.kernel_command_line())?;
    }

    if !cmdline.contains("hv_vmbus.message_connection_id") {
        // HACK: Set the vmbus connection id via kernel commandline if we haven't
        // gotten one from elsewhere.
        //
        // This code will be removed when the kernel supports setting connection id
        // via device tree.
        write!(
            cmdline,
            "hv_vmbus.message_connection_id=0x{:x} ",
            partition_info.vmbus_vtl2.connection_id
        )?;
    }

    // Prepend the computed parameters to the original command line.
    cmdline.write_str(&partition_info.cmdline)?;

    Ok(())
}

// The Linux kernel requires that the FDT fit within a single 256KB mapping, as
// that is the maximum size the kernel can use during its early boot processes.
// We also want our FDT to be as large as possible to support as many vCPUs as
// possible. We set it to 256KB, but it must also be page-aligned, as leaving it
// unaligned runs the possibility of it taking up 1 too many pages, resulting in
// a 260KB mapping, which will fail.
const FDT_SIZE: usize = 256 * 1024;

#[repr(C, align(4096))]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct Fdt {
    header: setup_data,
    data: [u8; FDT_SIZE - size_of::<setup_data>()],
}

/// Raw shim parameters are provided via a relative offset from the base of
/// where the shim is loaded. Return a ShimParams structure based on the raw
/// offset based RawShimParams.
fn shim_parameters(shim_params_raw_offset: isize) -> ShimParams {
    unsafe extern "C" {
        static __ehdr_start: u8;
    }

    let shim_base = core::ptr::addr_of!(__ehdr_start) as usize;

    // SAFETY: The host is required to relocate everything by the same bias, so
    //         the shim parameters should be at the build time specified offset
    //         from the base address of the image.
    let raw_shim_params = unsafe {
        &*(shim_base.wrapping_add_signed(shim_params_raw_offset) as *const ShimParamsRaw)
    };

    ShimParams::new(shim_base as u64, raw_shim_params)
}

#[cfg_attr(not(target_arch = "x86_64"), expect(dead_code))]
mod x86_boot {
    use crate::PageAlign;
    use crate::memory::AddressSpaceManager;
    use crate::single_threaded::OffStackRef;
    use crate::single_threaded::off_stack;
    use crate::zeroed;
    use core::mem::size_of;
    use core::ops::Range;
    use core::ptr;
    use loader_defs::linux::E820_RAM;
    use loader_defs::linux::E820_RESERVED;
    use loader_defs::linux::SETUP_E820_EXT;
    use loader_defs::linux::boot_params;
    use loader_defs::linux::e820entry;
    use loader_defs::linux::setup_data;
    use loader_defs::shim::MemoryVtlType;
    use memory_range::MemoryRange;
    use zerocopy::FromZeros;
    use zerocopy::Immutable;
    use zerocopy::KnownLayout;

    #[repr(C)]
    #[derive(FromZeros, Immutable, KnownLayout)]
    pub struct E820Ext {
        pub header: setup_data,
        pub entries: [e820entry; 512],
    }

    fn add_e820_entry(
        entry: Option<&mut e820entry>,
        range: MemoryRange,
        typ: u32,
    ) -> Result<(), BuildE820MapError> {
        *entry.ok_or(BuildE820MapError::OutOfE820Entries)? = e820entry {
            addr: range.start().into(),
            size: range.len().into(),
            typ: typ.into(),
        };
        Ok(())
    }

    #[derive(Debug)]
    pub enum BuildE820MapError {
        /// Out of e820 entries.
        OutOfE820Entries,
    }

    /// Build the e820 map for the kernel representing usable VTL2 ram.
    pub fn build_e820_map(
        boot_params: &mut boot_params,
        ext: &mut E820Ext,
        address_space: &AddressSpaceManager,
    ) -> Result<bool, BuildE820MapError> {
        boot_params.e820_entries = 0;
        let mut entries = boot_params
            .e820_map
            .iter_mut()
            .chain(ext.entries.iter_mut());

        let mut n = 0;
        for (range, typ) in address_space.vtl2_ranges() {
            match typ {
                MemoryVtlType::VTL2_RAM => {
                    add_e820_entry(entries.next(), range, E820_RAM)?;
                    n += 1;
                }
                MemoryVtlType::VTL2_CONFIG
                | MemoryVtlType::VTL2_SIDECAR_IMAGE
                | MemoryVtlType::VTL2_SIDECAR_NODE
                | MemoryVtlType::VTL2_RESERVED
                | MemoryVtlType::VTL2_GPA_POOL
                | MemoryVtlType::VTL2_TDX_PAGE_TABLES
                | MemoryVtlType::VTL2_BOOTSHIM_LOG_BUFFER
                | MemoryVtlType::VTL2_PERSISTED_STATE_HEADER
                | MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF => {
                    add_e820_entry(entries.next(), range, E820_RESERVED)?;
                    n += 1;
                }

                _ => {
                    panic!("unexpected vtl2 ram type {typ:?} for range {range:#?}");
                }
            }
        }

        let base = n.min(boot_params.e820_map.len());
        boot_params.e820_entries = base as u8;

        if base < n {
            ext.header.len = ((n - base) * size_of::<e820entry>()) as u32;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn build_boot_params(
        address_space: &AddressSpaceManager,
        initrd: Range<u64>,
        cmdline: &str,
        setup_data_head: *const setup_data,
        setup_data_tail: &mut &mut setup_data,
    ) -> OffStackRef<'static, PageAlign<boot_params>> {
        let mut boot_params_storage = off_stack!(PageAlign<boot_params>, zeroed());
        let boot_params = &mut boot_params_storage.0;
        boot_params.hdr.type_of_loader = 0xff; // Unknown loader type

        // HACK: A kernel change just in the Underhill kernel tree has a workaround
        // to disable probe_roms and reserve_bios_regions when X86_SUBARCH_LGUEST
        // (1) is set by the bootloader. This stops the kernel from reading VTL0
        // memory during kernel boot, which can have catastrophic consequences
        // during a servicing operation when VTL0 has written values to memory, or
        // unaccepted page accesses in an isolated partition.
        //
        // This is only intended as a stopgap until a suitable upstreamable kernel
        // patch is made.
        boot_params.hdr.hardware_subarch = 1.into();

        boot_params.hdr.ramdisk_image = (initrd.start as u32).into();
        boot_params.ext_ramdisk_image = (initrd.start >> 32) as u32;
        let initrd_len = initrd.end - initrd.start;
        boot_params.hdr.ramdisk_size = (initrd_len as u32).into();
        boot_params.ext_ramdisk_size = (initrd_len >> 32) as u32;

        let e820_ext = OffStackRef::leak(off_stack!(E820Ext, zeroed()));

        let used_ext = build_e820_map(boot_params, e820_ext, address_space)
            .expect("building e820 map must succeed");

        if used_ext {
            e820_ext.header.ty = SETUP_E820_EXT;
            setup_data_tail.next = ptr::from_ref(&e820_ext.header) as u64;
            *setup_data_tail = &mut e820_ext.header;
        }

        let cmd_line_addr = cmdline.as_ptr() as u64;
        boot_params.hdr.cmd_line_ptr = (cmd_line_addr as u32).into();
        boot_params.ext_cmd_line_ptr = (cmd_line_addr >> 32) as u32;

        boot_params.hdr.setup_data = (setup_data_head as u64).into();

        boot_params_storage
    }
}

/// Build the cc_blob containing the location of different parameters associated with SEV.
#[cfg(target_arch = "x86_64")]
fn build_cc_blob_sev_info(
    cc_blob: &mut loader_defs::linux::cc_blob_sev_info,
    shim_params: &ShimParams,
) {
    // TODO SNP: Currently only the first CPUID page is passed through.
    // Consider changing this.
    cc_blob.magic = loader_defs::linux::CC_BLOB_SEV_INFO_MAGIC;
    cc_blob.version = 0;
    cc_blob._reserved = 0;
    cc_blob.secrets_phys = shim_params.secrets_start();
    cc_blob.secrets_len = hvdef::HV_PAGE_SIZE as u32;
    cc_blob._rsvd1 = 0;
    cc_blob.cpuid_phys = shim_params.cpuid_start();
    cc_blob.cpuid_len = hvdef::HV_PAGE_SIZE as u32;
    cc_blob._rsvd2 = 0;
}

#[repr(C, align(4096))]
#[derive(FromZeros, Immutable, KnownLayout)]
struct PageAlign<T>(T);

const fn zeroed<T: FromZeros>() -> T {
    // SAFETY: `T` implements `FromZeros`, so this is a safe initialization of `T`.
    unsafe { core::mem::MaybeUninit::<T>::zeroed().assume_init() }
}

fn get_ref_time(isolation: IsolationType) -> Option<u64> {
    match isolation {
        #[cfg(target_arch = "x86_64")]
        IsolationType::Tdx => get_tdx_tsc_reftime(),
        #[cfg(target_arch = "x86_64")]
        IsolationType::Snp => None,
        _ => Some(minimal_rt::reftime::reference_time()),
    }
}

fn shim_main(shim_params_raw_offset: isize) -> ! {
    let p = shim_parameters(shim_params_raw_offset);
    if p.isolation_type == IsolationType::None {
        enable_enlightened_panic();
    }

    // Enable the in-memory log.
    boot_logger_memory_init(p.log_buffer);

    log::set_logger(&boot_logger::BOOT_LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    log::info!("test log");
    log::trace!("test trace");

    let boot_reftime = get_ref_time(p.isolation_type);

    // The support code for the fast hypercalls does not set
    // the Guest ID if it is not set yet as opposed to the slow
    // hypercall code path where that is done automatically.
    // Thus the fast hypercalls will fail as the the Guest ID has
    // to be set first hence initialize hypercall support
    // explicitly.
    if !p.isolation_type.is_hardware_isolated() {
        hvcall().initialize();
    }

    let mut static_options = BootCommandLineOptions::new();
    if let Some(cmdline) = p.command_line().command_line() {
        static_options.parse(cmdline);
    }

    let static_confidential_debug = static_options.confidential_debug;
    let can_trust_host = p.isolation_type == IsolationType::None || static_confidential_debug;

    let mut dt_storage = off_stack!(PartitionInfo, PartitionInfo::new());
    let address_space = OffStackRef::leak(off_stack!(
        AddressSpaceManager,
        AddressSpaceManager::new_const()
    ));
    let partition_info = match PartitionInfo::read_from_dt(
        &p,
        &mut dt_storage,
        address_space,
        static_options,
        can_trust_host,
    ) {
        Ok(val) => val,
        Err(e) => panic!("unable to read device tree params {}", e),
    };

    // Enable logging ASAP. This is fine even when isolated, as we don't have
    // any access to secrets in the boot shim.
    boot_logger_runtime_init(p.isolation_type, partition_info.com3_serial_available);
    log::info!("openhcl_boot: logging enabled");
    boot_logger_write_memory_log_to_runtime();

    // Confidential debug will show up in boot_options only if included in the
    // static command line, or if can_trust_host is true (so the dynamic command
    // line has been parsed).
    let is_confidential_debug =
        static_confidential_debug || partition_info.boot_options.confidential_debug;

    // Fill out the non-devicetree derived parts of PartitionInfo.
    if !p.isolation_type.is_hardware_isolated()
        && hvcall().vtl() == Vtl::Vtl2
        && hvdef::HvRegisterVsmCapabilities::from(
            hvcall()
                .get_register(hvdef::HvAllArchRegisterName::VsmCapabilities.into())
                .expect("failed to query vsm capabilities")
                .as_u64(),
        )
        .vtl0_alias_map_available()
    {
        // If the vtl0 alias map was not provided in the devicetree, attempt to
        // derive it from the architectural physical address bits.
        //
        // The value in the ID_AA64MMFR0_EL1 register used to determine the
        // physical address bits can only represent multiples of 4. As a result,
        // the Surface Pro X (and systems with similar CPUs) cannot properly
        // report their address width of 39 bits. This causes the calculated
        // alias map to be incorrect, which results in panics when trying to
        // read memory and getting invalid data.
        if partition_info.vtl0_alias_map.is_none() {
            partition_info.vtl0_alias_map =
                Some(1 << (arch::physical_address_bits(p.isolation_type) - 1));
        }
    } else {
        // Ignore any devicetree-provided alias map if the conditions above
        // aren't met.
        partition_info.vtl0_alias_map = None;
    }

    // Rebind partition_info as no longer mutable.
    let partition_info: &PartitionInfo = partition_info;

    if partition_info.cpus.is_empty() {
        panic!("no cpus");
    }

    validate_vp_hw_ids(partition_info);

    setup_vtl2_memory(&p, partition_info, address_space);
    setup_vtl2_vp(partition_info);

    verify_imported_regions_hash(&p);

    let mut sidecar_params = off_stack!(PageAlign<SidecarParams>, zeroed());
    let mut sidecar_output = off_stack!(PageAlign<SidecarOutput>, zeroed());
    let sidecar = sidecar::start_sidecar(
        &p,
        partition_info,
        address_space,
        &mut sidecar_params.0,
        &mut sidecar_output.0,
    );

    // Rebind address_space as no longer mutable.
    let address_space: &AddressSpaceManager = address_space;

    let mut cmdline = off_stack!(ArrayString<COMMAND_LINE_SIZE>, ArrayString::new_const());
    build_kernel_command_line(BuildKernelCommandLineParams {
        params: &p,
        cmdline: &mut cmdline,
        partition_info,
        can_trust_host,
        is_confidential_debug,
        sidecar: sidecar.as_ref(),
        vtl2_pool_supported: address_space.has_vtl2_pool(),
    })
    .unwrap();

    let mut fdt = off_stack!(Fdt, zeroed());
    fdt.header.len = fdt.data.len() as u32;
    fdt.header.ty = SETUP_DTB;

    #[cfg(target_arch = "x86_64")]
    let mut setup_data_tail = &mut fdt.header;
    #[cfg(target_arch = "x86_64")]
    let setup_data_head = core::ptr::from_ref(setup_data_tail);

    #[cfg(target_arch = "x86_64")]
    if p.isolation_type == IsolationType::Snp {
        let cc_blob = OffStackRef::leak(off_stack!(loader_defs::linux::cc_blob_sev_info, zeroed()));
        build_cc_blob_sev_info(cc_blob, &p);

        let cc_data = OffStackRef::leak(off_stack!(loader_defs::linux::cc_setup_data, zeroed()));
        cc_data.header.len = size_of::<loader_defs::linux::cc_setup_data>() as u32;
        cc_data.header.ty = loader_defs::linux::SETUP_CC_BLOB;
        cc_data.cc_blob_address = core::ptr::from_ref(&*cc_blob) as u32;

        // Chain in the setup data.
        setup_data_tail.next = core::ptr::from_ref(&*cc_data) as u64;
        setup_data_tail = &mut cc_data.header;
    }

    let initrd = p.initrd_base..p.initrd_base + p.initrd_size;

    // Validate the initrd crc matches what was put at file generation time.
    let computed_crc = crc32fast::hash(p.initrd());
    assert_eq!(
        computed_crc, p.initrd_crc,
        "computed initrd crc does not match build time calculated crc"
    );

    #[cfg(target_arch = "x86_64")]
    let boot_params = x86_boot::build_boot_params(
        address_space,
        initrd.clone(),
        &cmdline,
        setup_data_head,
        &mut setup_data_tail,
    );

    // Compute the ending boot time. This has to be before writing to device
    // tree, so this is as late as we can do it.

    let boot_times = boot_reftime.map(|start| BootTimes {
        start,
        end: get_ref_time(p.isolation_type).unwrap_or(0),
    });

    // Validate that no imported regions that are pending are not part of vtl2
    // ram.
    for (range, result) in walk_ranges(
        partition_info.vtl2_ram.iter().map(|r| (r.range, ())),
        p.imported_regions(),
    ) {
        match result {
            RangeWalkResult::Neither | RangeWalkResult::Left(_) | RangeWalkResult::Both(_, _) => {}
            RangeWalkResult::Right(accepted) => {
                // Ranges that are not a part of VTL2 ram must have been
                // preaccepted, as usermode expect that to be the case.
                assert!(
                    accepted,
                    "range {:#x?} not in vtl2 ram was not preaccepted at launch",
                    range
                );
            }
        }
    }

    write_dt(
        &mut fdt.data,
        partition_info,
        address_space,
        p.imported_regions().map(|r| {
            // Discard if the range was previously pending - the bootloader has
            // accepted all pending ranges.
            //
            // NOTE: No VTL0 memory today is marked as pending. The check above
            // validates that, and this code may need to change if this becomes
            // no longer true.
            r.0
        }),
        initrd,
        &cmdline,
        sidecar.as_ref(),
        boot_times,
        p.isolation_type,
    )
    .unwrap();

    rt::verify_stack_cookie();

    log::info!("uninitializing hypercalls, about to jump to kernel");
    hvcall().uninitialize();

    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            // SAFETY: the parameter blob is trusted.
            let kernel_entry: extern "C" fn(u64, &loader_defs::linux::boot_params) -> ! =
                unsafe { core::mem::transmute(p.kernel_entry_address) };
            kernel_entry(0, &boot_params.0)
        } else if #[cfg(target_arch = "aarch64")] {
            // SAFETY: the parameter blob is trusted.
            let kernel_entry: extern "C" fn(fdt_data: *const u8, mbz0: u64, mbz1: u64, mbz2: u64) -> ! =
                unsafe { core::mem::transmute(p.kernel_entry_address) };
            // Disable MMU for kernel boot without EFI, as required by the boot protocol.
            // Flush (and invalidate) the caches, as that is required for disabling MMU.
            // SAFETY: Just changing a bit in the register and then jumping to the kernel.
            unsafe {
                core::arch::asm!(
                    "
                    mrs     {0}, sctlr_el1
                    bic     {0}, {0}, #0x1
                    msr     sctlr_el1, {0}
                    tlbi    vmalle1
                    dsb     sy
                    isb     sy",
                    lateout(reg) _,
                );
            }
            kernel_entry(fdt.data.as_ptr(), 0, 0, 0)
        } else {
            panic!("unsupported arch")
        }
    }
}

/// Ensure that mshv VP indexes for the CPUs listed in the partition info
/// correspond to the N in the cpu@N devicetree node name. OpenVMM assumes that
/// this will be the case.
fn validate_vp_hw_ids(partition_info: &PartitionInfo) {
    use host_params::MAX_CPU_COUNT;
    use hypercall::HwId;

    if partition_info.isolation.is_hardware_isolated() {
        // TODO TDX SNP: we don't have a GHCB/GHCI page set up to communicate
        // with the hypervisor here, so we can't easily perform the check. Since
        // there is no security impact to this check, we can skip it for now; if
        // the VM fails to boot, then this is due to a host contract violation.
        //
        // For TDX, we could use ENUM TOPOLOGY to validate that the TD VCPU
        // indexes correspond to the APIC IDs in the right order. I am not
        // certain if there are places where we depend on this mapping today.
        return;
    }

    if hvcall().vtl() != Vtl::Vtl2 {
        // If we're not using guest VSM, then the guest won't communicate
        // directly with the hypervisor, so we can choose the VP indexes
        // ourselves.
        return;
    }

    // Ensure the host and hypervisor agree on VP index ordering.

    let mut hw_ids = off_stack!(ArrayVec<HwId, MAX_CPU_COUNT>, ArrayVec::new_const());
    hw_ids.clear();
    hw_ids.extend(partition_info.cpus.iter().map(|c| c.reg as _));
    let mut vp_indexes = off_stack!(ArrayVec<u32, MAX_CPU_COUNT>, ArrayVec::new_const());
    vp_indexes.clear();
    if let Err(err) = hvcall().get_vp_index_from_hw_id(&hw_ids, &mut vp_indexes) {
        panic!(
            "failed to get VP index for hardware ID {:#x}: {}",
            hw_ids[vp_indexes.len().min(hw_ids.len() - 1)],
            err
        );
    }
    if let Some((i, &vp_index)) = vp_indexes
        .iter()
        .enumerate()
        .find(|&(i, vp_index)| i as u32 != *vp_index)
    {
        panic!(
            "CPU hardware ID {:#x} does not correspond to VP index {}",
            hw_ids[i], vp_index
        );
    }
}

// See build.rs. See `mod rt` for the actual bootstrap code required to invoke
// shim_main.
#[cfg(not(minimal_rt))]
fn main() {
    unimplemented!("build with MINIMAL_RT_BUILD to produce a working boot loader");
}

#[cfg(test)]
mod test {
    use super::x86_boot::E820Ext;
    use super::x86_boot::build_e820_map;
    use crate::cmdline::BootCommandLineOptions;
    use crate::dt::write_dt;
    use crate::host_params::MAX_CPU_COUNT;
    use crate::host_params::PartitionInfo;
    use crate::host_params::shim_params::IsolationType;
    use crate::memory::AddressSpaceManager;
    use crate::memory::AddressSpaceManagerBuilder;
    use arrayvec::ArrayString;
    use arrayvec::ArrayVec;
    use core::ops::Range;
    use host_fdt_parser::CpuEntry;
    use host_fdt_parser::MemoryEntry;
    use host_fdt_parser::VmbusInfo;
    use igvm_defs::MemoryMapEntryType;
    use loader_defs::linux::E820_RAM;
    use loader_defs::linux::E820_RESERVED;
    use loader_defs::linux::boot_params;
    use loader_defs::linux::e820entry;
    use memory_range::MemoryRange;
    use memory_range::subtract_ranges;
    use zerocopy::FromZeros;

    const HIGH_MMIO_GAP_END: u64 = 0x1000000000; //  64 GiB
    const VMBUS_MMIO_GAP_SIZE: u64 = 0x10000000; // 256 MiB
    const HIGH_MMIO_GAP_START: u64 = HIGH_MMIO_GAP_END - VMBUS_MMIO_GAP_SIZE;

    /// Create partition info with given cpu count enabled and sequential
    /// apic_ids.
    fn new_partition_info(cpu_count: usize) -> PartitionInfo {
        let mut cpus: ArrayVec<CpuEntry, MAX_CPU_COUNT> = ArrayVec::new();

        for id in 0..(cpu_count as u64) {
            cpus.push(CpuEntry { reg: id, vnode: 0 });
        }

        let mut mmio = ArrayVec::new();
        mmio.push(
            MemoryRange::try_new(HIGH_MMIO_GAP_START..HIGH_MMIO_GAP_END).expect("valid range"),
        );

        PartitionInfo {
            vtl2_ram: ArrayVec::new(),
            partition_ram: ArrayVec::new(),
            isolation: IsolationType::None,
            bsp_reg: cpus[0].reg as u32,
            cpus,
            cmdline: ArrayString::new(),
            vmbus_vtl2: VmbusInfo {
                mmio,
                connection_id: 0,
            },
            vmbus_vtl0: VmbusInfo {
                mmio: ArrayVec::new(),
                connection_id: 0,
            },
            com3_serial_available: false,
            gic: None,
            pmu_gsiv: None,
            memory_allocation_mode: host_fdt_parser::MemoryAllocationMode::Host,
            entropy: None,
            vtl0_alias_map: None,
            nvme_keepalive: false,
            boot_options: BootCommandLineOptions::new(),
        }
    }

    // ensure we can boot with a _lot_ of vcpus
    #[test]
    #[cfg_attr(
        target_arch = "aarch64",
        ignore = "TODO: investigate why this doesn't always work on ARM"
    )]
    fn fdt_cpu_scaling() {
        const MAX_CPUS: usize = 2048;

        let mut buf = [0; 0x40000];
        write_dt(
            &mut buf,
            &new_partition_info(MAX_CPUS),
            &AddressSpaceManager::new_const(),
            [],
            0..0,
            &ArrayString::from("test").unwrap_or_default(),
            None,
            None,
            IsolationType::None,
        )
        .unwrap();
    }

    // Must match the DeviceTree blob generated with the standard tooling
    // to ensure being compliant to the standards (or, at least, compatibility
    // with a widely used implementation).
    // For details on regenerating the test content, see `fdt_dtc_decompile`
    // below.
    #[test]
    #[ignore = "TODO: temporarily broken"]
    fn fdt_dtc_check_content() {
        const MAX_CPUS: usize = 2;
        const BUF_SIZE: usize = 0x1000;

        // Rust cannot infer the type.
        let dtb_data_spans: [(usize, &[u8]); 2] = [
            (
                /* Span starts at offset */ 0,
                b"\xd0\x0d\xfe\xed\x00\x00\x10\x00\x00\x00\x04\x38\x00\x00\x00\x38\
                \x00\x00\x00\x28\x00\x00\x00\x11\x00\x00\x00\x10\x00\x00\x00\x00\
                \x00\x00\x00\x4a\x00\x00\x01\x6c\x00\x00\x00\x00\x00\x00\x00\x00\
                \x00\x00\x00\x00\x00\x00\x00\x00\x23\x61\x64\x64\x72\x65\x73\x73\
                \x2d\x63\x65\x6c\x6c\x73\x00\x23\x73\x69\x7a\x65\x2d\x63\x65\x6c\
                \x6c\x73\x00\x6d\x6f\x64\x65\x6c\x00\x72\x65\x67\x00\x64\x65\x76\
                \x69\x63\x65\x5f\x74\x79\x70\x65\x00\x73\x74\x61\x74\x75\x73\x00\
                \x63\x6f\x6d\x70\x61\x74\x69\x62\x6c\x65\x00\x72\x61\x6e\x67\x65\
                \x73",
            ),
            (
                /* Span starts at offset */ 0x430,
                b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\
                \x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x02\
                \x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x0f\x00\x00\x00\x00\
                \x00\x00\x00\x03\x00\x00\x00\x0f\x00\x00\x00\x1b\x6d\x73\x66\x74\
                \x2c\x75\x6e\x64\x65\x72\x68\x69\x6c\x6c\x00\x00\x00\x00\x00\x01\
                \x63\x70\x75\x73\x00\x00\x00\x00\x00\x00\x00\x03\x00\x00\x00\x04\
                \x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x03\x00\x00\x00\x04\
                \x00\x00\x00\x0f\x00\x00\x00\x00\x00\x00\x00\x01\x63\x70\x75\x40\
                \x30\x00\x00\x00\x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x25\
                \x63\x70\x75\x00\x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x21\
                \x00\x00\x00\x00\x00\x00\x00\x03\x00\x00\x00\x05\x00\x00\x00\x31\
                \x6f\x6b\x61\x79\x00\x00\x00\x00\x00\x00\x00\x02\x00\x00\x00\x01\
                \x63\x70\x75\x40\x31\x00\x00\x00\x00\x00\x00\x03\x00\x00\x00\x04\
                \x00\x00\x00\x25\x63\x70\x75\x00\x00\x00\x00\x03\x00\x00\x00\x04\
                \x00\x00\x00\x21\x00\x00\x00\x01\x00\x00\x00\x03\x00\x00\x00\x05\
                \x00\x00\x00\x31\x6f\x6b\x61\x79\x00\x00\x00\x00\x00\x00\x00\x02\
                \x00\x00\x00\x02\x00\x00\x00\x01\x76\x6d\x62\x75\x73\x00\x00\x00\
                \x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x02\
                \x00\x00\x00\x03\x00\x00\x00\x04\x00\x00\x00\x0f\x00\x00\x00\x01\
                \x00\x00\x00\x03\x00\x00\x00\x0b\x00\x00\x00\x38\x6d\x73\x66\x74\
                \x2c\x76\x6d\x62\x75\x73\x00\x00\x00\x00\x00\x03\x00\x00\x00\x14\
                \x00\x00\x00\x43\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x0f\
                \xf0\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x02\x00\x00\x00\x02\
                \x00\x00\x00\x09",
            ),
        ];

        let mut sample_buf = [0u8; BUF_SIZE];
        for (span_start, bytes) in dtb_data_spans {
            sample_buf[span_start..span_start + bytes.len()].copy_from_slice(bytes);
        }

        let mut buf = [0u8; BUF_SIZE];
        write_dt(
            &mut buf,
            &new_partition_info(MAX_CPUS),
            &AddressSpaceManager::new_const(),
            [],
            0..0,
            &ArrayString::from("test").unwrap_or_default(),
            None,
            None,
            IsolationType::None,
        )
        .unwrap();

        assert!(sample_buf == buf);
    }

    // This test should be manually enabled when need to regenerate
    // the sample content above and validate spec compliance with `dtc`.
    // Before running the test, please install the DeviceTree compiler:
    // ```shell
    // sudo apt-get update && sudo apt-get install device-tree-compiler
    // ```
    #[test]
    #[ignore = "enabling the test requires installing additional software, \
                and developers will experience a break."]
    fn fdt_dtc_decompile() {
        const MAX_CPUS: usize = 2048;

        let mut buf = [0; 0x40000];
        write_dt(
            &mut buf,
            &new_partition_info(MAX_CPUS),
            &AddressSpaceManager::new_const(),
            [],
            0..0,
            &ArrayString::from("test").unwrap_or_default(),
            None,
            None,
            IsolationType::None,
        )
        .unwrap();

        let input_dtb_file_name = "openhcl_boot.dtb";
        let output_dts_file_name = "openhcl_boot.dts";
        std::fs::write(input_dtb_file_name, buf).unwrap();
        let success = std::process::Command::new("dtc")
            .args([input_dtb_file_name, "-I", "dtb", "-o", output_dts_file_name])
            .status()
            .unwrap()
            .success();
        assert!(success);
    }

    fn new_address_space_manager(
        ram: &[MemoryRange],
        bootshim_used: MemoryRange,
        persisted_range: MemoryRange,
        parameter_range: MemoryRange,
        reclaim: Option<MemoryRange>,
    ) -> AddressSpaceManager {
        let ram = ram
            .iter()
            .cloned()
            .map(|range| MemoryEntry {
                range,
                mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                vnode: 0,
            })
            .collect::<Vec<_>>();
        let mut address_space = AddressSpaceManager::new_const();
        AddressSpaceManagerBuilder::new(
            &mut address_space,
            &ram,
            bootshim_used,
            persisted_range,
            subtract_ranges([parameter_range], reclaim),
        )
        .init()
        .unwrap();
        address_space
    }

    fn check_e820(boot_params: &boot_params, ext: &E820Ext, expected: &[(Range<u64>, u32)]) {
        let actual = boot_params.e820_map[..boot_params.e820_entries as usize]
            .iter()
            .chain(
                ext.entries
                    .iter()
                    .take((ext.header.len as usize) / size_of::<e820entry>()),
            );

        assert_eq!(actual.clone().count(), expected.len());

        for (actual, (expected_range, expected_type)) in actual.zip(expected.iter()) {
            let addr: u64 = actual.addr.into();
            let size: u64 = actual.size.into();
            let typ: u32 = actual.typ.into();
            assert_eq!(addr, expected_range.start);
            assert_eq!(size, expected_range.end - expected_range.start);
            assert_eq!(typ, *expected_type);
        }
    }

    const PAGE_SIZE: u64 = 0x1000;
    const ONE_MB: u64 = 0x10_0000;

    #[test]
    fn test_e820_basic() {
        // memmap with no param reclaim
        let mut boot_params: boot_params = FromZeros::new_zeroed();
        let mut ext = FromZeros::new_zeroed();
        let bootshim_used = MemoryRange::try_new(ONE_MB..3 * ONE_MB).unwrap();
        let persisted_header_end = ONE_MB + PAGE_SIZE;
        let persisted_end = ONE_MB + 4 * PAGE_SIZE;
        let persisted_state = MemoryRange::try_new(ONE_MB..persisted_end).unwrap();
        let parameter_range = MemoryRange::try_new(2 * ONE_MB..3 * ONE_MB).unwrap();
        let address_space = new_address_space_manager(
            &[MemoryRange::new(ONE_MB..4 * ONE_MB)],
            bootshim_used,
            persisted_state,
            parameter_range,
            None,
        );

        assert!(build_e820_map(&mut boot_params, &mut ext, &address_space).is_ok());

        check_e820(
            &boot_params,
            &ext,
            &[
                (ONE_MB..(persisted_header_end), E820_RESERVED),
                (persisted_header_end..persisted_end, E820_RESERVED),
                (persisted_end..2 * ONE_MB, E820_RAM),
                (2 * ONE_MB..3 * ONE_MB, E820_RESERVED),
                (3 * ONE_MB..4 * ONE_MB, E820_RAM),
            ],
        );

        // memmap with reclaim
        let mut boot_params: boot_params = FromZeros::new_zeroed();
        let mut ext = FromZeros::new_zeroed();
        let bootshim_used = MemoryRange::try_new(ONE_MB..5 * ONE_MB).unwrap();
        let persisted_header_end = ONE_MB + PAGE_SIZE;
        let persisted_end = ONE_MB + 4 * PAGE_SIZE;
        let persisted_state = MemoryRange::try_new(ONE_MB..persisted_end).unwrap();
        let parameter_range = MemoryRange::try_new(2 * ONE_MB..5 * ONE_MB).unwrap();
        let reclaim = MemoryRange::try_new(3 * ONE_MB..4 * ONE_MB).unwrap();
        let address_space = new_address_space_manager(
            &[MemoryRange::new(ONE_MB..6 * ONE_MB)],
            bootshim_used,
            persisted_state,
            parameter_range,
            Some(reclaim),
        );

        assert!(build_e820_map(&mut boot_params, &mut ext, &address_space).is_ok());

        check_e820(
            &boot_params,
            &ext,
            &[
                (ONE_MB..(persisted_header_end), E820_RESERVED),
                (persisted_header_end..persisted_end, E820_RESERVED),
                (persisted_end..2 * ONE_MB, E820_RAM),
                (2 * ONE_MB..3 * ONE_MB, E820_RESERVED),
                (3 * ONE_MB..4 * ONE_MB, E820_RAM),
                (4 * ONE_MB..5 * ONE_MB, E820_RESERVED),
                (5 * ONE_MB..6 * ONE_MB, E820_RAM),
            ],
        );

        // two mem ranges
        let mut boot_params: boot_params = FromZeros::new_zeroed();
        let mut ext = FromZeros::new_zeroed();
        let bootshim_used = MemoryRange::try_new(ONE_MB..5 * ONE_MB).unwrap();
        let persisted_header_end = ONE_MB + PAGE_SIZE;
        let persisted_end = ONE_MB + 4 * PAGE_SIZE;
        let persisted_state = MemoryRange::try_new(ONE_MB..persisted_end).unwrap();
        let parameter_range = MemoryRange::try_new(2 * ONE_MB..5 * ONE_MB).unwrap();
        let reclaim = MemoryRange::try_new(3 * ONE_MB..4 * ONE_MB).unwrap();
        let address_space = new_address_space_manager(
            &[
                MemoryRange::new(ONE_MB..4 * ONE_MB),
                MemoryRange::new(4 * ONE_MB..10 * ONE_MB),
            ],
            bootshim_used,
            persisted_state,
            parameter_range,
            Some(reclaim),
        );

        assert!(build_e820_map(&mut boot_params, &mut ext, &address_space).is_ok());

        check_e820(
            &boot_params,
            &ext,
            &[
                (ONE_MB..(persisted_header_end), E820_RESERVED),
                (persisted_header_end..persisted_end, E820_RESERVED),
                (persisted_end..2 * ONE_MB, E820_RAM),
                (2 * ONE_MB..3 * ONE_MB, E820_RESERVED),
                (3 * ONE_MB..4 * ONE_MB, E820_RAM),
                (4 * ONE_MB..5 * ONE_MB, E820_RESERVED),
                (5 * ONE_MB..10 * ONE_MB, E820_RAM),
            ],
        );

        // memmap in 1 mb chunks
        let mut boot_params: boot_params = FromZeros::new_zeroed();
        let mut ext = FromZeros::new_zeroed();
        let bootshim_used = MemoryRange::try_new(ONE_MB..5 * ONE_MB).unwrap();
        let persisted_header_end = ONE_MB + PAGE_SIZE;
        let persisted_end = ONE_MB + 4 * PAGE_SIZE;
        let persisted_state = MemoryRange::try_new(ONE_MB..persisted_end).unwrap();
        let parameter_range = MemoryRange::try_new(2 * ONE_MB..5 * ONE_MB).unwrap();
        let reclaim = MemoryRange::try_new(3 * ONE_MB..4 * ONE_MB).unwrap();
        let address_space = new_address_space_manager(
            &[
                MemoryRange::new(ONE_MB..2 * ONE_MB),
                MemoryRange::new(2 * ONE_MB..3 * ONE_MB),
                MemoryRange::new(3 * ONE_MB..4 * ONE_MB),
                MemoryRange::new(4 * ONE_MB..5 * ONE_MB),
                MemoryRange::new(5 * ONE_MB..6 * ONE_MB),
                MemoryRange::new(6 * ONE_MB..7 * ONE_MB),
                MemoryRange::new(7 * ONE_MB..8 * ONE_MB),
            ],
            bootshim_used,
            persisted_state,
            parameter_range,
            Some(reclaim),
        );

        assert!(build_e820_map(&mut boot_params, &mut ext, &address_space).is_ok());

        check_e820(
            &boot_params,
            &ext,
            &[
                (ONE_MB..(persisted_header_end), E820_RESERVED),
                (persisted_header_end..persisted_end, E820_RESERVED),
                (persisted_end..2 * ONE_MB, E820_RAM),
                (2 * ONE_MB..3 * ONE_MB, E820_RESERVED),
                (3 * ONE_MB..4 * ONE_MB, E820_RAM),
                (4 * ONE_MB..5 * ONE_MB, E820_RESERVED),
                (5 * ONE_MB..8 * ONE_MB, E820_RAM),
            ],
        );
    }

    // test e820 with spillover into ext
    #[test]
    fn test_e820_huge() {
        use crate::memory::AllocationPolicy;
        use crate::memory::AllocationType;

        // Create 64 RAM ranges, then allocate 256 ranges to test spillover
        // boot_params.e820_map has E820_MAX_ENTRIES_ZEROPAGE (128) entries
        const E820_MAX_ENTRIES_ZEROPAGE: usize = 128;
        const RAM_RANGES: usize = 64;
        const TOTAL_ALLOCATIONS: usize = 256;

        // Create 64 large RAM ranges (64MB each = 64 * 1MB pages per range)
        let mut ranges = Vec::new();
        for i in 0..RAM_RANGES {
            let start = (i as u64) * 64 * ONE_MB;
            let end = start + 64 * ONE_MB;
            ranges.push(MemoryRange::new(start..end));
        }

        let bootshim_used = MemoryRange::try_new(0..ONE_MB * 2).unwrap();
        let persisted_range = MemoryRange::try_new(0..ONE_MB).unwrap();
        let parameter_range = MemoryRange::try_new(ONE_MB..2 * ONE_MB).unwrap();

        let mut address_space = {
            let ram = ranges
                .iter()
                .cloned()
                .map(|range| MemoryEntry {
                    range,
                    mem_type: MemoryMapEntryType::VTL2_PROTECTABLE,
                    vnode: 0,
                })
                .collect::<Vec<_>>();
            let mut address_space = AddressSpaceManager::new_const();
            AddressSpaceManagerBuilder::new(
                &mut address_space,
                &ram,
                bootshim_used,
                persisted_range,
                core::iter::once(parameter_range),
            )
            .init()
            .unwrap();
            address_space
        };

        for i in 0..TOTAL_ALLOCATIONS {
            // Intersperse sidecar node allocations with gpa pool allocations,
            // as otherwise the address space manager will collapse adjacent
            // ranges of the same type.
            let _allocated = address_space
                .allocate(
                    None,
                    ONE_MB,
                    if i % 2 == 0 {
                        AllocationType::GpaPool
                    } else {
                        AllocationType::SidecarNode
                    },
                    AllocationPolicy::LowMemory,
                )
                .expect("should be able to allocate sidecar node");
        }

        let mut boot_params: boot_params = FromZeros::new_zeroed();
        let mut ext = FromZeros::new_zeroed();
        let total_ranges = address_space.vtl2_ranges().count();

        let used_ext = build_e820_map(&mut boot_params, &mut ext, &address_space).unwrap();

        // Verify that we used the extension
        assert!(used_ext, "should use extension when there are many ranges");

        // Verify the standard e820_map is full
        assert_eq!(boot_params.e820_entries, E820_MAX_ENTRIES_ZEROPAGE as u8);

        // Verify the extension has the overflow entries
        let ext_entries = (ext.header.len as usize) / size_of::<e820entry>();
        assert_eq!(ext_entries, total_ranges - E820_MAX_ENTRIES_ZEROPAGE);

        // Verify we have the expected number of total ranges
        let total_e820_entries = boot_params.e820_entries as usize + ext_entries;
        assert_eq!(total_e820_entries, total_ranges);
    }
}
