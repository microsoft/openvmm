// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The machine-check state of a KVM vCPU: `IA32_MCG_STATUS`, `IA32_MCG_CTL`,
//! and the registers of its error-reporting banks.
//!
//! KVM emulates them for every guest, whatever its CPUID says, and keeps them
//! in the vCPU, where a guest's writes land. A guest writes `IA32_MCG_STATUS`,
//! the control of each bank (`IA32_MCi_CTL`, and `IA32_MCi_CTL2` on a vCPU with
//! CMCI), and the address and miscellaneous registers of its banks, and clears
//! their status registers. A new vCPU starts with KVM's values, so a restore
//! that does not set them drops what the guest wrote.
//!
//! KVM lists `IA32_MCG_STATUS` and `IA32_MCG_CTL`, but not the registers of the
//! banks, in `KVM_GET_MSR_INDEX_LIST`. A vCPU has as many banks as its
//! `IA32_MCG_CAP` reports, at most [`MAX_BANKS`], so the partition's capability
//! follows `IA32_MCG_STATUS` (see [`super::msr_state::set_capabilities`]) and
//! this module reads the number of banks from the vCPU.
//!
//! A bank starts with every error disabled (`IA32_MCi_CTL` is 0), except that
//! `bind` asks KVM to set up the vCPU's machine-check capability, which enables
//! every error of every bank, on the hosts that advertise CMCI, which are
//! Intel's.
//!
//! A host-initiated `KVM_SET_MSRS` skips KVM's restrictions on who may write
//! which register, but not its checks of the value: KVM refuses an
//! `IA32_MCi_CTL` that is neither 0 nor all ones (bit 10 and bit 0 may be
//! clear), and a nonzero `IA32_MCi_CTL2` or `IA32_MCG_CTL` on a vCPU without
//! CMCI or `MCG_CTL_P`. A guest never holds such a value, since KVM refuses its
//! write, so a state that holds one, which only another backend or a crafted
//! state does, fails the restore.

use crate::KvmError;
use virt::x86::vp::MachineCheck;
use virt::x86::vp::MachineCheckBank;
use x86defs::McgCap;

/// KVM's `KVM_MAX_MCE_BANKS`: the most banks that a vCPU has.
const MAX_BANKS: usize = 32;

/// The registers that the state holds for a bank: `IA32_MCi_CTL`,
/// `IA32_MCi_STATUS`, `IA32_MCi_ADDR`, `IA32_MCi_MISC`, and `IA32_MCi_CTL2`.
const BANK_REGISTERS: usize = 5;

/// Returns the state of a bank that no guest has written, on a partition whose
/// vCPUs do or do not advertise CMCI: the state that `bind` leaves.
fn reset_bank(cmci_supported: bool) -> MachineCheckBank {
    MachineCheckBank {
        ctl: if cmci_supported { !0 } else { 0 },
        ..Default::default()
    }
}

/// Returns the MSRs of the state of a vCPU with `banks` banks, in the order of
/// [`values_of`] and [`state_from`]: `IA32_MCG_STATUS` and `IA32_MCG_CTL`, and
/// for each bank the four registers that follow `IA32_MC0_CTL` and then its
/// `IA32_MCi_CTL2`.
fn msrs(banks: usize) -> Vec<u32> {
    let mut msrs = vec![x86defs::X86X_MSR_MCG_STATUS, x86defs::X86X_MSR_MCG_CTL];
    for bank in 0..banks as u32 {
        let base = x86defs::X86X_MSR_MC0_CTL + 4 * bank;
        msrs.extend([
            base,
            base + 1,
            base + 2,
            base + 3,
            x86defs::X86X_MSR_MC0_CTL2 + bank,
        ]);
    }
    msrs
}

/// Returns the state that holds the `values` of the MSRs that [`msrs`] lists.
fn state_from(values: &[u64]) -> MachineCheck {
    let (&[status, ctl], banks) = values
        .split_first_chunk()
        .expect("the values of the two global MSRs");
    MachineCheck {
        status,
        ctl,
        banks: banks
            .chunks_exact(BANK_REGISTERS)
            .map(|bank| MachineCheckBank {
                ctl: bank[0],
                status: bank[1],
                addr: bank[2],
                misc: bank[3],
                ctl2: bank[4],
            })
            .collect(),
    }
}

/// Returns the values of the MSRs that [`msrs`] lists for a vCPU with `banks`
/// banks, as `state` sets them: a bank that the state lists as it holds it, and
/// one that it does not as `reset` has it.
fn values_of(state: &MachineCheck, banks: usize, reset: MachineCheckBank) -> Vec<u64> {
    let mut values = vec![state.status, state.ctl];
    for bank in 0..banks {
        let MachineCheckBank {
            ctl,
            status,
            addr,
            misc,
            ctl2,
        } = state.banks.get(bank).copied().unwrap_or(reset);
        values.extend([ctl, status, addr, misc, ctl2]);
    }
    values
}

