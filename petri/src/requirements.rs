// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.
#[cfg(windows)]
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
    /// Unknown CPU vendor.
    Unknown,
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
    /// OpenVMM hypervisor.
    OpenVmm,
    /// Microsoft Hyper-V.
    HyperV,
}

/// Hyper-V Get VM Host Output
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct HyperVGetVmHost {
    /// GuestIsolationTypes supported on the host
    #[serde(rename = "GuestIsolationTypes")]
    pub guest_isolation_types: Vec<IsolationType>,
    /// Whether SNP is supported on the host
    #[serde(rename = "SnpStatus", deserialize_with = "int_to_bool")]
    pub snp_status: bool,
    /// Whether TDX is supported on the host
    #[serde(rename = "TdxStatus", deserialize_with = "int_to_bool")]
    pub tdx_status: bool,
}

fn int_to_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = i32::deserialize(deserializer)?;
    Ok(v == 1)
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
    /// Create a new host context by querying host information
    #[cfg(target_arch = "x86_64")]
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

    #[cfg(not(target_arch = "x86_64"))]
    pub async fn new() -> Self {
        let is_nested = false;
        let vendor = Vendor::Unknown;
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

/// Core trait for test requirements that can be evaluated at runtime
pub trait TestRequirement: Send + Sync {
    /// Unique identifier for this requirement type
    fn requirement_type(&self) -> &'static str;

    /// Evaluate if this requirement is met in the current environment
    fn is_satisfied(&self, context: &HostContext) -> bool;
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
    requirements: Vec<Box<dyn TestRequirement>>,
}

impl TestCaseRequirements {
    /// Create a new empty requirements container
    pub fn new() -> Self {
        Self {
            requirements: Vec::new(),
        }
    }

    /// Add a requirement to this test case
    pub fn require<R: TestRequirement + 'static>(mut self, requirement: R) -> Self {
        self.requirements.push(Box::new(requirement));
        self
    }

    /// Evaluate all requirements with cached host context and return comprehensive result
    pub fn evaluate(&self, test_name: &str, context: &HostContext) -> TestEvaluationResult {
        let can_run = self
            .requirements
            .iter()
            .all(|req| req.is_satisfied(context));

        TestEvaluationResult {
            test_name: test_name.to_string(),
            can_run,
        }
    }

    /// Get all requirements for inspection
    pub fn requirements(&self) -> &[Box<dyn TestRequirement>] {
        &self.requirements
    }
}

/// Execution environment requirements for test cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionEnvironmentRequirement {
    /// The required execution environment
    pub environment: ExecutionEnvironment,
}

impl TestRequirement for ExecutionEnvironmentRequirement {
    fn requirement_type(&self) -> &'static str {
        "ExecutionEnvironment"
    }

    fn is_satisfied(&self, context: &HostContext) -> bool {
        context.execution_environment == self.environment
    }
}

/// CPU vendor requirements for test cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRequirement {
    /// The required CPU vendor
    pub vendor: Vendor,
}

impl TestRequirement for VendorRequirement {
    fn requirement_type(&self) -> &'static str {
        "CpuVendor"
    }

    fn is_satisfied(&self, context: &HostContext) -> bool {
        context.vendor == self.vendor
    }
}

/// Isolation requirements for test cases.
#[derive(Debug, Clone)]
pub struct IsolationRequirement {
    /// The required isolation type
    pub isolation_type: IsolationType,
    /// The required VMM type
    pub vmm_type: VmmType,
}

impl TestRequirement for IsolationRequirement {
    fn requirement_type(&self) -> &'static str {
        "Isolation"
    }

    fn is_satisfied(&self, context: &HostContext) -> bool {
        #[cfg(windows)]
        {
            let context = context
                .vm_host_info
                .as_ref()
                .expect("Host context must include VM host info on Windows");
            match self.isolation_type {
                IsolationType::Vbs => context.guest_isolation_types.contains(&IsolationType::Vbs),
                IsolationType::Snp => context.snp_status,
                IsolationType::Tdx => context.tdx_status,
                IsolationType::Disabled => false,
                IsolationType::OpenHCL => false,
                IsolationType::TrustedLaunch => false,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = context;
            false
        }
    }
}

/// Evaluates if a test case can be run in the current execution environment with context.
pub fn can_run_test_with_context(
    test_name: &str,
    config: Option<&TestCaseRequirements>,
    context: &HostContext,
) -> TestEvaluationResult {
    if let Some(requirements) = config {
        requirements.evaluate(test_name, context)
    } else {
        // No requirements means the test can run if it's built.
        TestEvaluationResult::new(test_name)
    }
}
