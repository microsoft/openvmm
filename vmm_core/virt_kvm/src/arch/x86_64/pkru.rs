// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The PKRU of the thread that runs a VP.
//!
//! KVM keeps a guest's PKRU in its vCPU, apart from the guest's FPU state, and
//! `KVM_GET_XSAVE` reports it only if XSTATE_BV bit 9, the PKRU component, is
//! set in the guest FPU that KVM saved last. Every `KVM_RUN`, even one that
//! returns before it enters the guest, saves that FPU with XSAVE on its way
//! out, when the PKRU register holds the PKRU of the thread that called it (KVM
//! reloads it after each exit of a guest that uses protection keys), and XSAVE
//! takes the bit from XINUSE, which the processor may clear for a PKRU of 0,
//! the component's initial configuration. So a save of a guest that uses
//! protection keys, after a `KVM_RUN` on a thread whose PKRU is 0, can drop the
//! component and record the guest's PKRU as its initial value, 0, and a restore
//! then runs the guest with every protection key open. XINUSE is always set for
//! any other PKRU, so a save after a `KVM_RUN` on such a thread keeps the
//! guest's.
//!
//! A new process starts with Linux's `init_pkru`, and a new thread starts with
//! its creator's PKRU. An administrator can set `init_pkru` to 0, through the
//! debugfs file `x86/init_pkru` or the kernel command line, and then every
//! thread of the process has a PKRU of 0, as one has where other code in the
//! process wrote 0. The default, 0x5555_5554, is not 0, so a host in its
//! default configuration is not affected.
//!
//! Before its first `KVM_RUN`, a VP's thread therefore gets Linux's default if
//! its PKRU is 0, and keeps its PKRU otherwise. The default disables access to
//! the keys 1 to 15 and leaves key 0 open, and it is the PKRU that a process's
//! threads start with on a host in its default configuration. No crate that
//! OpenVMM's programs link, its own or a dependency, uses a protection key:
//! none calls `pkey_alloc` or `pkey_mprotect`, and none uses `PROT_EXEC`, the
//! one protection for which the kernel picks a key of its own (execute-only
//! memory). So all of the process's memory is on key 0, which the default
//! leaves open for the thread's own accesses and for the kernel's on its
//! behalf, such as KVM's to guest memory, which PKRU also checks.

use safe_intrinsics::cpuid;

/// `CPUID.(7,0):ECX[3]`, PKU: the CPU has protection keys.
const LEAF7_ECX_PKU: u32 = 1 << 3;

/// `CPUID.(7,0):ECX[4]`, OSPKE, which mirrors CR4.PKE: the kernel enabled
/// protection keys.
const LEAF7_ECX_OSPKE: u32 = 1 << 4;

/// Linux's PKRU for a new process, `init_pkru_value`: access disabled for the
/// keys 1 to 15, key 0 open.
const DEFAULT_PKRU: u32 = 0x5555_5554;

/// Gives the calling thread, which is about to run a VP, [`DEFAULT_PKRU`] if
/// its PKRU is 0, as the module describes. A host without protection keys has
/// no PKRU, so nothing changes there.
///
/// A PKRU that is not 0 stays, rather than the default replacing every PKRU as
/// microsoft/nvx#1359 first proposed: such a thread already keeps the guest's
/// PKRU in a save, so a host in its default configuration, or whose
/// administrator chose another `init_pkru`, runs as it did.
pub(super) fn raise_zero_thread_pkru() {
    raise_zero_pkru(
        cpuid(7, 0).ecx,
        // SAFETY: `raise_zero_pkru` calls this only where the kernel enabled
        // protection keys.
        || unsafe { rdpkru() },
        // SAFETY: as above, and `raise_zero_pkru` writes only `DEFAULT_PKRU`,
        // which leaves key 0, and so all of OpenVMM's memory, open.
        |pkru| unsafe { wrpkru(pkru) },
    );
}

