// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP nested-virtualization capability probe.
//!
//! Reports what the host's WHP implementation can do for nested
//! virtualization, independent of whether `virt_whp` is configured to
//! opt in. Used for diagnostics via `inspect` and (in later phases) for
//! validating the user's nested-virt request before partition setup.

use crate::Error;
use crate::WhpResultExt;
use inspect::Inspect;

/// Reports what the host's WHP implementation can do for nested
/// virtualization.
///
/// `property_supported` is the master gate: if false, asking WHP for a
/// nested-capable partition will fail because the
/// `WHvPartitionPropertyCodeNestedVirtualization` property is unknown to
/// this build of Windows. The remaining fields describe which feature
/// bits are available to advertise to the guest once a nested partition
/// is created.
#[derive(Debug, Copy, Clone, Inspect)]
pub(crate) struct NestedVirtCapability {
    /// `WHvPartitionPropertyCodeNestedVirtualization` is exposed by this
    /// build of Windows. This is the master switch — without it, no
    /// amount of feature-bank tweaking will create a nested-capable
    /// partition.
    pub property_supported: bool,
    /// `WHvCapabilityCodeProcessorFeaturesBanks` is exposed by this
    /// build of Windows. Required to set bank1 feature bits.
    pub bank1_supported: bool,
    /// `WHV_PROCESSOR_FEATURES1::NestedVirtSupport` is reported by the
    /// host. This is the CPUID/MSR-exposure bit that makes the L1 guest
    /// see VMX (Intel) or SVM (AMD).
    pub nested_virt_support: bool,
    /// `WHV_PROCESSOR_FEATURES1::VmxExceptionInjectSupport` is reported
    /// by the host. Controls whether `IA32_VMX_BASIC.ExceptionInject` is
    /// advertised to L1. Harmless on AMD.
    pub vmx_exception_inject: bool,
    /// `WHV_SYNTHETIC_PROCESSOR_FEATURES::EnlightenedVmcs` is reported
    /// by the host. Enables the Hyper-V enlightened-VMCS fast path.
    pub enlightened_vmcs: bool,
    /// `WHV_SYNTHETIC_PROCESSOR_FEATURES::NestedDebugCtl` is reported
    /// by the host. Allows nonzero `IA32_DEBUGCTL` in nested execution.
    pub nested_debug_ctl: bool,
    /// Host CPU vendor as reported by WHP. Intel ⇒ VMX, AMD ⇒ SVM.
    #[inspect(debug)]
    pub vendor: whp::abi::WHV_PROCESSOR_VENDOR,
}

/// Probe the host WHP implementation for nested-virt support.
///
/// Performs:
/// - A `WHvSetPartitionProperty` probe on a throwaway partition to
///   detect whether `WHvPartitionPropertyCodeNestedVirtualization` is
///   recognized.
/// - Reads `WHvCapabilityCodeProcessorFeaturesBanks` for bank1 bits.
/// - Reads `WHvCapabilityCodeSyntheticProcessorFeaturesBanks` for the
///   nested-friendly synthetic feature bits.
/// - Reads the host processor vendor.
pub(crate) fn nested_virt_capability() -> Result<NestedVirtCapability, Error> {
    use whp::abi::WHV_PROCESSOR_FEATURES1;
    use whp::abi::WHV_SYNTHETIC_PROCESSOR_FEATURES;

    let vendor = whp::capabilities::processor_vendor().for_op("get processor vendor")?;

    let processor_features =
        whp::capabilities::processor_features().for_op("get processor features capability")?;
    let bank1 = processor_features.bank1;
    let bank1_supported = bank1.0 != 0;
    let nested_virt_support = bank1.is_set(WHV_PROCESSOR_FEATURES1::NestedVirtSupport);
    let vmx_exception_inject = bank1.is_set(WHV_PROCESSOR_FEATURES1::VmxExceptionInjectSupport);

    let synth = whp::capabilities::synthetic_processor_features()
        .for_op("get synthetic processor features capability")?;
    let enlightened_vmcs = synth
        .bank0
        .is_set(WHV_SYNTHETIC_PROCESSOR_FEATURES::EnlightenedVmcs);
    let nested_debug_ctl = synth
        .bank0
        .is_set(WHV_SYNTHETIC_PROCESSOR_FEATURES::NestedDebugCtl);

    // Probe for the `WHvPartitionPropertyCodeNestedVirtualization`
    // property by creating a throwaway partition and setting it to its
    // default value. Older Windows builds that lack the property will
    // return `WHV_E_UNKNOWN_PROPERTY`. Setting `false` is a semantic
    // no-op (it is the default) and the partition is dropped without
    // ever being set up.
    let mut probe = whp::PartitionConfig::new().for_op("create probe partition")?;
    let property_supported =
        match probe.set_property(whp::PartitionProperty::NestedVirtualization(false)) {
            Ok(_) => true,
            Err(whp::WHvError::WHV_E_UNKNOWN_PROPERTY) => false,
            Err(err) => {
                return Err(err).for_op("probe NestedVirtualization property");
            }
        };

    Ok(NestedVirtCapability {
        property_supported,
        bank1_supported,
        nested_virt_support,
        vmx_exception_inject,
        enlightened_vmcs,
        nested_debug_ctl,
        vendor,
    })
}

/// Probe the host's nested-virt capability and validate a caller's
/// nested-virt request against it.
///
/// Always returns the capability (so the partition can expose it through
/// `inspect` regardless of whether nested-virt was requested). If
/// `requested` is `true`, also fails when the request can't be honored:
///
/// - `NestedVirtIncompatibleWithUserModeApic` if `user_mode_apic` is set
///   (WHP refuses `NestedVirtualization=TRUE` together with
///   `LocalApicEmulationMode=None`).
/// - `NestedVirtUnsupported` if the host's WHP does not expose the
///   `WHvPartitionPropertyCodeNestedVirtualization` property.
///
/// VTL2 and isolation are also incompatible with nested-virt because the
/// Windows hypervisor refuses to install a generic hypercall intercept on
/// a nested-virt-capable partition
/// (`onecore/hv/hvx/im/common/ImCpuCommon.c:ImInstallHypercallIntercept`
/// — *"For now, hypercall exits are not supported with nested."*), and
/// the VTL2 / isolation hypercall dispatchers depend on that intercept.
/// Those checks live in the caller because they depend on
/// `ProtoPartitionConfig` fields beyond this function's scope.
pub(crate) fn validate(
    requested: bool,
    user_mode_apic: bool,
) -> Result<NestedVirtCapability, Error> {
    let capability = nested_virt_capability()?;
    if requested {
        if user_mode_apic {
            return Err(Error::NestedVirtIncompatibleWithUserModeApic);
        }
        if !capability.property_supported {
            return Err(Error::NestedVirtUnsupported);
        }
    }
    Ok(capability)
}
