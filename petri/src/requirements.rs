// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.

use petri_artifacts_common::capabilities;
use std::collections::BTreeSet;

/// Execution environments where tests can run.
///
/// This describes whether the *test runner itself* is running inside a VM. It
/// is distinct from the `nested_virt` capability, which describes whether the
/// runner can *host* nested VMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionEnvironment {
    /// The test runner is running on bare metal (not nested virtualization).
    Baremetal,
    /// The test runner is itself running inside a virtual machine.
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
    /// Whether the host hypervisor can run a guest with nested virtualization
    /// enabled (i.e. the guest itself can host a second-level VM).
    pub nested_virt_capable: bool,
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

        let vendor = {
            // xtask-fmt allow-target-arch cpu-intrinsic
            #[cfg(target_arch = "x86_64")]
            {
                let result = safe_intrinsics::cpuid(
                    x86defs::cpuid::CpuidFunction::VendorAndMaxFunction.0,
                    0,
                );
                if x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx)
                    .is_amd_compatible()
                {
                    Vendor::Amd
                } else {
                    assert!(
                        x86defs::cpuid::Vendor::from_ebx_ecx_edx(
                            result.ebx, result.ecx, result.edx
                        )
                        .is_intel_compatible()
                    );
                    Vendor::Intel
                }
            }
            // xtask-fmt allow-target-arch cpu-intrinsic
            #[cfg(not(target_arch = "x86_64"))]
            {
                Vendor::Arm
            }
        };

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

        let nested_virt_capable = nested_virt_capable_probe();

        Self {
            vm_host_info,
            vendor,
            execution_environment: if is_nested {
                ExecutionEnvironment::Nested
            } else {
                ExecutionEnvironment::Baremetal
            },
            vpci_supported,
            nested_virt_capable,
        }
    }
}

/// Probe whether the host can run a guest with nested virtualization.
fn nested_virt_capable_probe() -> bool {
    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/dev/kvm").exists() {
            return false;
        }
        let nested_enabled = |path: &str| -> bool {
            match std::fs::read_to_string(path) {
                Ok(s) => {
                    let v = s.trim();
                    v.eq_ignore_ascii_case("Y") || v == "1"
                }
                Err(_) => false,
            }
        };
        nested_enabled("/sys/module/kvm_intel/parameters/nested")
            || nested_enabled("/sys/module/kvm_amd/parameters/nested")
    }
    // WHP reports nested-virt support through a processor-feature capability
    // bit (x86-only).
    // xtask-fmt allow-target-arch oneoff-petri-host-arch
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        match whp::capabilities::processor_features() {
            Ok(features) => features
                .bank1
                .is_set(whp::abi::WHV_PROCESSOR_FEATURES1::NestedVirtSupport),
            Err(_) => false,
        }
    }
    // xtask-fmt allow-target-arch oneoff-petri-host-arch
    #[cfg(not(any(target_os = "linux", all(windows, target_arch = "x86_64"))))]
    {
        false
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
    RequiresCapability(&'static str),
    /// Requires a hypervisor backend that supports VPCI (virtual PCI)
    /// device emulation. On Linux this means /dev/mshv (not KVM).
    VpciSupport,
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
        self.is_satisfied_with_capabilities(context, &available_capabilities(context))
    }

    fn is_satisfied_with_capabilities(
        &self,
        context: &HostContext,
        capabilities: &BTreeSet<&'static str>,
    ) -> bool {
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
            TestRequirement::RequiresCapability(name) => capabilities.contains(name),
            TestRequirement::VpciSupport => context.vpci_supported,
            TestRequirement::And(req1, req2) => {
                req1.is_satisfied_with_capabilities(context, capabilities)
                    && req2.is_satisfied_with_capabilities(context, capabilities)
            }
            TestRequirement::Or(req1, req2) => {
                req1.is_satisfied_with_capabilities(context, capabilities)
                    || req2.is_satisfied_with_capabilities(context, capabilities)
            }
            TestRequirement::Not(req) => !req.is_satisfied_with_capabilities(context, capabilities),
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

fn available_capabilities(context: &HostContext) -> BTreeSet<&'static str> {
    let mut capabilities = BTreeSet::new();

    if context.vpci_supported {
        capabilities.insert(capabilities::VPCI);
    }

    if context.nested_virt_capable {
        capabilities.insert(capabilities::NESTED_VIRT);
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
