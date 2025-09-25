// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(all(windows))]
use crate::vm::hyperv::powershell;
use serde::Deserialize;
use serde::Serialize;

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

/// Types of isolation supported.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq)]
#[serde(try_from = "i32")]
pub enum IsolationType {
    /// Trusted Launch (OpenHCL, SecureBoot, TPM)
    TrustedLaunch = 0,
    /// VBS
    Vbs = 1,
    /// SNP
    Snp = 2,
    /// TDX
    Tdx = 3,
    /// OpenHCL but no isolation
    OpenHCL = 16,
    /// No HCL and no isolation
    Disabled = -1,
}

impl TryFrom<i32> for IsolationType {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            -1 => Ok(IsolationType::Disabled),
            0 => Ok(IsolationType::TrustedLaunch),
            1 => Ok(IsolationType::Vbs),
            2 => Ok(IsolationType::Snp),
            3 => Ok(IsolationType::Tdx),
            16 => Ok(IsolationType::OpenHCL),
            _ => Err(format!("Unknown isolation type: {}", value)),
        }
    }
}

/// VMM implementation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmmType {
    /// OpenVMM.
    OpenVmm,
    /// Microsoft Hyper-V.
    HyperV,
}

/// Platform-specific host context extending the base HostContext
#[derive(Debug, Clone)]
pub struct HostContext {
    /// VmHost information retrieved via PowerShell
    pub vm_host_info: Option<HyperVGetVmHost>,
    /// CPU vendor
    pub vendor: Vendor,
    /// Execution environment
    pub execution_environment: ExecutionEnvironment,
}

impl HostContext {
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    /// Create a new host context by querying host information
    pub async fn new() -> Self {
        let is_nested = {
            let result =
                safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, 0);
            hvdef::HvEnlightenmentInformation::from(
                result.eax as u128
                    | (result.ebx as u128) << 32
                    | (result.ecx as u128) << 64
                    | (result.edx as u128) << 96,
            )
            .nested()
        };

        let vendor = {
            let result =
                safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VendorAndMaxFunction.0, 0);
            if x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx)
                .is_amd_compatible()
            {
                Vendor::Amd
            } else {
                assert!(
                    x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx)
                        .is_intel_compatible()
                );
                Vendor::Intel
            }
        };

        Self {
            #[cfg(windows)]
            vm_host_info: powershell::run_get_vm_host().await.ok(),
            #[cfg(not(windows))]
            vm_host_info: None,
            vendor,
            execution_environment: if is_nested {
                ExecutionEnvironment::Nested
            } else {
                ExecutionEnvironment::Baremetal
            },
        }
    }

    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    /// Create a new host context by querying host information
    pub async fn new() -> Self {
        let is_nested = false;
        let vendor = Vendor::Arm;
        Self {
            vm_host_info: None,
            vendor,
            execution_environment: if is_nested {
                ExecutionEnvironment::Nested
            } else {
                ExecutionEnvironment::Baremetal
            },
        }
    }
}

/// A single requirement for a test to run.
pub enum TestRequirement {
    /// No specific requirements.
    None,
    /// Execution environment requirement.
    ExecutionEnvironment(ExecutionEnvironment),
    /// Vendor requirement.
    Vendor(Vendor),
    /// Isolation requirement.
    Isolation(IsolationType),
    /// VMM type requirement.
    VmmType(VmmType),
    /// Logical AND of two requirements.
    And(Box<TestRequirement>, Box<TestRequirement>),
    /// Logical OR of two requirements.
    Or(Box<TestRequirement>, Box<TestRequirement>),
    /// Logical NOT of a requirement.
    Not(Box<TestRequirement>),
}

impl TestRequirement {
    /// Evaluate if this requirement is satisfied with the given host context
    pub fn is_satisfied(&self, context: &HostContext) -> bool {
        match self {
            TestRequirement::None => true,
            TestRequirement::ExecutionEnvironment(env) => context.execution_environment == *env,
            TestRequirement::Vendor(vendor) => context.vendor == *vendor,
            TestRequirement::Isolation { isolation_type, .. } => {
                if let Some(vm_host_info) = &context.vm_host_info {
                    match isolation_type {
                        IsolationType::Vbs => vm_host_info
                            .guest_isolation_types
                            .contains(&IsolationType::Vbs),
                        IsolationType::Snp => vm_host_info.snp_status,
                        IsolationType::Tdx => vm_host_info.tdx_status,
                        IsolationType::TrustedLaunch => false,
                        IsolationType::OpenHCL => false,
                        IsolationType::Disabled => false,
                    }
                } else {
                    false
                }
            }
            TestRequirement::And(req1, req2) => {
                req1.is_satisfied(context) && req2.is_satisfied(context)
            }
            TestRequirement::Or(req1, req2) => {
                req1.is_satisfied(context) || req2.is_satisfied(context)
            }
            TestRequirement::Not(req) => !req.is_satisfied(context),
        }
    }
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
pub fn can_run_test_with_context(config: &TestCaseRequirements, context: &HostContext) -> bool {
    config.requirements.is_satisfied(context)
}
