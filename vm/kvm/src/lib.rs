// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(target_os = "linux")]
// UNSAFETY: Calling KVM APIs and IOCTLs and dealing with the raw pointers
// necessary for doing so.
#![expect(unsafe_code)]

pub use kvm_bindings::kvm_ioeventfd_flag_nr_datamatch;
pub use kvm_bindings::kvm_ioeventfd_flag_nr_deassign;
pub use kvm_bindings::*;
use pal::unix::pthread::*;
use parking_lot::RwLock;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::os::unix::prelude::*;
use std::sync::Once;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use thiserror::Error;

mod ioctl {
    use kvm_bindings::*;
    use nix::ioctl_read;
    use nix::ioctl_readwrite;
    use nix::ioctl_write_int_bad;
    use nix::ioctl_write_ptr;
    use nix::request_code_none;
    const KVMIO: u8 = 0xae;
    ioctl_write_int_bad!(kvm_create_vm, request_code_none!(KVMIO, 0x1));
    ioctl_write_int_bad!(kvm_check_extension, request_code_none!(KVMIO, 0x03));
    ioctl_write_int_bad!(kvm_get_vcpu_mmap_size, request_code_none!(KVMIO, 0x04));
    #[cfg(target_arch = "x86_64")]
    ioctl_readwrite!(kvm_get_supported_cpuid, KVMIO, 0x05, kvm_cpuid2);
    ioctl_write_int_bad!(kvm_create_vcpu, request_code_none!(KVMIO, 0x41));
    ioctl_write_ptr!(
        kvm_set_user_memory_region,
        KVMIO,
        0x46,
        kvm_userspace_memory_region
    );
    ioctl_write_ptr!(kvm_irq_line, KVMIO, 0x61, kvm_irq_level);
    ioctl_write_ptr!(kvm_set_gsi_routing, KVMIO, 0x6a, kvm_irq_routing);
    ioctl_write_ptr!(kvm_irqfd, KVMIO, 0x76, kvm_irqfd);
    ioctl_write_int_bad!(kvm_set_boot_cpu_id, request_code_none!(KVMIO, 0x78));
    ioctl_read!(kvm_get_clock, KVMIO, 0x7c, kvm_clock_data);
    ioctl_write_int_bad!(kvm_run, request_code_none!(KVMIO, 0x80));
    // Is *NOT* defined for arm64
    #[cfg(not(target_arch = "aarch64"))]
    ioctl_read!(kvm_get_regs, KVMIO, 0x81, kvm_regs);
    // Is *NOT* defined for arm64
    #[cfg(not(target_arch = "aarch64"))]
    ioctl_write_ptr!(kvm_set_regs, KVMIO, 0x82, kvm_regs);
    ioctl_read!(kvm_get_sregs, KVMIO, 0x83, kvm_sregs);
    ioctl_write_ptr!(kvm_set_sregs, KVMIO, 0x84, kvm_sregs);
    ioctl_readwrite!(kvm_translation, KVMIO, 0x85, kvm_translation);
    ioctl_write_ptr!(kvm_interrupt, KVMIO, 0x86, kvm_interrupt);
    #[cfg(target_arch = "x86_64")]
    ioctl_readwrite!(kvm_get_msrs, KVMIO, 0x88, kvm_msrs);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_msrs, KVMIO, 0x89, kvm_msrs);
    ioctl_write_ptr!(kvm_set_signal_mask, KVMIO, 0x8b, kvm_signal_mask);
    ioctl_read!(kvm_get_fpu, KVMIO, 0x8c, kvm_fpu);
    ioctl_write_ptr!(kvm_set_fpu, KVMIO, 0x8d, kvm_fpu);
    #[cfg(target_arch = "x86_64")]
    ioctl_read!(kvm_get_lapic, KVMIO, 0x8e, kvm_lapic_state);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_lapic, KVMIO, 0x8f, kvm_lapic_state);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_cpuid2, KVMIO, 0x90, kvm_cpuid2);
    ioctl_read!(kvm_get_mp_state, KVMIO, 0x98, kvm_mp_state);
    ioctl_write_ptr!(kvm_set_mp_state, KVMIO, 0x99, kvm_mp_state);
    ioctl_read!(kvm_get_vcpu_events, KVMIO, 0x9f, kvm_vcpu_events);
    ioctl_write_ptr!(kvm_set_vcpu_events, KVMIO, 0xa0, kvm_vcpu_events);
    #[cfg(target_arch = "x86_64")]
    ioctl_read!(kvm_get_debugregs, KVMIO, 0xa1, kvm_debugregs);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_debugregs, KVMIO, 0xa2, kvm_debugregs);
    ioctl_write_ptr!(kvm_enable_cap, KVMIO, 0xa3, kvm_enable_cap);
    #[cfg(target_arch = "x86_64")]
    ioctl_read!(kvm_get_xsave, KVMIO, 0xa4, kvm_xsave);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_xsave, KVMIO, 0xa5, kvm_xsave);
    ioctl_write_ptr!(kvm_signal_msi, KVMIO, 0xa5, kvm_msi);
    #[cfg(target_arch = "x86_64")]
    ioctl_read!(kvm_get_xcrs, KVMIO, 0xa6, kvm_xcrs);
    #[cfg(target_arch = "x86_64")]
    ioctl_write_ptr!(kvm_set_xcrs, KVMIO, 0xa7, kvm_xcrs);
    ioctl_write_ptr!(kvm_get_reg, KVMIO, 0xab, kvm_one_reg);
    ioctl_write_ptr!(kvm_set_reg, KVMIO, 0xac, kvm_one_reg);
    #[cfg(target_arch = "aarch64")]
    ioctl_write_ptr!(kvm_arm_vcpu_init, KVMIO, 0xae, kvm_vcpu_init);
    #[cfg(target_arch = "aarch64")]
    ioctl_read!(kvm_arm_preferred_target, KVMIO, 0xaf, kvm_vcpu_init);
    ioctl_write_ptr!(kvm_ioeventfd, KVMIO, 0x79, kvm_ioeventfd);
    ioctl_write_ptr!(kvm_set_guest_debug, KVMIO, 0x9b, kvm_guest_debug);
    ioctl_readwrite!(kvm_create_device, KVMIO, 0xe0, kvm_create_device);
    ioctl_write_ptr!(kvm_set_device_attr, KVMIO, 0xe1, kvm_device_attr);
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("failed to open /dev/kvm")]
    OpenKvm(#[source] io::Error),
    #[error("SignalMsi")]
    SignalMsi(#[source] nix::Error),
    #[error("SetMemoryRegion")]
    SetMemoryRegion(#[source] nix::Error),
    #[error("CreateVm")]
    CreateVm(#[source] nix::Error),
    #[error("EnableCap({0})")]
    EnableCap(&'static str, #[source] nix::Error),
    #[error("CreateVCpu")]
    CreateVCpu(#[source] nix::Error),
    #[error("GetRegs")]
    GetRegs(#[source] nix::Error),
    #[error("GetSRegs")]
    GetSRegs(#[source] nix::Error),
    #[error("SetRegs")]
    SetRegs(#[source] nix::Error),
    #[error("SetSRegs")]
    SetSRegs(#[source] nix::Error),
    #[error("Run")]
    Run(#[source] nix::Error),
    #[error("GetVCpuMmapSize")]
    GetVCpuMmapSize(#[source] nix::Error),
    #[error("MmapVCpu")]
    MmapVCpu(#[source] io::Error),
    #[error("SetFpu")]
    SetFpu(#[source] nix::Error),
    #[error("GetSupportedCpuid")]
    GetSupportedCpuid(#[source] nix::Error),
    #[error("SetCpuid")]
    SetCpuid(#[source] nix::Error),
    #[error("Interrupt")]
    Interrupt(#[source] nix::Error),
    #[error("GetLApic")]
    GetLApic(#[source] nix::Error),
    #[error("SetLApic")]
    SetLApic(#[source] nix::Error),
    #[error("GetXsave")]
    GetXsave(#[source] nix::Error),
    #[error("SetXsave")]
    SetXsave(#[source] nix::Error),
    #[error("GetDebugRegs")]
    GetDebugRegs(#[source] nix::Error),
    #[error("SetDebugRegs")]
    SetDebugRegs(#[source] nix::Error),
    #[error("GetXcrs")]
    GetXcrs(#[source] nix::Error),
    #[error("SetXcrs")]
    SetXcrs(#[source] nix::Error),
    #[error("xsave is not enabled")]
    XsaveNotEnabled,
    #[error("SetGsiRouting")]
    SetGsiRouting(#[source] nix::Error),
    #[error("IrqLine")]
    IrqLine(#[source] nix::Error),
    #[error("GetMsrs")]
    GetMsrs(#[source] nix::Error),
    #[error("SetMsrs")]
    SetMsrs(#[source] nix::Error),
    #[error("GetMpState")]
    GetMpState(#[source] nix::Error),
    #[error("SetMpState")]
    SetMpState(#[source] nix::Error),
    #[error("GetVcpuEvents")]
    GetVcpuEvents(#[source] nix::Error),
    #[error("SetVcpuEvents")]
    SetVcpuEvents(#[source] nix::Error),
    #[error("TranslateGva")]
    TranslateGva(#[source] nix::Error),
    #[error("unknown exit {0:#x}")]
    UnknownExit(u32),
    #[error("unknown Hyper-V exit {0:#x}")]
    UnknownHvExit(u32),
    #[error("ioeventfd")]
    IoEventFd(#[source] nix::Error),
    #[error("irqfd")]
    IrqFd(#[source] nix::Error),
    #[error("failed to set BSP")]
    SetBsp(#[source] nix::Error),
    #[error("CreateDevice")]
    CreateDevice(#[source] nix::Error),
    #[error("SetDeviceAttr")]
    SetDeviceAttr(#[source] nix::Error),
}

type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
struct Vp {
    vcpu: File,
    run_data: VpPtr,
    thread: RwLock<Option<Pthread>>,
    _phantom: PhantomData<kvm_run>,
}

/// Send+Sync wrapper around the mapped kvm_run pointer.
#[derive(Debug)]
struct VpPtr {
    ptr: *mut kvm_run,
    len: usize,
}

// SAFETY: this type contains a pointer to mapped data. By itself this is
// Send+Sync since it's just a raw pointer value with no methods, but in context
// it must be carefully accessed only by one thread at a time. This is mediated
// by `Vp`.
unsafe impl Send for VpPtr {}
// SAFETY: see above comment
unsafe impl Sync for VpPtr {}

/// An open file to `/dev/kvm`.
#[derive(Debug)]
pub struct Kvm(File);

impl Kvm {
    /// Opens `/dev/kvm`.
    pub fn new() -> Result<Self> {
        let kvm = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(Error::OpenKvm)?;

        Ok(Self(kvm))
    }

    /// Returns the CPUID values that are supported by the hypervisor.
    #[cfg(target_arch = "x86_64")]
    pub fn supported_cpuid(&self) -> Result<Vec<kvm_cpuid_entry2>> {
        const MAX_CPUID_ENTRIES: usize = 256;
        let mut supported_cpuid = Cpuid {
            cpuid: kvm_cpuid2 {
                nent: MAX_CPUID_ENTRIES as u32,
                ..Default::default()
            },
            entries: [Default::default(); MAX_CPUID_ENTRIES],
        };

        // TODO: We are not checking for KVM_CAP_EXT_CPUID first.
        // SAFETY: We have allocated an array for the ioctl to write to and correctly specified its size in nent.
        unsafe {
            ioctl::kvm_get_supported_cpuid(self.as_fd().as_raw_fd(), &mut supported_cpuid.cpuid)
                .map_err(Error::GetSupportedCpuid)?;
        }

        Ok(supported_cpuid.entries[..supported_cpuid.cpuid.nent as usize].to_vec())
    }

    pub fn check_extension(&self, extension: u32) -> nix::Result<libc::c_int> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe { ioctl::kvm_check_extension(self.as_fd().as_raw_fd(), extension as i32) }
    }

    pub fn new_vm(&self) -> Result<Partition> {
        // On ARM, can request memory isolation which we don't use.
        // For that, include the `KVM_VM_TYPE_ARM_PROTECTED` flag.
        // Use 0 as the fallback machine type, which implies 40bit
        // IPA on ARM64, and on x86_64 is the only option.
        let vm_type = self.check_extension(KVM_CAP_ARM_VM_IPA_SIZE).unwrap_or(0);

        // SAFETY: Calling IOCTL as documented, with no special requirements.
        let vm = unsafe {
            let fd =
                ioctl::kvm_create_vm(self.as_fd().as_raw_fd(), vm_type).map_err(Error::CreateVm)?;
            File::from_raw_fd(fd)
        };

        // TODO: We are not checking KVM_CAP_ENABLE_CAP_VM first.
        // TODO: We are not calling KVM_CHECK_EXTENSION first.
        // SAFETY: Calling IOCTLs as documented, with no special requirements.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            // Disable quirks to make KVM behave more architecturally correct.
            // TODO: Investigate using KVM_CAP_DISABLE_QUIRKS2 instead.
            ioctl::kvm_enable_cap(
                vm.as_raw_fd(),
                &kvm_enable_cap {
                    cap: KVM_CAP_DISABLE_QUIRKS,
                    args: [KVM_X86_QUIRK_LINT0_REENABLED.into(), 0, 0, 0],
                    ..Default::default()
                },
            )
            .map_err(|err| Error::EnableCap("disable_quirks", err))?;
        }

        // SAFETY: Calling IOCTL as documented, with no special requirements.
        let mmap_size = unsafe {
            ioctl::kvm_get_vcpu_mmap_size(self.as_fd().as_raw_fd(), 0)
                .map_err(Error::GetVCpuMmapSize)? as usize
        };

        Ok(Partition {
            vm,
            vps: Vec::new(),
            mmap_size,
        })
    }
}

impl AsFd for Kvm {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[repr(C)]
#[cfg(target_arch = "x86_64")]
struct Cpuid {
    cpuid: kvm_cpuid2,
    entries: [kvm_cpuid_entry2; 256],
}

#[derive(Debug)]
pub struct Partition {
    vm: File,
    vps: Vec<Option<Vp>>,
    mmap_size: usize,
}

impl Partition {
    pub fn enable_split_irqchip(&self, lines: u32) -> Result<()> {
        // TODO: We are not checking KVM_CAP_ENABLE_CAP_VM first.
        // TODO: We are not calling KVM_CHECK_EXTENSION first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_enable_cap(
                self.vm.as_raw_fd(),
                &kvm_enable_cap {
                    cap: KVM_CAP_SPLIT_IRQCHIP,
                    args: [lines.into(), 0, 0, 0],
                    ..Default::default()
                },
            )
            .map_err(|err| Error::EnableCap("split_irqchip", err))?;
        }
        Ok(())
    }

    /// Enable X2APIC IDs in interrupt and LAPIC APIs.
    #[cfg(target_arch = "x86_64")]
    pub fn enable_x2apic_api(&self) -> Result<()> {
        let flags = KVM_X2APIC_API_USE_32BIT_IDS;
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_enable_cap(
                self.vm.as_raw_fd(),
                &kvm_enable_cap {
                    cap: KVM_CAP_X2APIC_API,
                    args: [flags.into(), 0, 0, 0],
                    ..Default::default()
                },
            )
            .map_err(|err| Error::EnableCap("x2apic_api", err))?;
        }
        Ok(())
    }

    pub fn enable_unknown_msr_exits(&self) -> Result<()> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        // TODO: We are not checking KVM_CAP_ENABLE_CAP_VM first.
        unsafe {
            ioctl::kvm_enable_cap(
                self.vm.as_raw_fd(),
                &kvm_enable_cap {
                    cap: KVM_CAP_X86_USER_SPACE_MSR,
                    args: [KVM_MSR_EXIT_REASON_UNKNOWN.into(), 0, 0, 0],
                    ..Default::default()
                },
            )
            .map_err(|err| Error::EnableCap("user_space_msr", err))?;
        }
        Ok(())
    }

    /// Set the VCPU index of the BSP. This must be called before any VCPUs are
    /// created.
    #[cfg(target_arch = "x86_64")]
    pub fn set_bsp(&mut self, vcpu_idx: u32) -> Result<()> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_boot_cpu_id(self.vm.as_raw_fd(), vcpu_idx as i32)
                .map_err(Error::SetBsp)?;
        }

        Ok(())
    }

    pub fn add_vp(&mut self, vcpu_idx: u32) -> Result<()> {
        // TODO: We are not checking KVM_CAP_NR_VCPUS or KVM_CAP_MAX_VCPUS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        let vcpu = unsafe {
            let fd = ioctl::kvm_create_vcpu(self.vm.as_raw_fd(), vcpu_idx as i32)
                .map_err(Error::CreateVCpu)?;
            File::from_raw_fd(fd)
        };

        // SAFETY: Calling mmap with a null pointer is valid, and vcpu is guaranteed to have a valid fd.
        let ptr = unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                self.mmap_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                vcpu.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(Error::MmapVCpu(io::Error::last_os_error()));
            }
            ptr
        };

        #[cfg(target_arch = "aarch64")]
        {
            // Can request additional features like so:
            let mut kvi = kvm_vcpu_init::default();
            kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;

            if vcpu_idx > 0 {
                kvi.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
            }

            let mut pref_target = kvm_vcpu_init::default();
            // SAFETY: Calling IOCTL as documented, with no special requirements.
            unsafe {
                ioctl::kvm_arm_preferred_target(self.vm.as_raw_fd(), &mut pref_target)
                    .map_err(Error::CreateVCpu)?
            };

            kvi.target = pref_target.target;
            // SAFETY: Calling IOCTL as documented, with no special requirements.
            unsafe { ioctl::kvm_arm_vcpu_init(vcpu.as_raw_fd(), &kvi).map_err(Error::CreateVCpu)? };
        }

        let vp = Vp {
            vcpu,
            run_data: VpPtr {
                ptr: ptr.cast(),
                len: self.mmap_size,
            },
            thread: RwLock::new(None),
            _phantom: PhantomData,
        };
        if self.vps.len() <= vcpu_idx as usize {
            self.vps.resize_with(vcpu_idx as usize + 1, || None);
        }
        assert!(self.vps[vcpu_idx as usize].replace(vp).is_none());

        Ok(())
    }

    pub fn vp(&self, index: u32) -> Processor<'_> {
        Processor(self, index)
    }

    pub fn request_msi(&self, msi: &kvm_msi) -> Result<()> {
        // TODO: We are not checking KVM_CAP_SIGNAL_MSI first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_signal_msi(self.vm.as_raw_fd(), msi).map_err(Error::SignalMsi)?;
        }
        Ok(())
    }

    #[expect(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]
    pub unsafe fn set_user_memory_region(
        &self,
        slot: u32,
        data: *mut u8,
        size: usize,
        addr: u64,
        readonly: bool,
    ) -> Result<()> {
        let region = kvm_userspace_memory_region {
            slot,
            flags: if readonly { KVM_MEM_READONLY } else { 0 },
            guest_phys_addr: addr,
            memory_size: size as u64,
            userspace_addr: data as usize as u64,
        };
        unsafe {
            ioctl::kvm_set_user_memory_region(self.vm.as_raw_fd(), &region)
                .map_err(Error::SetMemoryRegion)?;
        }
        Ok(())
    }

    pub fn set_gsi_routes(&self, routes: &[(u32, RoutingEntry)]) -> Result<()> {
        const MAX_ROUTES: usize = 2048;
        assert!(routes.len() <= MAX_ROUTES);

        #[repr(C)]
        struct Routes {
            header: kvm_irq_routing,
            entries: [kvm_irq_routing_entry; MAX_ROUTES],
        }

        let mut kvm_routes = Routes {
            header: Default::default(),
            entries: [Default::default(); MAX_ROUTES],
        };
        for (i, route) in routes.iter().enumerate() {
            let (type_, u) = match route.1 {
                RoutingEntry::Msi {
                    address_lo,
                    address_hi,
                    data,
                } => (
                    KVM_IRQ_ROUTING_MSI,
                    kvm_irq_routing_entry__bindgen_ty_1 {
                        msi: kvm_irq_routing_msi {
                            address_lo,
                            address_hi,
                            data,
                            __bindgen_anon_1: Default::default(),
                        },
                    },
                ),
                RoutingEntry::HvSint { vp, sint } => (
                    KVM_IRQ_ROUTING_HV_SINT,
                    kvm_irq_routing_entry__bindgen_ty_1 {
                        hv_sint: kvm_irq_routing_hv_sint {
                            vcpu: vp,
                            sint: sint.into(),
                        },
                    },
                ),
                RoutingEntry::Irqchip { pin } => (
                    KVM_IRQ_ROUTING_IRQCHIP,
                    kvm_irq_routing_entry__bindgen_ty_1 {
                        irqchip: kvm_irq_routing_irqchip { pin, irqchip: 0 },
                    },
                ),
            };
            kvm_routes.entries[i] = kvm_irq_routing_entry {
                gsi: route.0,
                type_,
                flags: 0,
                pad: 0,
                u,
            };
            kvm_routes.header.nr += 1;
        }

        // TODO: We are not checking KVM_CAP_IRQ_ROUTING first.
        // SAFETY: Our Routes type puts the entries array immediately after the header in memory, as required.
        unsafe {
            ioctl::kvm_set_gsi_routing(self.vm.as_raw_fd(), &kvm_routes.header)
                .map_err(Error::SetGsiRouting)?;
        }
        Ok(())
    }

    pub fn irqfd(&self, gsi: u32, event: RawFd, assign: bool) -> Result<()> {
        // TODO: We are not checking KVM_CAP_IRQFD first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_irqfd(
                self.vm.as_raw_fd(),
                &kvm_irqfd {
                    fd: event as u32,
                    gsi,
                    flags: if assign { 0 } else { KVM_IRQFD_FLAG_DEASSIGN },
                    resamplefd: 0,
                    pad: [0; 16],
                },
            )
            .map_err(Error::IrqFd)
            .map(drop)
        }
    }

    pub fn irq_line(&self, gsi: u32, level: bool) -> Result<()> {
        // TODO: We are not checking KVM_CAP_IRQCHIP first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_irq_line(
                self.vm.as_raw_fd(),
                &kvm_irq_level {
                    __bindgen_anon_1: kvm_irq_level__bindgen_ty_1 { irq: gsi },
                    level: level.into(),
                },
            )
            .map_err(Error::IrqLine)?;
        }
        Ok(())
    }

    pub fn ioeventfd(
        &self,
        datamatch: u64,
        addr: u64,
        len: u32,
        fd: i32,
        flags: u32,
    ) -> Result<()> {
        // TODO: We are not checking KVM_CAP_IOEVENTFD first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_ioeventfd(
                self.vm.as_raw_fd(),
                &kvm_ioeventfd {
                    datamatch,
                    addr,
                    len,
                    fd,
                    flags,
                    ..Default::default()
                },
            )
            .map_err(Error::IoEventFd)?;
        };
        Ok(())
    }

    pub fn create_device(&self, ty: u32, flags: u32) -> nix::Result<Device> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        // The reference: https://www.kernel.org/doc/html/latest/virt/kvm/api.html#kvm-create-device.
        // The kernel checks on the input parameters and returns the appropriate
        // error code.
        unsafe {
            let mut device = kvm_create_device {
                type_: ty,
                fd: 0,
                flags,
            };
            ioctl::kvm_create_device(self.vm.as_raw_fd(), &mut device)?;
            Ok(Device(File::from_raw_fd(device.fd as i32)))
        }
    }

    /// Gets the current kvmclock value.
    pub fn get_clock_ns(&self) -> Result<kvm_clock_data> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        let mut clock = kvm_clock_data::default();
        unsafe {
            ioctl::kvm_get_clock(self.vm.as_raw_fd(), &mut clock).map_err(Error::GetRegs)?;
        }
        Ok(clock)
    }
}

/// An in-kernel emulated device.
pub struct Device(File);

impl Device {
    /// # Safety
    ///
    /// `addr` must point to the appropriate input for the attribute being
    /// set.
    pub unsafe fn set_device_attr<T>(
        &self,
        group: u32,
        attr: u32,
        addr: &T,
        flags: u32,
    ) -> nix::Result<()> {
        // SAFETY: caller guaranteed.
        unsafe {
            ioctl::kvm_set_device_attr(
                self.0.as_raw_fd(),
                &kvm_device_attr {
                    group,
                    attr: attr as u64,
                    addr: std::ptr::from_ref(addr) as u64,
                    flags,
                },
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RoutingEntry {
    Irqchip {
        pin: u32,
    },
    Msi {
        address_lo: u32,
        address_hi: u32,
        data: u32,
    },
    HvSint {
        vp: u32,
        sint: u8,
    },
}

pub struct Processor<'a>(&'a Partition, u32);

impl<'a> Processor<'a> {
    pub fn enable_synic(&self) -> Result<()> {
        // TODO: We are not checking KVM_CAP_ENABLE_CAP_VM first.
        // TODO: We are not calling KVM_CHECK_EXTENSION first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_enable_cap(
                self.get().vcpu.as_raw_fd(),
                &kvm_enable_cap {
                    cap: KVM_CAP_HYPERV_SYNIC2,
                    ..Default::default()
                },
            )
            .map_err(|err| Error::EnableCap("hyperv_synic2", err))?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_cpuid(&self, entries: &[kvm_cpuid_entry2]) -> Result<()> {
        const MAX_CPUID_ENTRIES: usize = 256;
        assert!(entries.len() <= MAX_CPUID_ENTRIES);

        let mut cpuid: Cpuid = Cpuid {
            cpuid: Default::default(),
            entries: [Default::default(); MAX_CPUID_ENTRIES],
        };
        for (i, e) in entries.iter().enumerate() {
            cpuid.entries[i] = *e;
            cpuid.cpuid.nent += 1;
        }

        // SAFETY: Our Cpuid type puts the entries array immediately after the header in memory, as required.
        unsafe {
            ioctl::kvm_set_cpuid2(self.get().vcpu.as_raw_fd(), &cpuid.cpuid)
                .map_err(Error::SetCpuid)?;
        }
        Ok(())
    }

    fn get(&self) -> &'a Vp {
        self.0.vps[self.1 as usize].as_ref().expect("vp exists")
    }

    /// Forces an exit to be returned from the next call to [`VpRunner::run`].
    ///
    /// Note that this does nothing if a [`VpRunner`] does not currently exist
    /// for this VP, or if this is called from the same thread as the runner.
    pub fn force_exit(&self) {
        let vp = self.get();
        let thread = vp.thread.read();
        if let Some(thread) = *thread {
            if thread != Pthread::current() {
                thread
                    .signal(libc::SIGRTMIN())
                    .expect("thread cancel signal failed");
            }
        }
    }

    pub fn interrupt(&self, vector: u32) -> Result<()> {
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_interrupt(self.get().vcpu.as_raw_fd(), &kvm_interrupt { irq: vector })
                .map_err(Error::Interrupt)?;
        };
        Ok(())
    }

    /// Very not structured way of setting the register. Could enjoy using an enum.
    pub fn set_reg64(&self, reg_id: u64, value: u64) -> Result<()> {
        let reg = kvm_one_reg {
            id: reg_id,
            addr: std::ptr::from_ref(&value) as u64,
        };
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_reg(self.get().vcpu.as_raw_fd(), &reg).map_err(Error::SetRegs)?;
        }
        Ok(())
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn set_regs(&self, regs: &kvm_regs) -> Result<()> {
        // This IOCTL does not work on arm64.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_regs(self.get().vcpu.as_raw_fd(), regs).map_err(Error::SetRegs)?;
        }
        Ok(())
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn set_sregs(&self, sregs: &kvm_sregs) -> Result<()> {
        // This IOCTL does not work on arm64.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_sregs(self.get().vcpu.as_raw_fd(), sregs).map_err(Error::SetRegs)?;
        }
        Ok(())
    }

    /// Very not structured way of getting the register. Could enjoy using an enum.
    pub fn get_reg64(&self, reg_id: u64) -> Result<u64> {
        let mut value: u64 = 0;
        let reg = kvm_one_reg {
            id: reg_id,
            addr: std::ptr::from_mut(&mut value) as u64,
        };
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_reg(self.get().vcpu.as_raw_fd(), &reg).map_err(Error::GetRegs)?;
        }

        Ok(value)
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn get_regs(&self) -> Result<kvm_regs> {
        let mut regs = Default::default();
        // This IOCTL does not work on arm64.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_regs(self.get().vcpu.as_raw_fd(), &mut regs).map_err(Error::GetRegs)?;
        }
        Ok(regs)
    }

    #[cfg(not(target_arch = "aarch64"))]
    pub fn get_sregs(&self) -> Result<kvm_sregs> {
        let mut sregs = Default::default();
        // This IOCTL does not work on arm64.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_sregs(self.get().vcpu.as_raw_fd(), &mut sregs)
                .map_err(Error::GetSRegs)?;
        }
        Ok(sregs)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_msrs(&self, msrs: &[u32], values: &mut [u64]) -> Result<()> {
        const MAX_MSR_ENTRIES: usize = 256;
        assert_eq!(msrs.len(), values.len());
        assert!(msrs.len() <= MAX_MSR_ENTRIES);

        #[repr(C)]
        struct Msrs {
            header: kvm_msrs,
            entries: [kvm_msr_entry; MAX_MSR_ENTRIES],
        }
        let mut input = Msrs {
            header: kvm_msrs {
                nmsrs: msrs.len() as u32,
                ..Default::default()
            },
            entries: [Default::default(); MAX_MSR_ENTRIES],
        };
        for (i, msr) in msrs.iter().enumerate() {
            input.entries[i] = kvm_msr_entry {
                index: *msr,
                reserved: 0,
                data: 0,
            };
        }

        // SAFETY: Our Msrs type puts the entries array immediately after the header in memory, as required.
        unsafe {
            ioctl::kvm_get_msrs(self.get().vcpu.as_raw_fd(), &mut input.header)
                .map_err(Error::GetMsrs)?;
        }
        for (v, e) in values.iter_mut().zip(&input.entries) {
            *v = e.data;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_msrs(&self, msrs: &[(u32, u64)]) -> Result<()> {
        const MAX_MSR_ENTRIES: usize = 256;
        assert!(msrs.len() <= MAX_MSR_ENTRIES);

        #[repr(C)]
        struct Msrs {
            header: kvm_msrs,
            entries: [kvm_msr_entry; MAX_MSR_ENTRIES],
        }
        let mut input = Msrs {
            header: kvm_msrs {
                nmsrs: msrs.len() as u32,
                ..Default::default()
            },
            entries: [Default::default(); MAX_MSR_ENTRIES],
        };
        for (i, msr) in msrs.iter().enumerate() {
            input.entries[i] = kvm_msr_entry {
                index: msr.0,
                reserved: 0,
                data: msr.1,
            };
        }

        // SAFETY: Our Msrs type puts the entries array immediately after the header in memory, as required.
        unsafe {
            ioctl::kvm_set_msrs(self.get().vcpu.as_raw_fd(), &input.header)
                .map_err(Error::SetMsrs)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_lapic(&self, state: &mut [u8; 1024]) -> Result<()> {
        assert_eq!(size_of_val(state), size_of::<kvm_lapic_state>());

        // TODO: We are not checking KVM_CAP_IRQCHIP first.
        // SAFETY: We have verified that our output buffer is the correct size.
        unsafe {
            ioctl::kvm_get_lapic(self.get().vcpu.as_raw_fd(), state.as_mut_ptr().cast())
                .map_err(Error::GetLApic)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_lapic(&self, state: &[u8; 1024]) -> Result<()> {
        assert_eq!(size_of_val(state), size_of::<kvm_lapic_state>());

        // TODO: We are not checking KVM_CAP_IRQCHIP first.
        // SAFETY: We have verified that our input buffer is the correct size.
        unsafe {
            ioctl::kvm_set_lapic(self.get().vcpu.as_raw_fd(), state.as_ptr().cast())
                .map_err(Error::SetLApic)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_xsave(&self, state: &mut [u8; 4096]) -> Result<()> {
        assert_eq!(size_of_val(state), size_of::<kvm_xsave>());

        // TODO: We are not checking KVM_CAP_XSAVE2 first.
        // SAFETY: We have verified that our output buffer is the correct size.
        unsafe {
            ioctl::kvm_get_xsave(self.get().vcpu.as_raw_fd(), state.as_mut_ptr().cast())
                .map_err(Error::GetXsave)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_xsave(&self, state: &[u8; 4096]) -> Result<()> {
        assert_eq!(size_of_val(state), size_of::<kvm_xsave>());

        // TODO: We are not checking KVM_CAP_XSAVE2 first.
        // SAFETY: We have verified that our input buffer is the correct size.
        unsafe {
            ioctl::kvm_set_xsave(self.get().vcpu.as_raw_fd(), state.as_ptr().cast())
                .map_err(Error::SetXsave)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_debug_regs(&self, regs: &DebugRegisters) -> Result<()> {
        let data = kvm_debugregs {
            db: regs.db,
            dr6: regs.dr6,
            dr7: regs.dr7,
            flags: 0,
            reserved: [0; 9],
        };

        // TODO: We are not checking KVM_CAP_DEBUGREGS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_debugregs(self.get().vcpu.as_raw_fd(), &data)
                .map_err(Error::SetDebugRegs)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_debug_regs(&self) -> Result<DebugRegisters> {
        let mut data = Default::default();

        // TODO: We are not checking KVM_CAP_DEBUGREGS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_debugregs(self.get().vcpu.as_raw_fd(), &mut data)
                .map_err(Error::GetDebugRegs)?;
        }

        Ok(DebugRegisters {
            db: data.db,
            dr6: data.dr6,
            dr7: data.dr7,
        })
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_xcr0(&self, value: u64) -> Result<()> {
        let mut data = kvm_xcrs {
            nr_xcrs: 1,
            ..Default::default()
        };
        data.xcrs[0] = kvm_xcr {
            xcr: 0,
            reserved: 0,
            value,
        };

        // TODO: We are not checking KVM_CAP_XCRS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_xcrs(self.get().vcpu.as_raw_fd(), &data).map_err(Error::GetXcrs)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_xcr0(&self) -> Result<u64> {
        let mut data = Default::default();

        // TODO: We are not checking KVM_CAP_XCRS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_xcrs(self.get().vcpu.as_raw_fd(), &mut data).map_err(Error::SetXcrs)?;
        }

        if data.nr_xcrs < 1 {
            return Err(Error::XsaveNotEnabled);
        }
        assert_eq!(data.nr_xcrs, 1);
        assert_eq!(data.xcrs[0].xcr, 0);
        Ok(data.xcrs[0].value)
    }

    pub fn set_mp_state(&self, state: u32) -> Result<()> {
        let state = kvm_mp_state { mp_state: state };
        // TODO: We are not checking KVM_CAP_MP_STATE first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_mp_state(self.get().vcpu.as_raw_fd(), &state)
                .map_err(Error::SetMpState)?;
        }
        Ok(())
    }

    pub fn get_mp_state(&self) -> Result<u32> {
        let mut state = Default::default();
        // TODO: We are not checking KVM_CAP_MP_STATE first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_mp_state(self.get().vcpu.as_raw_fd(), &mut state)
                .map_err(Error::GetMpState)?;
        }
        Ok(state.mp_state)
    }

    pub fn set_vcpu_events(&self, events: &kvm_vcpu_events) -> Result<()> {
        // TODO: We are not checking KVM_CAP_VCPU_EVENTS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_vcpu_events(self.get().vcpu.as_raw_fd(), events)
                .map_err(Error::SetVcpuEvents)?;
        }
        Ok(())
    }

    pub fn get_vcpu_events(&self) -> Result<kvm_vcpu_events> {
        let mut events = Default::default();
        // TODO: We are not checking KVM_CAP_VCPU_EVENTS first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_get_vcpu_events(self.get().vcpu.as_raw_fd(), &mut events)
                .map_err(Error::GetVcpuEvents)?;
        }
        Ok(events)
    }

    pub fn translate_gva(&self, gva: u64) -> Result<kvm_translation> {
        let mut translation = kvm_translation {
            linear_address: gva,
            ..Default::default()
        };

        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_translation(self.get().vcpu.as_raw_fd(), &mut translation)
                .map_err(Error::TranslateGva)?;
        }

        Ok(translation)
    }

    /// Sets the guest debugging state: `control` bits `KVM_GUESTDBG_*`, `db`
    /// containing DR0 through DR3, and `dr7`.
    #[cfg(target_arch = "x86_64")]
    pub fn set_guest_debug(&self, control: u32, db: [u64; 4], dr7: u64) -> Result<()> {
        // N.B. Debug registers 4 through 6 are not used by KVM in this path.
        let debug = kvm_guest_debug {
            control,
            pad: 0,
            arch: kvm_guest_debug_arch {
                debugreg: [db[0], db[1], db[2], db[3], 0, 0, 0, dr7],
            },
        };

        // TODO: We are not checking KVM_CAP_SET_GUEST_DEBUG first.
        // SAFETY: Calling IOCTL as documented, with no special requirements.
        unsafe {
            ioctl::kvm_set_guest_debug(self.get().vcpu.as_raw_fd(), &debug)
                .map_err(Error::GetRegs)?;
        }
        Ok(())
    }

    /// # Safety
    ///
    /// `addr` must point to the appropriate input for the attribute being
    /// set.
    pub unsafe fn set_device_attr<T>(
        &self,
        group: u32,
        attr: u32,
        addr: &T,
        flags: u32,
    ) -> nix::Result<libc::c_int> {
        // SAFETY: caller guaranteed.
        unsafe {
            ioctl::kvm_set_device_attr(
                self.get().vcpu.as_raw_fd(),
                &kvm_device_attr {
                    group,
                    attr: attr as u64,
                    addr: std::ptr::from_ref(addr) as u64,
                    flags,
                },
            )
        }
    }

    pub fn runner(&self) -> VpRunner<'a> {
        // Ensure this thread is uniquely running the VP, and store the thread
        // ID to support cancellation.
        assert!(
            self.get()
                .thread
                .write()
                .replace(Pthread::current())
                .is_none()
        );

        VpRunner {
            partition: self.0,
            idx: self.1,
            _not_send_sync: PhantomData,
        }
    }
}

pub struct VpRunner<'a> {
    partition: &'a Partition,
    idx: u32,
    // This type stores the current thread in `partition` and removes it in
    // `drop`, so don't allow sending or sharing this.
    _not_send_sync: PhantomData<*const u8>,
}

impl Drop for VpRunner<'_> {
    fn drop(&mut self) {
        // The thread is no longer in use.
        let thread = self.get().thread.write().take();
        assert_eq!(thread, Some(Pthread::current()));
    }
}

impl<'a> VpRunner<'a> {
    fn get(&self) -> &'a Vp {
        self.partition.vp(self.idx).get()
    }

    fn run_data(&mut self) -> &mut kvm_run {
        let vp = self.get();
        // SAFETY: there are no other references to this data right
        // now since this thread is uniquely processing the VP, and
        // the VP is not running (so the kernel is not mutating the
        // structure either).
        unsafe { &mut *vp.run_data.ptr }
    }

    fn run_data_slice(&mut self) -> &mut [u8] {
        let vp = self.get();
        // SAFETY: there are no other references to this data right
        // now since this thread is uniquely processing the VP, and
        // the VP is not running (so the kernel is not mutating the
        // structure either).
        unsafe { std::slice::from_raw_parts_mut(vp.run_data.ptr.cast::<u8>(), vp.run_data.len) }
    }

    /// Issues an IOCTL to run the VP.
    fn run_vp_once(&mut self) -> Result<bool> {
        CURRENT_KVM_RUN.with(|r| {
            let vp = self.get();

            // Clear immediate_exit before giving up exclusive ownership of the
            // kvm_run structure.
            self.run_data().immediate_exit = 0;

            // Swap the kvm_run structure pointer in so the signal handler can set
            // immediate_exit if the signal arrives just before the kvm_run ioctl.
            match r.swap(vp.run_data.ptr as usize, Ordering::Relaxed) {
                NO_KVM_RUN => {}
                CANCEL_KVM_RUN => {
                    // A cancel request signal arrived before the swap. Set
                    // immediate_exit so that any pending exit gets completed,
                    // and then the IOCTL returns before actually running the
                    // VP.
                    //
                    // The kvm_run structure is now aliased, so don't call
                    // `run_data()` to get it.
                    //
                    // SAFETY: the signal thread that might access the structure
                    // will also use `set_immediate_exit`.
                    unsafe { set_immediate_exit(vp.run_data.ptr) };
                }
                state => unreachable!("unexpected state {:#x}", state),
            }

            // SAFETY: Calling IOCTL as documented, with no special requirements.
            let result = unsafe { ioctl::kvm_run(vp.vcpu.as_raw_fd(), 0) };
            CURRENT_KVM_RUN.with(|r| r.store(NO_KVM_RUN, Ordering::Relaxed));
            match result {
                Ok(_) => Ok(true),
                Err(err) => match err {
                    nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN => Ok(false),
                    _ => Err(Error::Run(err)),
                },
            }
        })
    }

    /// Completes the current exit without running the VP further.
    ///
    /// This may generate more exits.
    pub fn complete_exit(&mut self) -> Result<Exit<'_>, Error> {
        CURRENT_KVM_RUN.with(|run| run.store(CANCEL_KVM_RUN, Ordering::Relaxed));
        self.run()
    }

    /// Continues running the VP.
    ///
    /// Runs until an exit occurs or interrupted by a signal or a call to
    /// [`Processor::force_exit`].
    pub fn run(&mut self) -> Result<Exit<'_>, Error> {
        if !self.run_vp_once()? {
            return Ok(Exit::Interrupted);
        }

        let exit = match self.run_data().exit_reason {
            KVM_EXIT_DEBUG => {
                // SAFETY: no other references to this data.
                let debug = unsafe { &self.run_data().__bindgen_anon_1.debug };

                #[cfg(not(target_arch = "x86_64"))]
                {
                    _ = debug;
                    todo!("debug exit on non-x86_64")
                }

                #[cfg(target_arch = "x86_64")]
                {
                    Exit::Debug {
                        exception: debug.arch.exception,
                        pc: debug.arch.pc,
                        dr6: debug.arch.dr6,
                        dr7: debug.arch.dr7,
                    }
                }
            }
            KVM_EXIT_IO => {
                // SAFETY: this is the active union field.
                let io = unsafe { self.run_data().__bindgen_anon_1.io };

                let offset = io.data_offset as usize;
                let data = &mut self.run_data_slice()
                    [offset..offset + io.size as usize * io.count as usize];
                if io.direction == KVM_EXIT_IO_IN as u8 {
                    Exit::IoIn {
                        port: io.port,
                        size: io.size,
                        data,
                    }
                } else {
                    Exit::IoOut {
                        port: io.port,
                        size: io.size,
                        data,
                    }
                }
            }
            KVM_EXIT_IRQ_WINDOW_OPEN => {
                let rdata = self.run_data();
                assert!(rdata.ready_for_interrupt_injection != 0);
                rdata.request_interrupt_window = 0;
                Exit::InterruptWindow
            }
            KVM_EXIT_MMIO => {
                // SAFETY: this is the active union field.
                let mmio = unsafe { &mut self.run_data().__bindgen_anon_1.mmio };
                if mmio.is_write != 0 {
                    Exit::MmioWrite {
                        address: mmio.phys_addr,
                        data: &mmio.data[0..mmio.len as usize],
                    }
                } else {
                    mmio.data = [0; 8];
                    Exit::MmioRead {
                        address: mmio.phys_addr,
                        data: &mut mmio.data[0..mmio.len as usize],
                    }
                }
            }
            KVM_EXIT_SHUTDOWN => Exit::Shutdown,
            KVM_EXIT_HYPERV => {
                // SAFETY: this is the active union field.
                let hyperv = unsafe { &mut self.run_data().__bindgen_anon_1.hyperv };
                match hyperv.type_ {
                    KVM_EXIT_HYPERV_HCALL => {
                        // SAFETY: this is the active union field.
                        let hcall = unsafe { &mut hyperv.u.hcall };
                        Exit::HvHypercall {
                            input: hcall.input,
                            result: &mut hcall.result,
                            params: hcall.params,
                        }
                    }
                    KVM_EXIT_HYPERV_SYNIC => {
                        // SAFETY: this is the active union field.
                        let synic = unsafe { &hyperv.u.synic };
                        Exit::SynicUpdate {
                            msr: synic.msr,
                            control: synic.control,
                            siefp: synic.evt_page,
                            simp: synic.msg_page,
                        }
                    }
                    _ => return Err(Error::UnknownHvExit(hyperv.type_)),
                }
            }
            KVM_EXIT_IOAPIC_EOI => {
                // SAFETY: this is the active union field.
                let eoi = unsafe { &mut self.run_data().__bindgen_anon_1.eoi };

                Exit::Eoi { irq: eoi.vector }
            }
            KVM_EXIT_FAIL_ENTRY => {
                // SAFETY: this is the active union field.
                let fail_entry = unsafe { &self.run_data().__bindgen_anon_1.fail_entry };
                Exit::FailEntry {
                    hardware_entry_failure_reason: fail_entry.hardware_entry_failure_reason,
                }
            }
            KVM_EXIT_INTERNAL_ERROR => {
                // SAFETY: this is the active union field.
                let internal = unsafe { &self.run_data().__bindgen_anon_1.internal };
                if internal.suberror == KVM_INTERNAL_ERROR_EMULATION {
                    // FUTURE: update bindings and get the instruction bytes when they are present.
                    Exit::EmulationFailure {
                        instruction_bytes: &[],
                    }
                } else {
                    Exit::InternalError {
                        error: internal.suberror,
                        data: &internal.data[..internal.ndata as usize],
                    }
                }
            }
            KVM_EXIT_X86_WRMSR => {
                // SAFETY: this is the active union field.
                let msr = unsafe { &mut self.run_data().__bindgen_anon_1.msr };
                msr.error = 0;
                Exit::MsrWrite {
                    index: msr.index,
                    data: msr.data,
                    error: &mut msr.error,
                }
            }
            KVM_EXIT_X86_RDMSR => {
                // SAFETY: this is the active union field.
                let msr = unsafe { &mut self.run_data().__bindgen_anon_1.msr };
                msr.data = 0;
                msr.error = 0;
                Exit::MsrRead {
                    index: msr.index,
                    data: &mut msr.data,
                    error: &mut msr.error,
                }
            }
            exit_reason => return Err(Error::UnknownExit(exit_reason)),
        };
        Ok(exit)
    }

    /// Request an exit when the interrupt window opens.
    ///
    /// Returns true if the window is already open (in which case the request is
    /// not registered).
    #[must_use]
    pub fn check_or_request_interrupt_window(&mut self) -> bool {
        let rdata = self.run_data();
        if rdata.ready_for_interrupt_injection != 0 {
            true
        } else {
            rdata.request_interrupt_window = 1;
            false
        }
    }

    /// Injects an extint interrupt.
    ///
    /// Caller must ensure that either it has received a
    /// [`Exit::InterruptWindow`] exit, or that
    /// [`Self::check_or_request_interrupt_window`] has returned `true`.
    pub fn inject_extint_interrupt(&mut self, vector: u8) -> Result<()> {
        self.partition.vp(self.idx).interrupt(vector.into())?;
        // Remember that there is a pending extint interrupt. KVM will update
        // this field again after the VP runs.
        self.run_data().ready_for_interrupt_injection = 0;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Exit<'a> {
    Interrupted,
    InterruptWindow,
    IoIn {
        port: u16,
        size: u8,
        data: &'a mut [u8],
    },
    IoOut {
        port: u16,
        size: u8,
        data: &'a [u8],
    },
    MmioRead {
        address: u64,
        data: &'a mut [u8],
    },
    MmioWrite {
        address: u64,
        data: &'a [u8],
    },
    MsrRead {
        index: u32,
        data: &'a mut u64,
        error: &'a mut u8,
    },
    MsrWrite {
        index: u32,
        data: u64,
        error: &'a mut u8,
    },
    Shutdown,
    FailEntry {
        hardware_entry_failure_reason: u64,
    },
    InternalError {
        error: u32,
        data: &'a [u64],
    },
    EmulationFailure {
        instruction_bytes: &'a [u8],
    },
    SynicUpdate {
        msr: u32,
        control: u64,
        siefp: u64,
        simp: u64,
    },
    HvHypercall {
        input: u64,
        result: &'a mut u64,
        params: [u64; 2],
    },
    Debug {
        exception: u32,
        pc: u64,
        dr6: u64,
        dr7: u64,
    },
    Eoi {
        irq: u8,
    },
}

/// Set up a signal used to cause KVM run_vp to return.
pub fn init() {
    static SIGNAL_HANDLER_INIT: Once = Once::new();
    SIGNAL_HANDLER_INIT.call_once(|| {
        let handler = || {
            CURRENT_KVM_RUN.with(|run| {
                // This interrupts the other code that accesses CURRENT_KVM_RUN, so a
                // compare_exchange is not necessary.
                let rdata = run.load(Ordering::Relaxed);
                match rdata {
                    NO_KVM_RUN => run.store(CANCEL_KVM_RUN, Ordering::Relaxed),
                    CANCEL_KVM_RUN => {}
                    _ => {
                        // SAFETY: other concurrent accesses to the structure are via
                        // `set_immediate_exit` or via atomic accesses in the kernel.
                        unsafe { set_immediate_exit(rdata as *mut kvm_run) };
                    }
                }
            })
        };
        // Ensure the thread local is initialized.
        CURRENT_KVM_RUN.with(|value| {
            std::hint::black_box(value);
        });
        // SAFETY: The signal handler does not perform any actions that are forbidden
        // for signal handlers to perform, as it only performs thread-local and atomic
        // reads and writes. We are guaranteed to not interrupt thread local initialization
        // as we have ensured it is initialized above.
        unsafe {
            signal_hook::low_level::register(libc::SIGRTMIN(), handler).unwrap();
        }
    });
}

const NO_KVM_RUN: usize = 0;
const CANCEL_KVM_RUN: usize = 1;

thread_local! {
    static CURRENT_KVM_RUN: AtomicUsize = const { AtomicUsize::new(NO_KVM_RUN) };
}

/// Sets `rdata.immediate_exit` to 1 without constructing a mutable reference.
///
/// This can be used when the kvm_run is aliased by the kernel or by other
/// threads that might call this function.
#[expect(clippy::missing_safety_doc)]
unsafe fn set_immediate_exit(rdata: *mut kvm_run) {
    // SAFETY: rdata may be aliased by the kernel right now, so it's
    // not safe to construct a mutable reference to it. Use an
    // atomic store to carefully write without requiring a mutable
    // reference.
    unsafe {
        (*(std::ptr::addr_of!((*rdata).immediate_exit).cast::<AtomicU8>()))
            .store(1, Ordering::Relaxed);
    }
}

pub struct DebugRegisters {
    /// DR0-3.
    pub db: [u64; 4],
    pub dr6: u64,
    pub dr7: u64,
}
