// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::common_yaml::ADO_YAML_MAX_BYTES;
use super::common_yaml::GENERATED_YAML_HEADER;
use super::common_yaml::serialize_yaml;
use anyhow::Context;
use serde_yaml::Mapping;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

fn template_header(pipeline_file: &Path) -> anyhow::Result<String> {
    let name = pipeline_file
        .file_name()
        .and_then(|name| name.to_str())
        .context("pipeline filename must be UTF-8")?;
    Ok(format!(
        "{GENERATED_YAML_HEADER}# Step template for {name}\n"
    ))
}

pub(super) fn referenced_template_filenames(pipeline: &Value) -> BTreeSet<String> {
    fn collect(value: &Value, filenames: &mut BTreeSet<String>) {
        match value {
            Value::Mapping(mapping) => {
                if let Some(template) = mapping.get("template").and_then(Value::as_str) {
                    // Reserve basenames conservatively, including other repositories.
                    if let Some(name) = Path::new(template.split('@').next().unwrap())
                        .file_name()
                        .and_then(|name| name.to_str())
                    {
                        filenames.insert(name.to_owned());
                    }
                }
                for child in mapping.values() {
                    collect(child, filenames);
                }
            }
            Value::Sequence(sequence) => {
                for child in sequence {
                    collect(child, filenames);
                }
            }
            _ => {}
        }
    }

    let mut filenames = BTreeSet::new();
    collect(pipeline, &mut filenames);
    filenames
}

fn contains_template_expression(value: &Value) -> bool {
    match value {
        Value::String(value) => value.contains("${{"),
        Value::Mapping(mapping) => mapping.iter().any(|(key, value)| {
            contains_template_expression(key) || contains_template_expression(value)
        }),
        Value::Sequence(sequence) => sequence.iter().any(contains_template_expression),
        Value::Tagged(_) => true,
        _ => false,
    }
}

struct StepTemplates<'a> {
    pipeline_file: &'a Path,
    stem: &'a str,
    header: String,
    reserved: BTreeSet<String>,
    next_template: usize,
    files: BTreeMap<PathBuf, String>,
}

