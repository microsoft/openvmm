// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
        "{GENERATED_YAML_HEADER}# Job template for {name}\n"
    ))
}

pub(super) fn extract_jobs(
    pipeline: &mut Value,
    pipeline_file: &Path,
) -> anyhow::Result<BTreeMap<PathBuf, String>> {
    let stem = pipeline_file
        .file_stem()
        .and_then(|name| name.to_str())
        .context("pipeline filename must be UTF-8")?;
    let parameters = pipeline.get("parameters").cloned();
    let mut arguments = Mapping::new();
    if let Some(parameters) = &parameters {
        for parameter in parameters
            .as_sequence()
            .context("pipeline parameters must be a sequence")?
        {
            let name = parameter
                .get("name")
                .and_then(Value::as_str)
                .context("pipeline parameter must have a name")?;
            arguments.insert(
                name.into(),
                format!("${{{{ parameters['{}'] }}}}", name.replace('\'', "''")).into(),
            );
        }
    }

    let mut files = BTreeMap::new();
    let header = template_header(pipeline_file)?;
    let mut extract = |container: &mut Value| -> anyhow::Result<()> {
        let Some(jobs) = container.get_mut("jobs") else {
            return Ok(());
        };
        for job in jobs.as_sequence_mut().context("jobs must be a sequence")? {
            // Existing template references already have their own parameter scope.
            if job.get("template").is_some() {
                continue;
            }
            anyhow::ensure!(
                job.get("job").is_some() || job.get("deployment").is_some(),
                "job templates require explicit job or deployment entries"
            );
            // Keep templates beside the root so nested relative includes retain
            // their original meaning.
            let filename = format!("{stem}.job-{}.yaml", files.len());
            let mut reference = Mapping::new();
            reference.insert("template".into(), filename.clone().into());
            if !arguments.is_empty() {
                reference.insert("parameters".into(), arguments.clone().into());
            }
            let original_job = std::mem::replace(job, reference.into());
            let mut template = Mapping::new();
            if let Some(parameters) = &parameters {
                template.insert("parameters".into(), parameters.clone());
            }
            template.insert("jobs".into(), vec![original_job].into());
            let yaml = serialize_yaml(&template.into())?;
            files.insert(
                pipeline_file.with_file_name(filename),
                format!(
                    "{header}{}",
                    yaml.strip_prefix(GENERATED_YAML_HEADER).unwrap()
                ),
            );
        }
        Ok(())
    };

    extract(pipeline)?;
    if let Some(stages) = pipeline.get_mut("stages") {
        for stage in stages
            .as_sequence_mut()
            .context("stages must be a sequence")?
        {
            extract(stage)?;
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
            extract(stage)?;
        }
    }
    anyhow::ensure!(
        files.len() < 100,
        "generated job templates plus the root pipeline exceed Azure DevOps' 100 YAML file limit"
    );
    Ok(files)
}

pub(super) fn stale_templates(
    pipeline_file: &Path,
    directory: &Path,
    expected: &BTreeSet<PathBuf>,
) -> anyhow::Result<Vec<PathBuf>> {
    let prefix = format!(
        "{}.job-",
        pipeline_file
            .file_stem()
            .and_then(|name| name.to_str())
            .context("pipeline filename must be UTF-8")?
    );
    let header = template_header(pipeline_file)?;
    let expected: BTreeSet<_> = expected
        .iter()
        .filter_map(|path| path.file_name())
        .collect();
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
        let generated_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|name| name.strip_suffix(".yaml"))
            .is_some_and(|index| index.parse::<usize>().is_ok());
        if generated_name
            && entry.file_type()?.is_file()
            && !expected.contains(path.file_name().unwrap())
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
                if let Some(path) = mapping.get("template").and_then(Value::as_str) {
                    if let Some(yaml) = files.get(&Path::new("ci").join(path)) {
                        let template: Value = serde_yaml::from_str(yaml).unwrap();
                        assert_eq!(
                            mapping["parameters"]["enabled"],
                            "${{ parameters['enabled'] }}"
                        );
                        assert_eq!(template["parameters"][0]["type"], "boolean");
                        *value = template["jobs"][0].clone();
                        return;
                    }
                }
                for child in mapping.values_mut() {
                    expand(child, files);
                }
            }
            Value::Sequence(sequence) => {
                for child in sequence {
                    expand(child, files);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn templates_preserve_jobs_parameters_and_pipeline_metadata() {
        let parameters: Value = serde_yaml::from_str(
            r#"
- name: enabled
  type: boolean
  default: true
- name: flavor
  type: string
  default: debug
  values: [debug, release]
"#,
        )
        .unwrap();
        let jobs: Value = serde_yaml::from_str(
            r#"
- job: build
  pool: {vmImage: ubuntu-latest}
  steps:
  - bash: echo "${{ parameters.flavor }}"
  - template: steps/shared.yaml
- job: test
  dependsOn: [build]
  condition: and(succeeded(), ${{ parameters.enabled }})
  steps:
  - bash: echo "$(Build.BuildId)"
- template: shared/external.yaml
  parameters: {other: value}
"#,
        )
        .unwrap();
        for layout in ["jobs", "stages", "extends"] {
            let mut original: Value = serde_yaml::from_str(
                "trigger: none\nvariables: {example: value}\nresources: {repositories: []}",
            )
            .unwrap();
            original["parameters"] = parameters.clone();
            match layout {
                "jobs" => original["jobs"] = jobs.clone(),
                "stages" => {
                    original["stages"] = serde_yaml::from_str(
                        "- stage: first\n- stage: second\n  dependsOn: [first]",
                    )
                    .unwrap();
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
            let files = extract_jobs(&mut split, Path::new("ci/example.yaml")).unwrap();
            let mut again = original.clone();
            assert_eq!(
                files,
                extract_jobs(&mut again, Path::new("ci/example.yaml")).unwrap()
            );
            assert_eq!(split, again);
            assert_eq!(files.len(), if layout == "stages" { 4 } else { 2 });
            for yaml in files.values() {
                let template: Value = serde_yaml::from_str(yaml).unwrap();
                assert_eq!(template["parameters"], parameters);
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
                    ("steps".into(), Vec::<Value>::new().into()),
                ]))
            })
            .collect::<Vec<_>>()
            .into();
        let path = Path::new("example.yaml");
        assert!(extract_jobs(&mut pipeline.clone(), path).is_err());
        pipeline["jobs"].as_sequence_mut().unwrap().pop();
        assert_eq!(extract_jobs(&mut pipeline, path).unwrap().len(), 99);
    }
}
