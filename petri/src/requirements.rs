// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.

use petri_artifacts_common::capabilities;
use std::collections::BTreeSet;

/// Execution environments where tests can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionEnvironment {
    /// Bare metal execution (not nested virtualization).
    Baremetal,
    /// Nested virtualization environment.
    Nested,
}

/// CPU vendors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    /// AMD processors.
    Amd,
    /// Intel processors.
    Intel,
    /// ARM processors.
    Arm,
}

impl Vendor {
    /// Detect the vendor of the host CPU the test is running on.
    pub fn host() -> Self {
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "x86_64")]
        {
            let result =
                safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VendorAndMaxFunction.0, 0);
            let vendor =
                x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx);
            if vendor.is_amd_compatible() {
                Vendor::Amd
            } else {
                assert!(vendor.is_intel_compatible());
                Vendor::Intel
            }
        }
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(not(target_arch = "x86_64"))]
        {
            Vendor::Arm
        }
    }
}

/// Types of isolation supported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IsolationType {
    /// Virtualization-based Security (VBS)
    Vbs,
    /// Secure Nested Paging (SNP)
    Snp,
    /// Trusted Domain Extensions (TDX)
    Tdx,
}

/// VMM implementation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmmType {
    /// OpenVMM.
    OpenVmm,
    /// Microsoft Hyper-V.
    HyperV,
}

/// Hypervisor backends that OpenVMM can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenVmmHypervisor {
    /// Linux Microsoft Hypervisor backend.
    Mshv,
    /// Linux KVM backend.
    Kvm,
    /// Windows Hypervisor Platform backend.
    Whp,
    /// macOS Hypervisor Framework backend.
    Hvf,
}

/// Information about the VM host, retrieved via PowerShell on Windows.
#[derive(Debug, Clone)]
pub struct VmHostInfo {
    /// VBS support status
    pub vbs_supported: bool,
    /// SNP support status
    pub snp_status: bool,
    /// TDX support status
    pub tdx_status: bool,
}

/// Platform-specific host context extending the base HostContext
#[derive(Debug, Clone)]
pub struct HostContext {
    /// VmHost information retrieved via PowerShell
    pub vm_host_info: Option<VmHostInfo>,
    /// CPU vendor
    pub vendor: Vendor,
    /// Execution environment
    pub execution_environment: ExecutionEnvironment,
    /// Whether the host hypervisor supports software VPCI device emulation
    pub vpci_supported: bool,
    /// Hypervisor backend that OpenVMM will select on this host.
    pub openvmm_hypervisor: Option<OpenVmmHypervisor>,
}

impl HostContext {
    /// Create a new host context by querying host information
    pub async fn new() -> Self {
        let is_nested = {
            // xtask-fmt allow-target-arch cpu-intrinsic
            #[cfg(target_arch = "x86_64")]
            {
                let result = safe_intrinsics::cpuid(
                    hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION,
                    0,
                );
                hvdef::HvEnlightenmentInformation::from(
                    result.eax as u128
                        | (result.ebx as u128) << 32
                        | (result.ecx as u128) << 64
                        | (result.edx as u128) << 96,
                )
                .nested()
            }
            // xtask-fmt allow-target-arch cpu-intrinsic
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        };

        let vendor = Vendor::host();

        let vm_host_info = {
            #[cfg(windows)]
            {
                crate::vm::hyperv::powershell::run_get_vm_host()
                    .await
                    .ok()
                    .map(|info| VmHostInfo {
                        vbs_supported: info.guest_isolation_types.contains(
                            &crate::vm::hyperv::powershell::HyperVGuestStateIsolationType::Vbs,
                        ),
                        snp_status: info.snp_status,
                        tdx_status: info.tdx_status,
                    })
            }
            #[cfg(not(windows))]
            {
                None
            }
        };

        // VPCI support: only Windows (virt_whp and Hyper-V) supports it for now.
        let vpci_supported = cfg!(windows);

        let openvmm_hypervisor = if cfg!(target_os = "linux") {
            if fs_err::File::open("/dev/mshv").is_ok() {
                Some(OpenVmmHypervisor::Mshv)
            } else if fs_err::File::options()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok()
            {
                Some(OpenVmmHypervisor::Kvm)
            } else {
                None
            }
        } else if cfg!(windows) {
            Some(OpenVmmHypervisor::Whp)
        } else if cfg!(target_os = "macos") {
            Some(OpenVmmHypervisor::Hvf)
        } else {
            None
        };

        Self {
            vm_host_info,
            vendor,
            execution_environment: if is_nested {
                ExecutionEnvironment::Nested
            } else {
                ExecutionEnvironment::Baremetal
            },
            vpci_supported,
            openvmm_hypervisor,
        }
    }
}

/// A single requirement for a test to run.
pub enum TestRequirement {
    /// Execution environment requirement.
    ExecutionEnvironment(ExecutionEnvironment),
    /// Vendor requirement.
    Vendor(Vendor),
    /// Isolation requirement.
    Isolation(IsolationType),
    /// Requires a named capability advertised by the execution environment or
    /// detected by petri.
    ///
    /// Capabilities are how a test says "I need a specific resource to be
    /// provisioned for me" without naming who provides it or how. The
    /// execution environment can advertise capabilities via the
    /// comma-separated `PETRI_CAPABILITIES` environment variable, and petri can
    /// add capabilities that it detects itself. A test requiring a capability
    /// that is not available is skipped, so such tests automatically
    /// self-exclude on any host that cannot satisfy them.
    RequiresCapability {
        /// Capability name.
        name: &'static str,
        /// VMM used by the test, which may affect capability availability.
        vmm: VmmType,
    },
    /// Logical AND of two requirements.
    And(Box<TestRequirement>, Box<TestRequirement>),
    /// Logical OR of two requirements.
    Or(Box<TestRequirement>, Box<TestRequirement>),
    /// Logical NOT of a requirement.
    Not(Box<TestRequirement>),
    /// Requirement satisfied by any host context.
    Any,
}