impl StepTemplates<'_> {
    fn flush(&mut self, steps: &mut Vec<Value>, output: &mut Vec<Value>) -> anyhow::Result<()> {
        if steps.is_empty() {
            return Ok(());
        }
        let filename = loop {
            let filename = format!("{}.steps-{}.yaml", self.stem, self.next_template);
            self.next_template += 1;
            if !self.reserved.contains(&filename) {
                break filename;
            }
        };
        let template = Value::Mapping(Mapping::from_iter([(
            "steps".into(),
            std::mem::take(steps).into(),
        )]));
        let yaml = serialize_yaml(&template)?;
        self.files.insert(
            self.pipeline_file.with_file_name(&filename),
            format!(
                "{}{}",
                self.header,
                yaml.strip_prefix(GENERATED_YAML_HEADER).unwrap()
            ),
        );
        output.push(Value::Mapping(Mapping::from_iter([(
            "template".into(),
            filename.into(),
        )])));
        Ok(())
    }

    fn steps(&mut self, container: &mut Value) -> anyhow::Result<()> {
        let Some(steps) = container.get_mut("steps").and_then(Value::as_sequence_mut) else {
            return Ok(());
        };
        let mut output = Vec::new();
        let mut group = Vec::new();
        let base_size = self.header.len() + "steps:\n".len();
        let mut size = base_size;
        for step in std::mem::take(steps) {
            // Template expansion changes compile-time variable scope. Keep all
            // expressions and existing includes in their original context.
            if step.get("template").is_some() || contains_template_expression(&step) {
                self.flush(&mut group, &mut output)?;
                size = base_size;
                output.push(step);
                continue;
            }
            let step_size = serde_yaml::to_string(&[&step])?.len();
            if size + step_size > ADO_YAML_MAX_BYTES && !group.is_empty() {
                self.flush(&mut group, &mut output)?;
                size = base_size;
            }
            size += step_size;
            group.push(step);
        }
        self.flush(&mut group, &mut output)?;
        *steps = output;
        Ok(())
    }

    fn deployment_hooks(&mut self, value: &mut Value) -> anyhow::Result<()> {
        self.steps(value)?;
        if let Value::Mapping(mapping) = value {
            for (key, child) in mapping {
                if key.as_str() != Some("steps") {
                    self.deployment_hooks(child)?;
                }
            }
        }
        Ok(())
    }

    fn jobs(&mut self, container: &mut Value) -> anyhow::Result<()> {
        let Some(jobs) = container.get_mut("jobs") else {
            return Ok(());
        };
        for job in jobs.as_sequence_mut().context("jobs must be a sequence")? {
            if job.get("job").is_some() {
                self.steps(job)?;
            } else if job.get("deployment").is_some() {
                if let Some(strategy) = job.get_mut("strategy") {
                    self.deployment_hooks(strategy)?;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn extract_steps(
    pipeline: &mut Value,
    pipeline_file: &Path,
) -> anyhow::Result<BTreeMap<PathBuf, String>> {
    let mut templates = StepTemplates {
        pipeline_file,
        stem: pipeline_file
            .file_stem()
            .and_then(|name| name.to_str())
            .context("pipeline filename must be UTF-8")?,
        header: template_header(pipeline_file)?,
        reserved: referenced_template_filenames(pipeline),
        next_template: 0,
        files: BTreeMap::new(),
    };
    templates.jobs(pipeline)?;
    if let Some(stages) = pipeline.get_mut("stages") {
        for stage in stages
            .as_sequence_mut()
            .context("stages must be a sequence")?
        {
            templates.jobs(stage)?;
        }
    }
    if let Some(stages) = pipeline
        .get_mut("extends")
        .and_then(|extends| extends.get_mut("parameters"))
        .and_then(|parameters| parameters.get_mut("stages"))
    {
        for stage in stages
            .as_sequence_mut()
            .context("stages must be a sequence")?
        {
            templates.jobs(stage)?;
        }
    }
    anyhow::ensure!(
        templates.files.len() < 100,
        "generated step templates plus the root pipeline exceed Azure DevOps' 100 YAML file limit"
    );
    Ok(templates.files)
}

pub(super) fn check_can_write_templates(
    files: &BTreeMap<PathBuf, String>,
    pipeline_file: &Path,
    repo_root: &Path,
) -> anyhow::Result<()> {
    let header = template_header(pipeline_file)?;
    for path in files.keys().filter(|path| *path != pipeline_file) {
        let path = repo_root.join(path);
        let metadata = match fs_err::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            metadata.is_file() && fs_err::read_to_string(&path)?.starts_with(&header),
            "refusing to overwrite '{}' because it is not a step template generated for '{}'",
            path.display(),
            pipeline_file.display(),
        );
    }
    Ok(())
}

pub(super) fn stale_templates(
    pipeline_file: &Path,
    directory: &Path,
    expected: &BTreeSet<String>,
) -> anyhow::Result<Vec<PathBuf>> {
    let prefix = format!(
        "{}.steps-",
        pipeline_file
            .file_stem()
            .and_then(|name| name.to_str())
            .context("pipeline filename must be UTF-8")?
    );
    let header = template_header(pipeline_file)?;
    let directory = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    };
    let entries = match fs_err::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut stale = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let generated_name = name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".yaml"))
            .is_some_and(|index| index.parse::<usize>().is_ok());
        if generated_name
            && entry.file_type()?.is_file()
            && !expected.contains(name)
            && fs_err::read_to_string(&path)?.starts_with(&header)
        {
            stale.push(path);
        }
    }
    stale.sort();
    Ok(stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn expand(value: &mut Value, files: &BTreeMap<PathBuf, String>) {
        match value {
            Value::Mapping(mapping) => {
                for child in mapping.values_mut() {
                    expand(child, files);
                }
            }
            Value::Sequence(sequence) => {
                let mut output = Vec::new();
                for mut child in std::mem::take(sequence) {
                    if let Some(yaml) = child
                        .get("template")
                        .and_then(Value::as_str)
                        .and_then(|name| files.get(&Path::new("ci").join(name)))
                    {
                        let template: Value = serde_yaml::from_str(yaml).unwrap();
                        assert!(template.get("parameters").is_none());
                        output.extend(template["steps"].as_sequence().unwrap().iter().cloned());
                    } else {
                        expand(&mut child, files);
                        output.push(child);
                    }
                }
                *sequence = output;
            }
            _ => {}
        }
    }

    #[test]
    fn templates_preserve_jobs_parameters_variables_and_pipeline_metadata() {
        let jobs: Value = serde_yaml::from_str(
            r#"
- job: build
  pool: {vmImage: ubuntu-latest}
  variables: {local: value}
  steps:
  - bash: echo first
  - bash: echo "${{ join(',', parameters.regions) }}"
  - bash: echo "${{ variables.local }}"
  - ${{ if parameters.enabled }}:
    - bash: echo conditional
  - template: steps/shared.yaml
    parameters: {message: "${{ variables.global }}"}
  - bash: echo "$(Build.BuildId)"
- job: test
  dependsOn: [build]
  condition: and(succeeded(), ${{ parameters.enabled }})
  steps:
  - bash: echo test
- template: shared/external.yaml
  parameters: {other: value}
"#,
        )
        .unwrap();
        for layout in ["jobs", "stages", "extends"] {
            let mut original: Value = serde_yaml::from_str(
                "trigger: none\nvariables: {global: value}\nparameters:\n- name: regions\n  type: stringList\n  default: [east]\n  values: [east, west]\n- name: enabled\n  type: boolean\n  default: true",
            ).unwrap();
            match layout {
                "jobs" => original["jobs"] = jobs.clone(),
                "stages" => {
                    original["stages"] = serde_yaml::from_str(
                        "- stage: first\n  variables: {stageVar: value}\n- stage: second\n  dependsOn: [first]",
                    ).unwrap();
                    original["stages"][0]["jobs"] = jobs.clone();
                    original["stages"][1]["jobs"] = jobs.clone();
                }
                "extends" => {
                    original["extends"] = serde_yaml::from_str(
                        "template: shared/base.yaml\nparameters:\n  stages:\n  - stage: first",
                    )
                    .unwrap();
                    original["extends"]["parameters"]["stages"][0]["jobs"] = jobs.clone();
                }
                _ => unreachable!(),
            }
            let mut split = original.clone();
            let files = extract_steps(&mut split, Path::new("ci/example.yaml")).unwrap();
            let mut again = original.clone();
            assert_eq!(
                files,
                extract_steps(&mut again, Path::new("ci/example.yaml")).unwrap()
            );
            assert_eq!(split, again);
            assert_eq!(files.len(), if layout == "stages" { 6 } else { 3 });
            for yaml in files.values() {
                assert!(!yaml.contains("${{"));
                assert!(!yaml.contains("parameters:"));
            }
            expand(&mut split, &files);
            assert_eq!(split, original);
        }
    }

    #[test]
    fn generated_templates_respect_the_include_count_limit() {
        let mut pipeline = Value::Mapping(Mapping::new());
        pipeline["jobs"] = (0..100)
            .map(|index| {
                Value::Mapping(Mapping::from_iter([
                    ("job".into(), format!("job{index}").into()),
                    (
                        "steps".into(),
                        serde_yaml::from_str("[{bash: echo}]").unwrap(),
                    ),
                ]))
            })
            .collect::<Vec<_>>()
            .into();
        let path = Path::new("ci/example.yaml");
        assert!(extract_steps(&mut pipeline.clone(), path).is_err());
        pipeline["jobs"].as_sequence_mut().unwrap().pop();
        assert_eq!(extract_steps(&mut pipeline, path).unwrap().len(), 99);
    }

    #[test]
    fn existing_template_references_reserve_generated_filenames() {
        let mut pipeline: Value = serde_yaml::from_str(
            "jobs:\n- job: build\n  steps:\n  - template: ./example.steps-0.yaml\n  - bash: echo literal\n  - template: /ci/example.steps-1.yaml@self\n  - template: example.steps-2.yaml",
        ).unwrap();
        let original = pipeline.clone();
        let files = extract_steps(&mut pipeline, Path::new("ci/example.yaml")).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files.contains_key(Path::new("ci/example.steps-3.yaml")));
        expand(&mut pipeline, &files);
        assert_eq!(pipeline, original);
    }

    #[test]
    fn large_step_groups_split_without_reordering_steps() {
        let mut pipeline: Value =
            serde_yaml::from_str("jobs:\n- job: build\n  steps: [{bash: echo}, {bash: echo}]")
                .unwrap();
        for step in pipeline["jobs"][0]["steps"].as_sequence_mut().unwrap() {
            step["bash"] = "é".repeat(150_000).into();
        }
        let original = pipeline.clone();
        let files = extract_steps(&mut pipeline, Path::new("ci/example.yaml")).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.values().all(|yaml| yaml.len() <= ADO_YAML_MAX_BYTES));
        expand(&mut pipeline, &files);
        assert_eq!(pipeline, original);
    }

    #[test]
    fn deployment_hooks_preserve_scope_and_step_order() {
        let mut pipeline: Value = serde_yaml::from_str(
            "jobs:\n- deployment: deploy\n  environment: example\n  strategy:\n    runOnce:\n      preDeploy:\n        steps: [{bash: prepare}]\n      deploy:\n        steps: [{bash: deploy}]\n      on:\n        failure:\n          steps: [{bash: cleanup}]",
        ).unwrap();
        let original = pipeline.clone();
        let files = extract_steps(&mut pipeline, Path::new("ci/example.yaml")).unwrap();
        assert_eq!(files.len(), 3);
        expand(&mut pipeline, &files);
        assert_eq!(pipeline, original);
    }

    #[test]
    fn expression_only_and_empty_step_lists_remain_inline() {
        let mut pipeline: Value = serde_yaml::from_str(
            "jobs:\n- job: expression\n  steps: ${{ parameters.steps }}\n- job: empty\n  steps: []\n- job: conditional\n  steps:\n  - ${{ if parameters.enabled }}:\n    - bash: echo conditional\n  - bash: echo\n    env: {MESSAGE: '${{ variables.message }}'}",
        ).unwrap();
        let original = pipeline.clone();
        assert!(
            extract_steps(&mut pipeline, Path::new("ci/example.yaml"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(pipeline, original);
    }
}