/// Returns the number of banks that `vp` has.
fn banks(vp: &kvm::Processor<'_>) -> Result<usize, KvmError> {
    let mut cap = [0];
    vp.get_msrs(&[x86defs::X86X_MSR_MCG_CAP], &mut cap)?;
    let banks = usize::from(McgCap::from(cap[0]).count());
    if banks > MAX_BANKS {
        return Err(KvmError::InvalidState(
            "the vCPU has more machine-check banks than KVM's maximum",
        ));
    }
    Ok(banks)
}

/// Reads the machine-check state of `vp`.
pub(super) fn save(vp: &kvm::Processor<'_>) -> Result<MachineCheck, KvmError> {
    let msrs = msrs(banks(vp)?);
    let mut values = vec![0; msrs.len()];
    vp.get_msrs(&msrs, &mut values)?;
    Ok(state_from(&values))
}

/// Sets the machine-check state of `vp`: the banks that `state` lists, and the
/// banks that it does not as `bind` leaves them, which `cmci_supported` tells.
/// It refuses a state that lists more banks than `vp` has before it writes
/// anything.
pub(super) fn restore(
    vp: &kvm::Processor<'_>,
    state: &MachineCheck,
    cmci_supported: bool,
) -> Result<(), KvmError> {
    let banks = banks(vp)?;
    if state.banks.len() > banks {
        return Err(KvmError::InvalidState(
            "the saved machine-check state lists more banks than the vCPU has",
        ));
    }
    let writes: Vec<_> = msrs(banks)
        .into_iter()
        .zip(values_of(state, banks, reset_bank(cmci_supported)))
        .collect();
    vp.set_msrs(&writes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn bank(seed: u64) -> MachineCheckBank {
        MachineCheckBank {
            ctl: seed,
            status: seed + 1,
            addr: seed + 2,
            misc: seed + 3,
            ctl2: seed + 4,
        }
    }

    #[test]
    fn the_msrs_of_a_state_are_the_global_two_and_five_per_bank() {
        assert_eq!(msrs(0), [0x17a, 0x17b]);
        assert_eq!(
            msrs(2),
            [
                0x17a, 0x17b, //
                0x400, 0x401, 0x402, 0x403, 0x280, // bank 0
                0x404, 0x405, 0x406, 0x407, 0x281, // bank 1
            ]
        );
        // The last bank that a vCPU has.
        let msrs = msrs(MAX_BANKS);
        assert_eq!(msrs.len(), 2 + BANK_REGISTERS * MAX_BANKS);
        assert_eq!(msrs[msrs.len() - 5..], [0x47c, 0x47d, 0x47e, 0x47f, 0x29f]);
        // One `KVM_GET_MSRS` carries them all.
        assert!(msrs.len() <= 256);
    }

    #[test]
    fn a_state_holds_the_values_in_the_order_of_its_msrs() {
        let values: Vec<u64> = (100..100 + 2 + 3 * BANK_REGISTERS as u64).collect();
        let state = state_from(&values);
        assert_eq!(
            state,
            MachineCheck {
                status: 100,
                ctl: 101,
                banks: vec![bank(102), bank(107), bank(112)],
            }
        );
        assert_eq!(values_of(&state, 3, reset_bank(true)), values);
    }

    #[test]
    fn a_bank_that_the_state_lacks_is_at_the_state_that_bind_leaves() {
        let state = MachineCheck {
            status: 7,
            ctl: 0,
            banks: vec![bank(10)],
        };
        for cmci_supported in [true, false] {
            let reset = reset_bank(cmci_supported);
            assert_eq!(
                state_from(&values_of(&state, 3, reset)),
                MachineCheck {
                    status: 7,
                    ctl: 0,
                    banks: vec![bank(10), reset, reset],
                }
            );
            // The state at reset lists no bank, so it resets them all.
            assert_eq!(
                state_from(&values_of(&MachineCheck::default(), 2, reset)),
                MachineCheck {
                    status: 0,
                    ctl: 0,
                    banks: vec![reset; 2],
                }
            );
        }
        // A bank enables every error only on a host that advertises CMCI, and
        // records nothing on any.
        assert_eq!(
            reset_bank(true),
            MachineCheckBank {
                ctl: !0,
                ..Default::default()
            }
        );
        assert_eq!(reset_bank(false), MachineCheckBank::default());
    }
}

/// Tests against the host's KVM.
#[cfg(test)]
mod kvm_tests {
    use super::*;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::VmType;
    use test_with_tracing::test;

    /// Returns a VM with one vCPU that `bind` would have set up: with CMCI,
    /// where the host advertises it. It also returns whether that is so.
    fn vm() -> (Partition, bool) {
        let kvm = Kvm::new().unwrap();
        let cmci_supported = McgCap::from(kvm.supported_mce_cap().unwrap()).cmci_p();
        let mut vm = kvm.new_vm(VmType::Default).unwrap();
        vm.add_vp(0).unwrap();
        if cmci_supported {
            let mut cap = [0];
            vm.vp(0)
                .get_msrs(&[x86defs::X86X_MSR_MCG_CAP], &mut cap)
                .unwrap();
            vm.vp(0)
                .setup_mce(McgCap::from(cap[0]).with_cmci_p(true).into())
                .unwrap();
        }
        (vm, cmci_supported)
    }

    fn with_addr(bank: MachineCheckBank, addr: u64) -> MachineCheckBank {
        MachineCheckBank { addr, ..bank }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_new_vcpu_holds_every_bank_in_the_state_that_bind_leaves() {
        let (vm, cmci_supported) = vm();
        let state = save(&vm.vp(0)).unwrap();
        assert_eq!(
            state,
            MachineCheck {
                status: 0,
                ctl: 0,
                banks: vec![reset_bank(cmci_supported); MAX_BANKS],
            }
        );
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn the_state_of_one_vcpu_restores_into_another() {
        let (source, cmci_supported) = vm();
        let mut writes = vec![
            (x86defs::X86X_MSR_MCG_STATUS, 0x7),
            // KVM takes 0 or all ones, and bit 10 and bit 0 clear.
            (x86defs::X86X_MSR_MC0_CTL, 0),
            (x86defs::X86X_MSR_MC0_CTL + 2, 0x1234),
            (x86defs::X86X_MSR_MC0_CTL + 3, 0x5678),
            (x86defs::X86X_MSR_MC0_CTL + 4 * 3, !(1 << 10)),
            (x86defs::X86X_MSR_MC0_CTL + 4 * 31 + 2, 0xdead_beef),
        ];
        if cmci_supported {
            writes.push((x86defs::X86X_MSR_MC0_CTL2 + 1, 0x4000_0003));
        }
        source.vp(0).set_msrs(&writes).unwrap();
        let saved = save(&source.vp(0)).unwrap();
        assert_eq!(saved.status, 0x7);
        assert_eq!(saved.banks[0].addr, 0x1234);
        assert_eq!(saved.banks[0].misc, 0x5678);
        assert_eq!(saved.banks[31].addr, 0xdead_beef);
        assert_eq!(saved.banks[1].ctl2, if cmci_supported { 0x4000_0003 } else { 0 });

        let (destination, _) = vm();
        assert_ne!(save(&destination.vp(0)).unwrap(), saved);
        restore(&destination.vp(0), &saved, cmci_supported).unwrap();
        assert_eq!(save(&destination.vp(0)).unwrap(), saved);
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_restore_resets_the_banks_that_the_state_lacks() {
        let (vm, cmci_supported) = vm();
        let reset = reset_bank(cmci_supported);
        vm.vp(0)
            .set_msrs(&[
                (x86defs::X86X_MSR_MCG_STATUS, 0x7),
                (x86defs::X86X_MSR_MC0_CTL + 4 + 2, 0x99),
            ])
            .unwrap();
        let state = MachineCheck {
            status: 1,
            ctl: 0,
            banks: vec![with_addr(reset, 0x11)],
        };
        restore(&vm.vp(0), &state, cmci_supported).unwrap();
        let restored = save(&vm.vp(0)).unwrap();
        assert_eq!(restored.status, 1);
        assert_eq!(restored.banks[0], with_addr(reset, 0x11));
        assert!(restored.banks[1..].iter().all(|bank| *bank == reset));

        // The state at reset resets them all.
        restore(&vm.vp(0), &MachineCheck::default(), cmci_supported).unwrap();
        assert_eq!(
            save(&vm.vp(0)).unwrap(),
            MachineCheck {
                status: 0,
                ctl: 0,
                banks: vec![reset; MAX_BANKS],
            }
        );
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_restore_refuses_more_banks_than_the_vcpu_has_and_writes_nothing() {
        let (vm, cmci_supported) = vm();
        let before = save(&vm.vp(0)).unwrap();
        let state = MachineCheck {
            status: 0x55,
            banks: vec![reset_bank(cmci_supported); MAX_BANKS + 1],
            ..Default::default()
        };
        assert!(matches!(
            restore(&vm.vp(0), &state, cmci_supported),
            Err(KvmError::InvalidState(_))
        ));
        assert_eq!(save(&vm.vp(0)).unwrap(), before);
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_restore_of_a_control_that_kvm_refuses_fails() {
        let (vm, cmci_supported) = vm();
        let state = MachineCheck {
            banks: vec![MachineCheckBank {
                ctl: 0x1234,
                ..reset_bank(cmci_supported)
            }],
            ..Default::default()
        };
        assert!(restore(&vm.vp(0), &state, cmci_supported).is_err());
    }
}