/// Writes [`DEFAULT_PKRU`] with `write` if `read` returns 0, on a host whose
/// `CPUID.(7,0):ECX` is `leaf7_ecx`. Returns whether it wrote.
///
/// It calls `read` and `write` only on a host that has protection keys, since
/// RDPKRU and WRPKRU raise #UD on one that does not.
fn raise_zero_pkru(leaf7_ecx: u32, read: impl FnOnce() -> u32, write: impl FnOnce(u32)) -> bool {
    if !protection_keys_enabled(leaf7_ecx) || read() != 0 {
        return false;
    }
    write(DEFAULT_PKRU);
    true
}

/// Returns whether a host whose `CPUID.(7,0):ECX` is `leaf7_ecx` has
/// protection keys that the kernel enabled, without which RDPKRU and WRPKRU
/// raise #UD.
fn protection_keys_enabled(leaf7_ecx: u32) -> bool {
    let enabled = LEAF7_ECX_PKU | LEAF7_ECX_OSPKE;
    leaf7_ecx & enabled == enabled
}

/// Reads the calling thread's PKRU.
///
/// # Safety
///
/// The kernel must have enabled protection keys (CR4.PKE), as OSPKE reports,
/// or RDPKRU raises #UD.
unsafe fn rdpkru() -> u32 {
    let pkru: u32;
    // SAFETY: the caller guarantees that CR4.PKE is set. RDPKRU requires ECX to
    // be 0, writes EAX and EDX, and touches no memory.
    unsafe {
        core::arch::asm!(
            "rdpkru",
            in("ecx") 0,
            out("eax") pkru,
            out("edx") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    pkru
}

/// Writes the calling thread's PKRU.
///
/// # Safety
///
/// The kernel must have enabled protection keys (CR4.PKE), as OSPKE reports,
/// or WRPKRU raises #UD. The new PKRU must also leave open the keys that the
/// thread's memory accesses use.
unsafe fn wrpkru(pkru: u32) {
    // SAFETY: the caller guarantees that CR4.PKE is set. WRPKRU requires ECX
    // and EDX to be 0, and changes only the thread's PKRU. It changes which
    // memory accesses are allowed, so the instruction is not marked as touching
    // no memory.
    unsafe {
        core::arch::asm!(
            "wrpkru",
            in("eax") pkru,
            in("ecx") 0,
            in("edx") 0,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_PKRU;
    use super::LEAF7_ECX_OSPKE;
    use super::LEAF7_ECX_PKU;
    use super::protection_keys_enabled;
    use super::raise_zero_pkru;
    use super::raise_zero_thread_pkru;
    use super::rdpkru;
    use super::wrpkru;
    use safe_intrinsics::cpuid;
    use std::cell::Cell;
    use test_with_tracing::test;

    /// Runs the raise of a thread whose PKRU is `pkru`, on a host whose
    /// `CPUID.(7,0):ECX` is `leaf7_ecx`, and returns what it wrote and whether
    /// it read the PKRU.
    fn raise(leaf7_ecx: u32, pkru: u32) -> (Option<u32>, bool) {
        let read = Cell::new(false);
        let written = Cell::new(None);
        let raised = raise_zero_pkru(
            leaf7_ecx,
            || {
                read.set(true);
                pkru
            },
            |value| written.set(Some(value)),
        );
        assert_eq!(raised, written.get().is_some());
        (written.get(), read.get())
    }

    #[test]
    fn a_zero_pkru_gets_a_pkru_that_xsave_tracks_and_that_leaves_key_zero_open() {
        let (written, read) = raise(LEAF7_ECX_PKU | LEAF7_ECX_OSPKE, 0);
        assert!(read);
        let written = written.unwrap();
        assert_ne!(written, 0);
        // The access-disable and write-disable bits of key 0.
        assert_eq!(written & 0b11, 0);
    }

    #[test]
    fn a_pkru_that_is_not_zero_is_kept() {
        for pkru in [1, 0b100, DEFAULT_PKRU, u32::MAX] {
            assert_eq!(
                raise(LEAF7_ECX_PKU | LEAF7_ECX_OSPKE, pkru),
                (None, true),
                "PKRU {pkru:#x}"
            );
        }
    }

    #[test]
    fn a_host_without_enabled_protection_keys_is_left_alone() {
        // RDPKRU and WRPKRU raise #UD, so the PKRU is not even read.
        for leaf7_ecx in [
            0,
            LEAF7_ECX_PKU,
            LEAF7_ECX_OSPKE,
            !(LEAF7_ECX_PKU | LEAF7_ECX_OSPKE),
        ] {
            assert_eq!(raise(leaf7_ecx, 0), (None, false), "ECX {leaf7_ecx:#x}");
        }
    }

    /// Where the host has protection keys, the calling thread's own PKRU
    /// changes, and only from 0; elsewhere RDPKRU and WRPKRU would fault, so
    /// nothing runs them. PKRU is per thread, so this runs on a thread of its
    /// own.
    #[test]
    fn the_calling_threads_pkru_is_raised_only_where_it_is_zero() {
        // Restricts only key 15, which nothing uses.
        const PKRU_KEY_15: u32 = 1 << 30;
        std::thread::spawn(|| {
            raise_zero_thread_pkru();
            if !protection_keys_enabled(cpuid(7, 0).ecx) {
                return;
            }
            // SAFETY: the host has protection keys, and no PKRU here restricts
            // key 0.
            unsafe {
                wrpkru(0);
                raise_zero_thread_pkru();
                assert_eq!(rdpkru(), DEFAULT_PKRU);

                wrpkru(PKRU_KEY_15);
                raise_zero_thread_pkru();
                assert_eq!(rdpkru(), PKRU_KEY_15);
            }
        })
        .join()
        .unwrap();
    }
}

/// Tests against the host's KVM. KVM saves the guest's FPU on every `KVM_RUN`,
/// even one that returns before it enters the guest, so these restore the PKRU
/// of a guest into a VP and run `KVM_RUN` once without guest code.
#[cfg(test)]
mod kvm_tests {
    use super::LEAF7_ECX_PKU;
    use super::protection_keys_enabled;
    use super::raise_zero_thread_pkru;
    use super::wrpkru;
    use kvm::Exit;
    use kvm::Kvm;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::kvm_cpuid_entry2;
    use test_with_tracing::test;
    use x86defs::cpuid::CpuidFunction;
    use x86defs::xsave::XSAVE_LEGACY_LEN;

    /// The PKRU component of an XSAVE area.
    const PKRU_COMPONENT: u32 = 9;

    /// The PKRU of the guest: not 0, the initial PKRU, and not Linux's default
    /// either.
    const GUEST_PKRU: u32 = 0xaaaa_aaa8;

    /// Returns the CPUID of a guest that has protection keys and the offset of
    /// the PKRU component in the area that `KVM_GET_XSAVE` returns, or `None`
    /// where the host's kernel or KVM has no protection keys.
    fn guest_cpuid(kvm: &Kvm) -> Option<(Vec<kvm_cpuid_entry2>, usize)> {
        if !protection_keys_enabled(safe_intrinsics::cpuid(7, 0).ecx) {
            return None;
        }
        // The partition drops KVM's own leaves.
        let cpuid: Vec<_> = kvm
            .supported_cpuid()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.function & 0xf000_0000 != 0x4000_0000)
            .collect();
        let leaf = |function, index| {
            cpuid
                .iter()
                .find(|entry| entry.function == function && entry.index == index)
        };
        let has_pku = leaf(7, 0)?.ecx & LEAF7_ECX_PKU != 0;
        let component = leaf(CpuidFunction::ExtendedStateEnumeration.0, PKRU_COMPONENT)?;
        let offset = component.ebx as usize;
        let supported = has_pku && component.ebx != 0 && offset + 4 <= 4096;
        supported.then_some((cpuid, offset))
    }

    /// What `KVM_GET_XSAVE` reports of the PKRU.
    #[derive(Debug)]
    struct SavedPkru {
        xstate_bv: u64,
        pkru: u32,
    }

    impl SavedPkru {
        fn read(vp: &Processor<'_>, pkru_offset: usize) -> Self {
            let mut area = [0; 4096];
            vp.get_xsave(&mut area).unwrap();
            let xstate_bv = &area[XSAVE_LEGACY_LEN..XSAVE_LEGACY_LEN + 8];
            let pkru = &area[pkru_offset..pkru_offset + 4];
            Self {
                xstate_bv: u64::from_le_bytes(xstate_bv.try_into().unwrap()),
                pkru: u32::from_le_bytes(pkru.try_into().unwrap()),
            }
        }

        fn has_component(&self) -> bool {
            self.xstate_bv & (1 << PKRU_COMPONENT) != 0
        }
    }

    /// Restores a guest PKRU of `GUEST_PKRU` into a new VP, runs `KVM_RUN` once
    /// on a VP thread whose PKRU is 0, as under a host whose `x86/init_pkru` is
    /// 0, and returns what a save reports of the PKRU afterwards. The thread
    /// first raises its PKRU as `bind` does if `raise`.
    fn run_on_thread_with_pkru_zero(
        kvm: &Kvm,
        cpuid: &[kvm_cpuid_entry2],
        pkru_offset: usize,
        raise: bool,
    ) -> SavedPkru {
        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.add_vp(0).unwrap();
        let vp = partition.vp(0);
        vp.set_cpuid(cpuid).unwrap();

        // An area with the PKRU component alone, in the standard format.
        let mut area = [0; 4096];
        area[XSAVE_LEGACY_LEN..XSAVE_LEGACY_LEN + 8]
            .copy_from_slice(&(1u64 << PKRU_COMPONENT).to_le_bytes());
        area[pkru_offset..pkru_offset + 4].copy_from_slice(&GUEST_PKRU.to_le_bytes());
        vp.set_xsave(&area).unwrap();
        let restored = SavedPkru::read(&vp, pkru_offset);
        assert!(restored.has_component(), "{restored:x?}");
        assert_eq!(restored.pkru, GUEST_PKRU);

        let partition = &partition;
        std::thread::scope(|s| {
            s.spawn(move || {
                // SAFETY: the host has protection keys, as `guest_cpuid`
                // checked, and 0 leaves every key open.
                unsafe { wrpkru(0) };
                if raise {
                    raise_zero_thread_pkru();
                }
                let mut runner = partition.vp(0).runner();
                // `complete_exit` issues `KVM_RUN` with an immediate exit,
                // which swaps the guest's FPU in and out without entering the
                // guest.
                assert!(matches!(runner.complete_exit().unwrap(), Exit::Interrupted));
            })
            .join()
            .unwrap();
        });
        SavedPkru::read(&partition.vp(0), pkru_offset)
    }

    /// A save keeps the PKRU that a restore gave the guest after a `KVM_RUN` on
    /// a VP thread whose PKRU was 0 and that `bind` raised. On such a thread
    /// that was not raised, the processor may track the PKRU as not in use, and
    /// the save then drops the component.
    #[test]
    #[ignore = "requires /dev/kvm on a host with PKU"]
    fn a_save_keeps_a_restored_pkru_after_a_run_on_a_thread_whose_pkru_was_zero() {
        let kvm = Kvm::new().unwrap();
        let Some((cpuid, pkru_offset)) = guest_cpuid(&kvm) else {
            tracing::info!("the host or KVM has no protection keys");
            return;
        };

        let unraised = run_on_thread_with_pkru_zero(&kvm, &cpuid, pkru_offset, false);
        tracing::info!(
            ?unraised,
            has_component = unraised.has_component(),
            "unraised"
        );

        let raised = run_on_thread_with_pkru_zero(&kvm, &cpuid, pkru_offset, true);
        tracing::info!(?raised, has_component = raised.has_component(), "raised");
        assert!(raised.has_component(), "{raised:x?}");
        assert_eq!(raised.pkru, GUEST_PKRU);
    }
}
