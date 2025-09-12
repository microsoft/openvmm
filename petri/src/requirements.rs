// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.

#[cfg(windows)]
use crate::vm::hyperv::powershell;
use std::fmt;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationType {
    /// Virtualization-Based Security.
    Vbs,
    /// AMD Secure Nested Paging.
    Snp,
    /// Intel Trust Domain Extensions.
    Tdx,
}

/// VMM implementation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmmType {
    /// OpenVMM hypervisor.
    OpenVmm,
    /// Microsoft Hyper-V.
    HyperV,
}

/// Platform-specific host context extending the base HostContext
#[derive(Debug, Clone)]
pub struct HostContext {
    #[cfg(windows)]
    /// VmHost information retrieved via PowerShell
    pub vm_host_info: Option<powershell::HyperVGetVmHost>,
    /// CPU vendor
    pub vendor: Vendor,
    /// Execution environment
    pub execution_environment: ExecutionEnvironment,
}

impl HostContext {
    /// Create a new host context by querying host information
    pub async fn new() -> Self {
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "x86_64")]
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
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(not(target_arch = "x86_64"))]
        let is_nested = false;
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "x86_64")]
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
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(not(target_arch = "x86_64"))]
        let vendor = Vendor::Unknown;

        Self {
            #[cfg(windows)]
            vm_host_info: powershell::run_get_vm_host().await.ok(),
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
pub trait TestRequirement: Send + Sync + fmt::Debug {
    /// Unique identifier for this requirement type
    fn requirement_type(&self) -> &'static str;

    /// Evaluate if this requirement is met in the current environment
    fn is_satisfied(&self, context: &HostContext) -> RequirementResult;
}

/// Result of evaluating a single requirement
#[derive(Debug, Clone)]
pub enum RequirementResult {
    /// Requirement was satisfied
    Satisfied,
    /// Requirement failed
    Failed {
        /// Type of the requirement that failed
        requirement_type: String,
        /// Optional reason for the failure
        reason: Option<String>,
    },
}

/// Result of evaluating all requirements for a test
#[derive(Debug, Clone)]
pub struct TestEvaluationResult {
    /// Name of the test being evaluated
    pub test_name: String,
    /// Detailed results for each requirement
    pub results: Option<Vec<RequirementResult>>,
    /// Overall result: can the test be run?
    pub can_run: bool,
}

impl TestEvaluationResult {
    /// Create a default result indicating the test can run (no requirements)
    pub fn default(test_name: &str) -> Self {
        Self {
            test_name: test_name.to_string(),
            results: None,
            can_run: true,
        }
    }
}

/// Container for test requirements that can be evaluated
#[derive(Debug)]
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
        let results: Vec<RequirementResult> = self
            .requirements
            .iter()
            .map(|req| req.is_satisfied(context))
            .collect();

        let can_run = results
            .iter()
            .all(|r| matches!(r, RequirementResult::Satisfied));

        TestEvaluationResult {
            test_name: test_name.to_string(),
            results: Some(results),
            can_run,
        }
    }

    /// Get all requirements for inspection
    pub fn requirements(&self) -> &[Box<dyn TestRequirement>] {
        &self.requirements
    }
}

impl fmt::Display for TestCaseRequirements {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reqs: Vec<String> = self
            .requirements
            .iter()
            .map(|r| r.requirement_type().to_string())
            .collect();
        write!(f, "TestCaseRequirements({})", reqs.join(", "))
    }
}

impl Default for TestCaseRequirements {
    fn default() -> Self {
        Self::new()
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

    fn is_satisfied(&self, context: &HostContext) -> RequirementResult {
        if context.execution_environment == self.environment {
            RequirementResult::Satisfied
        } else {
            RequirementResult::Failed {
                requirement_type: self.requirement_type().to_string(),
                reason: Some(format!(
                    "Host environment {:?} does not match required {:?}",
                    context.execution_environment, self.environment
                )),
            }
        }
        // Ok(context.execution_environment == self.environment)
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

    fn is_satisfied(&self, context: &HostContext) -> RequirementResult {
        if context.vendor == self.vendor {
            RequirementResult::Satisfied
        } else {
            RequirementResult::Failed {
                requirement_type: self.requirement_type().to_string(),
                reason: Some(format!(
                    "Host vendor {:?} does not match required {:?}",
                    context.vendor, self.vendor
                )),
            }
        }
    }
}

/// Isolation requirements for test cases.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    fn is_satisfied(&self, context: &HostContext) -> RequirementResult {
        #[cfg(windows)]
        {
            let context = context
                .vm_host_info
                .as_ref()
                .expect("Host context must include VM host info on Windows");
            let supported = match self.isolation_type {
                IsolationType::Vbs => context
                    .guest_isolation_types
                    .contains(&powershell::HyperVGuestStateIsolationType::Vbs),
                IsolationType::Snp => context.snp_status,
                IsolationType::Tdx => context.tdx_status,
            };

            if supported {
                RequirementResult::Satisfied
            } else {
                RequirementResult::Failed {
                    requirement_type: self.requirement_type().to_string(),
                    reason: Some(format!(
                        "Host does not support required isolation type {:?}. Supported types: {:?}",
                        self.isolation_type, context.guest_isolation_types
                    )),
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = context;
            RequirementResult::Failed {
                requirement_type: self.requirement_type().to_string(),
                reason: Some(
                    "Isolation requirements are only supported on Windows hosts".to_string(),
                ),
            }
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
        TestEvaluationResult::default(test_name)
    }
}
