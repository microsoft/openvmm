// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Result;

/// Generate copilot-setup-steps.yml for GitHub Copilot Coding Agent
#[derive(clap::Args)]
pub struct CopilotSetupGeneratorCli {}

impl CopilotSetupGeneratorCli {
    pub fn run(self, repo_root: &std::path::Path) -> Result<()> {
        let output_file = repo_root.join(".github/workflows/copilot-setup-steps.yaml");

        // Ensure the output directory exists
        if let Some(parent) = output_file.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = generate_copilot_setup_steps();

        std::fs::write(&output_file, content)?;

        println!("Generated .github/workflows/copilot-setup-steps.yaml");
        Ok(())
    }
}

fn generate_copilot_setup_steps() -> String {
    crate::pipelines_shared::copilot_setup_steps_template::get_template()
}
