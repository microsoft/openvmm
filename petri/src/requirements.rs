// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.

#[cfg(windows)]
use crate::vm::hyperv::powershell;

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
    pub vm_host_info: Option<powershell::HyperVGetVmHost>,
    /// CPU vendor
    pub vendor: Vendor,
    /// Execution environment
    pub execution_environment: ExecutionEnvironment,
}

impl HostContext {
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
            x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx)
        };
        Self {
            #[cfg(windows)]
            vm_host_info: powershell::run_get_vm_host().await.ok(),
            vendor: if vendor.is_amd_compatible() {
                Vendor::Amd
            } else {
                assert!(vendor.is_intel_compatible());
                Vendor::Intel
            },
            execution_environment: if is_nested {
                ExecutionEnvironment::Nested
            } else {
                ExecutionEnvironment::Baremetal
            },
        }
    }
}

/// Core trait for test requirements that can be evaluated at runtime
pub trait TestRequirement: Send + Sync + std::fmt::Debug {
    /// Unique identifier for this requirement type
    fn requirement_type(&self) -> &'static str;

    /// Evaluate if this requirement is met in the current environment
    fn is_satisfied(&self, context: &HostContext) -> anyhow::Result<bool>;

    /// Human-readable description of the requirement
    fn description(&self) -> String;

    /// Optional: detailed reason why requirement failed
    fn failure_reason(&self) -> Option<String> {
        None
    }
}

/// Result of evaluating a single requirement
#[derive(Debug, Clone)]
pub enum RequirementResult {
    /// Requirement was satisfied
    Satisfied(String),
    /// Requirement failed
    Failed {
        /// Type of the requirement that failed
        requirement_type: String,
        /// Optional reason for the failure
        reason: Option<String>,
    },
    /// Error occurred while evaluating the requirement
    Error {
        /// Type of the requirement that errored
        requirement_type: String,
        /// Error message
        error: String,
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
        let mut results = Vec::new();
        let mut can_run = true;

        for requirement in &self.requirements {
            match requirement.is_satisfied(context) {
                Ok(true) => {
                    results.push(RequirementResult::Satisfied(
                        requirement.requirement_type().to_string(),
                    ));
                }
                Ok(false) => {
                    can_run = false;
                    results.push(RequirementResult::Failed {
                        requirement_type: requirement.requirement_type().to_string(),
                        reason: requirement.failure_reason(),
                    });
                }
                Err(e) => {
                    can_run = false;
                    results.push(RequirementResult::Error {
                        requirement_type: requirement.requirement_type().to_string(),
                        error: format!("{:#}", e),
                    });
                }
            }
        }

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

impl Default for TestCaseRequirements {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for TestCaseRequirements {
    fn clone(&self) -> Self {
        // Note: This is a simplified clone that recreates an empty container
        // In practice, you might want to implement Clone for Box<dyn TestRequirement>
        // or use a different approach if cloning with requirements is needed
        Self::new()
    }
}

/// Execution environment requirements for test cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionEnvironmentRequirement {
    /// The required execution environment
    pub environment: ExecutionEnvironment,
}

impl ExecutionEnvironmentRequirement {
    /// Create a new execution environment requirement
    pub fn new(environment: ExecutionEnvironment) -> Self {
        Self { environment }
    }
}

/// CPU vendor requirements for test cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRequirement {
    /// The required CPU vendor
    pub vendor: Vendor,
}

impl VendorRequirement {
    /// Create a new CPU vendor requirement
    pub fn new(vendor: Vendor) -> Self {
        Self { vendor }
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

impl IsolationRequirement {
    /// Create a new isolation requirement
    pub fn new(isolation_type: IsolationType, vmm_type: VmmType) -> Self {
        Self {
            isolation_type,
            vmm_type,
        }
    }
}

impl TestRequirement for ExecutionEnvironmentRequirement {
    fn requirement_type(&self) -> &'static str {
        "ExecutionEnvironment"
    }

    fn is_satisfied(&self, context: &HostContext) -> anyhow::Result<bool> {
        Ok(context.execution_environment == self.environment)
    }

    fn description(&self) -> String {
        match self.environment {
            ExecutionEnvironment::Baremetal => {
                "Requires bare metal execution environment".to_string()
            }
            ExecutionEnvironment::Nested => {
                "Requires nested virtualization environment".to_string()
            }
        }
    }
}

impl TestRequirement for VendorRequirement {
    fn requirement_type(&self) -> &'static str {
        "CpuVendor"
    }

    fn is_satisfied(&self, context: &HostContext) -> anyhow::Result<bool> {
        Ok(context.vendor == self.vendor)
    }

    fn description(&self) -> String {
        match self.vendor {
            Vendor::Amd => "Requires AMD processor".to_string(),
            Vendor::Intel => "Requires Intel processor".to_string(),
        }
    }
}

impl TestRequirement for IsolationRequirement {
    fn requirement_type(&self) -> &'static str {
        "Isolation"
    }

    fn is_satisfied(&self, context: &HostContext) -> anyhow::Result<bool> {
        #[cfg(windows)]
        {
            assert!(
                context.vm_host_info.is_some(),
                "Host context must include VM host info on Windows"
            );
            let context = context.vm_host_info.as_ref().unwrap();
            let supported = match self.isolation_type {
                IsolationType::Vbs => context
                    .guest_isolation_types
                    .contains(&powershell::HyperVGuestStateIsolationType::Vbs),
                IsolationType::Snp => context.snp_status,
                IsolationType::Tdx => context.tdx_status,
            };

            Ok(supported)
        }
        #[cfg(not(windows))]
        {
            let _ = context; // Suppress unused parameter warning
            // On non-Windows platforms, isolation is not supported
            Ok(false)
        }
    }

    fn description(&self) -> String {
        let isolation_str = match self.isolation_type {
            IsolationType::Vbs => "VBS",
            IsolationType::Snp => "SNP",
            IsolationType::Tdx => "TDX",
        };
        let vmm_str = match self.vmm_type {
            VmmType::OpenVmm => "OpenVMM",
            VmmType::HyperV => "Hyper-V",
        };
        format!("Requires {} isolation with {} VMM", isolation_str, vmm_str)
    }
}

/// Evaluates if a test case can be run in the current execution environment with cached context.
///
/// This function determines whether a test should be run or ignored based on
/// the current environment conditions by evaluating all test requirements using
/// pre-computed host context to avoid repeated expensive queries.
///
/// # Arguments
/// * `test_name` - The name of the test case
/// * `config` - Optional test configuration containing requirements
/// * `context` - Pre-computed host context to use for evaluation
///
/// # Returns
/// * `true` if the test can be run in the current environment
/// * `false` if the test should be ignored
pub fn can_run_test_with_context(
    test_name: &str,
    config: Option<&TestCaseRequirements>,
    context: &HostContext,
) -> TestEvaluationResult {
    if let Some(requirements) = config {
        requirements.evaluate(test_name, context)
    } else {
        // No requirements means the test can run
        TestEvaluationResult::default(test_name)
    }
}
