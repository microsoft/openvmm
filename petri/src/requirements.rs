// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test requirements framework for runtime test filtering.

#[cfg(windows)]
use crate::vm::hyperv::powershell;

/// Platform-specific host context extending the base HostContext
#[derive(Debug, Clone)]
pub struct HostContext {
    /// Windows-specific VM host information
    #[cfg(windows)]
    pub vm_host_info: Option<powershell::HyperVGetVmHost>,
}

impl HostContext {
    /// Create a new host context by querying host information
    pub async fn new() -> Self {
        Self {
            #[cfg(windows)]
            vm_host_info: powershell::run_get_vm_host().await.ok(),
        }
    }

    /// Create an empty host context (for testing or non-Windows platforms)
    pub fn empty() -> Self {
        Self {
            #[cfg(windows)]
            vm_host_info: None,
        }
    }
}

/// Core trait for test requirements that can be evaluated at runtime
pub trait TestRequirement: Send + Sync + std::fmt::Debug {
    /// Unique identifier for this requirement type
    fn requirement_type(&self) -> &'static str;

    /// Evaluate if this requirement is met in the current environment
    ///
    /// This method falls back to the context-aware version for backwards compatibility
    fn is_satisfied(&self) -> anyhow::Result<bool> {
        self.is_satisfied_with_context(&HostContext::empty())
    }

    /// Evaluate if this requirement is met in the current environment with cached context
    fn is_satisfied_with_context(&self, context: &HostContext) -> anyhow::Result<bool>;

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
    pub results: Vec<RequirementResult>,
    /// Overall result: can the test be run?
    pub can_run: bool,
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

    /// Evaluate all requirements and return comprehensive result
    pub fn evaluate(&self, test_name: &str) -> TestEvaluationResult {
        self.evaluate_with_context(test_name, &HostContext::empty())
    }

    /// Evaluate all requirements with cached host context and return comprehensive result
    pub fn evaluate_with_context(
        &self,
        test_name: &str,
        context: &HostContext,
    ) -> TestEvaluationResult {
        let mut results = Vec::new();
        let mut can_run = true;

        for requirement in &self.requirements {
            match requirement.is_satisfied_with_context(context) {
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
            results,
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

impl TestRequirement for ExecutionEnvironmentRequirement {
    fn requirement_type(&self) -> &'static str {
        "ExecutionEnvironment"
    }

    fn is_satisfied_with_context(&self, _context: &HostContext) -> anyhow::Result<bool> {
        // For now, return true as a placeholder
        // This should be implemented based on actual environment detection logic
        Ok(true)
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

    fn is_satisfied_with_context(&self, _context: &HostContext) -> anyhow::Result<bool> {
        Ok(true)
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

    fn is_satisfied_with_context(&self, context: &HostContext) -> anyhow::Result<bool> {
        #[cfg(windows)]
        {
            let host_info = context.vm_host_info.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Failed to retrieve VM host information on Windows")
            })?;

            let supported = match self.isolation_type {
                IsolationType::Vbs => host_info
                    .guest_isolation_types
                    .contains(&powershell::HyperVGuestStateIsolationType::Vbs),
                IsolationType::Snp => host_info.snp_status,
                IsolationType::Tdx => host_info.tdx_status,
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

/// Evaluates if a test case can be run in the current execution environment.
///
/// This function determines whether a test should be run or ignored based on
/// the current environment conditions by evaluating all test requirements.
///
/// # Arguments
/// * `test_name` - The name of the test case
/// * `config` - Optional test configuration containing requirements
///
/// # Returns
/// * `true` if the test can be run in the current environment
/// * `false` if the test should be ignored
pub fn can_run_test(test_name: &str, config: Option<&TestCaseRequirements>) -> bool {
    can_run_test_with_context(test_name, config, &HostContext::empty())
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
) -> bool {
    if let Some(requirements) = config {
        let evaluation = requirements.evaluate_with_context(test_name, context);
        evaluation.can_run
    } else {
        // No requirements means the test can run
        true
    }
}

/// Evaluates test requirements and returns detailed information about why
/// a test can or cannot run.
///
/// # Arguments
/// * `test_name` - The name of the test case
/// * `config` - Optional test configuration containing requirements
///
/// # Returns
/// * `TestEvaluationResult` with detailed requirement evaluation results
pub fn evaluate_test_requirements(
    test_name: &str,
    config: Option<&TestCaseRequirements>,
) -> Option<TestEvaluationResult> {
    config.map(|requirements| requirements.evaluate(test_name))
}