impl TestRequirement {
    /// Combine this requirement with another requirement using logical AND.
    pub fn and(self, other: TestRequirement) -> TestRequirement {
        TestRequirement::And(Box::new(self), Box::new(other))
    }

    /// Combine this requirement with another requirement using logical OR.
    pub fn or(self, other: TestRequirement) -> TestRequirement {
        TestRequirement::Or(Box::new(self), Box::new(other))
    }

    /// Negate this requirement.
    #[expect(clippy::should_implement_trait)]
    pub fn not(self) -> TestRequirement {
        TestRequirement::Not(Box::new(self))
    }

    /// Evaluate if this requirement is satisfied with the given host context
    pub fn is_satisfied(&self, context: &HostContext) -> bool {
        match self {
            TestRequirement::ExecutionEnvironment(env) => context.execution_environment == *env,
            TestRequirement::Vendor(vendor) => context.vendor == *vendor,
            TestRequirement::Isolation(isolation_type) => {
                if let Some(vm_host_info) = &context.vm_host_info {
                    match isolation_type {
                        IsolationType::Vbs => vm_host_info.vbs_supported,
                        IsolationType::Snp => vm_host_info.snp_status,
                        IsolationType::Tdx => vm_host_info.tdx_status,
                    }
                } else {
                    false
                }
            }
            TestRequirement::RequiresCapability { name, vmm } => {
                available_capabilities(context, *vmm).contains(name)
            }
            TestRequirement::And(req1, req2) => {
                req1.is_satisfied(context) && req2.is_satisfied(context)
            }
            TestRequirement::Or(req1, req2) => {
                req1.is_satisfied(context) || req2.is_satisfied(context)
            }
            TestRequirement::Not(req) => !req.is_satisfied(context),
            TestRequirement::Any => true,
        }
    }
}

/// Returns the canonical runtime name for a known capability.
pub fn known_capability(name: &str) -> Option<&'static str> {
    capabilities::known(name)
}

/// Returns whether `name` is a known capability.
pub fn is_known_capability(name: &str) -> bool {
    known_capability(name).is_some()
}

fn available_capabilities(context: &HostContext, vmm: VmmType) -> BTreeSet<&'static str> {
    let mut capabilities = BTreeSet::new();

    if context.vpci_supported {
        capabilities.insert(capabilities::VPCI);
    }

    // virt_mshv cannot currently reset partitions running Windows.
    // This is due to two issues:
    // * A hypervisor issue that prevents locked hv#1 MSRs from being set by the host VMM.
    // * Missing support for the HvScrubPartition hypercall.
    // Once either of these are fixed, we can remove this check and feature.
    if !matches!(
        (vmm, context.openvmm_hypervisor),
        (VmmType::OpenVmm, Some(OpenVmmHypervisor::Mshv))
    ) {
        capabilities.insert(capabilities::WINDOWS_PARTITION_RESET);
    }

    match std::env::var("PETRI_CAPABILITIES") {
        Ok(env_capabilities) => {
            for capability in env_capabilities.split(',').map(str::trim) {
                if capability.is_empty() {
                    continue;
                }
                let capability = known_capability(capability)
                    .unwrap_or_else(|| panic!("unknown PETRI_CAPABILITIES entry: {capability}"));
                capabilities.insert(capability);
            }
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("PETRI_CAPABILITIES is not valid UTF-8")
        }
    }

    capabilities
}

/// Result of evaluating all requirements for a test
#[derive(Debug, Clone)]
pub struct TestEvaluationResult {
    /// Name of the test being evaluated
    pub test_name: String,
    /// Overall result: can the test be run?
    pub can_run: bool,
}

impl TestEvaluationResult {
    /// Create a new result indicating the test can run (no requirements)
    pub fn new(test_name: &str) -> Self {
        Self {
            test_name: test_name.to_string(),
            can_run: true,
        }
    }
}

/// Container for test requirements that can be evaluated
pub struct TestCaseRequirements {
    requirements: TestRequirement,
}

impl TestCaseRequirements {
    /// Create a new TestCaseRequirements from a TestRequirement
    pub fn new(requirements: TestRequirement) -> Self {
        Self { requirements }
    }
}

/// Evaluates if a test case can be run in the current execution environment with context.
pub fn can_run_test_with_context(
    config: Option<&TestCaseRequirements>,
    context: &HostContext,
) -> bool {
    if let Some(config) = config {
        config.requirements.is_satisfied(context)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_context(openvmm_hypervisor: OpenVmmHypervisor) -> HostContext {
        HostContext {
            vm_host_info: None,
            vendor: Vendor::Intel,
            execution_environment: ExecutionEnvironment::Baremetal,
            vpci_supported: false,
            openvmm_hypervisor: Some(openvmm_hypervisor),
        }
    }

    #[test]
    fn capabilities_are_evaluated_for_the_selected_vmm() {
        let requirement = |vmm| TestRequirement::RequiresCapability {
            name: capabilities::WINDOWS_PARTITION_RESET,
            vmm,
        };
        let mshv = host_context(OpenVmmHypervisor::Mshv);

        assert!(!requirement(VmmType::OpenVmm).is_satisfied(&mshv));
        assert!(requirement(VmmType::HyperV).is_satisfied(&mshv));
        assert!(requirement(VmmType::OpenVmm).is_satisfied(&host_context(OpenVmmHypervisor::Kvm)));
    }
}
