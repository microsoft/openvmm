// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal nodes for publishing (well, preparing to publish) and resolving
//! artifacts.

// UNSAFETY: using internal macros which use linkme.
#![expect(unsafe_code)]

use anyhow::Context as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::path::PathBuf;

/// A trait representing a collection of files that can be published to or
/// resolved from a pipeline artifact.
///
/// This can be used with `publish_typed_artifact` and `resolve_typed_artifact`
/// to publish or resolve artifacts between jobs in a pipeline in a structured
/// way.
///
/// By implementing this trait, you are guaranteeing that the type serializes
/// into JSON in a format reflecting a directory structure, where each key is a
/// file name and each value is either a string containing the path to the file,
/// or another JSON object representing a subdirectory.
///
/// For example, you might have Rust types like this:
/// ```rust
/// # use serde::{Serialize, Deserialize};
/// # use std::path::PathBuf;
/// #[derive(Serialize, Deserialize)]
/// struct Artifact {
///     #[serde(rename = "file.exe")]
///     file: PathBuf,
///     subdir: Option<Inner>,
/// }
///
/// #[derive(Serialize, Deserialize)]
/// struct Inner {
///     #[serde(rename = "file2.exe")]
///     file2: PathBuf,
/// }
/// ```
///
/// This would serialize into JSON like this:
/// ```json
/// {
///    "file.exe": "path/to/file.exe",
///   "subdir": {
///       "file2.exe": "path/to/file2.exe"
///   }
/// }
/// ```
///
/// Which would in turn reflect a directory structure like this:
/// ```text
/// - file.exe
/// - subdir/
///   - file2.exe
/// ```
pub trait Artifact: Serialize + DeserializeOwned {
    /// If present, the published artifact should consist of a tar.gz file
    /// containing the contents of the artifact.
    ///
    /// This is mostly useful for artifacts with lots of files. Some backends
    /// (specifically Azure DevOps) apparently cannot cope with this.
    ///
    /// An alternate approach would be to detect this automatically, and/or to
    /// only do it for the affected backends. Currently, we don't bother with
    /// this complexity, preferring instead a predictable and consistent
    /// approach.
    const TAR_GZ_NAME: Option<&'static str> = None;
}

fn json_to_fs(value: serde_json::Value, path: &Path) -> anyhow::Result<()> {
    if let serde_json::Value::Object(map) = value {
        json_to_fs_inner(map, path)
    } else {
        anyhow::bail!("expected JSON object");
    }
}

fn json_to_fs_inner(
    value: serde_json::Map<String, serde_json::Value>,
    root: &Path,
) -> anyhow::Result<()> {
    for (key, value) in value {
        let path = root.join(key);
        match value {
            serde_json::Value::Object(map) => {
                fs_err::create_dir_all(&path)?;
                json_to_fs_inner(map, &path)?;
            }
            serde_json::Value::String(src_path) => {
                let src_path = Path::new(&src_path);
                if src_path.is_dir() {
                    crate::util::copy_dir_all(src_path, &path)?;
                    // Write a tag file so that `fs_to_json` knows that this is
                    // an opaque directory.
                    fs_err::File::create(tag_path(path))?;
                } else {
                    fs_err::copy(src_path, &path)?;
                }
            }
            _ => {
                anyhow::bail!("unsupported JSON value type");
            }
        }
    }
    Ok(())
}

fn fs_to_json(root: &Path) -> anyhow::Result<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for entry in fs_err::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry
            .file_name()
            .into_string()
            .ok()
            .context("non-utf8 filename")?;
        let recurse = if path.is_dir() {
            !tag_path(path.clone()).exists()
        } else if is_tag_path(&path) {
            continue;
        } else {
            false
        };

        let value = if recurse {
            fs_to_json(&path)?
        } else {
            let path = path
                .into_os_string()
                .into_string()
                .ok()
                .context("non-utf8 path")?;
            serde_json::Value::String(path)
        };
        map.insert(file_name, value);
    }
    Ok(serde_json::Value::Object(map))
}

fn is_tag_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".artifact-dir."))
}

fn tag_path(mut path: PathBuf) -> PathBuf {
    let file_name = path.file_name().unwrap().to_str().unwrap();
    path.set_file_name(format!(".artifact-dir.{file_name}"));
    path
}

pub mod publish {
    use super::Artifact;
    use crate::flowey_request;
    use crate::new_simple_flow_node;
    use crate::node::ClaimVar;
    use crate::node::ReadVar;
    use crate::node::SideEffect;
    use crate::node::SimpleFlowNode;
    use crate::node::WriteVar;
    use std::path::PathBuf;

    new_simple_flow_node!(struct Node);

    flowey_request! {
        pub struct Request {
            value: ReadVar<serde_json::Value>,
            path: ReadVar<PathBuf>,
            tar_gz_name: Option<String>,
            done: WriteVar<SideEffect>,
        }
    }

    impl Request {
        pub fn new<T: Artifact>(
            value: ReadVar<T>,
            path: ReadVar<PathBuf>,
            done: WriteVar<SideEffect>,
        ) -> Self {
            Self {
                value: value.into_json(),
                path,
                tar_gz_name: T::TAR_GZ_NAME.map(ToOwned::to_owned),
                done,
            }
        }
    }

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(_ctx: &mut crate::node::ImportCtx<'_>) {}

        fn process_request(
            request: Self::Request,
            ctx: &mut crate::node::NodeCtx<'_>,
        ) -> anyhow::Result<()> {
            let Request {
                value,
                path,
                tar_gz_name,
                done,
            } = request;

            ctx.emit_minor_rust_step("🌼 copy artifact contents", |ctx| {
                let path = path.claim(ctx);
                let value = value.claim(ctx);
                done.claim(ctx);
                |rt| {
                    let path = rt.read(path);
                    let value = rt.read(value);
                    if let Some(tar_gz_name) = tar_gz_name {
                        super::json_to_fs(value, ".".as_ref())
                            .expect("failed to copy artifact contents");
                        let tar_gz_path = path.join(tar_gz_name);
                        let r = std::process::Command::new("tar")
                            .arg("-acf")
                            .arg(&tar_gz_path)
                            .arg(".")
                            .output()
                            .expect("failed to launch tar");
                        if !r.status.success() {
                            panic!("failed to archive artifact contents: {r:?}");
                        }
                    } else {
                        super::json_to_fs(value, &path).expect("failed to copy artifact contents");
                    }
                }
            });
            Ok(())
        }
    }
}

pub mod resolve {
    use super::Artifact;
    use crate::flowey_request;
    use crate::new_simple_flow_node;
    use crate::node::ClaimVar;
    use crate::node::ReadVar;
    use crate::node::SimpleFlowNode;
    use crate::node::WriteVar;
    use std::path::PathBuf;

    new_simple_flow_node!(struct Node);

    flowey_request! {
        pub struct Request {
            path: ReadVar<PathBuf>,
            tar_gz_name: Option<String>,
            result: WriteVar<serde_json::Value>,
        }
    }

    impl Request {
        pub fn new<T: Artifact>(path: ReadVar<PathBuf>, result: WriteVar<T>) -> Self {
            Self {
                path,
                tar_gz_name: T::TAR_GZ_NAME.map(ToOwned::to_owned),
                result: result.into_json(),
            }
        }
    }

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(_ctx: &mut crate::node::ImportCtx<'_>) {}

        fn process_request(
            request: Self::Request,
            ctx: &mut crate::node::NodeCtx<'_>,
        ) -> anyhow::Result<()> {
            let Request {
                path,
                tar_gz_name,
                result,
            } = request;

            ctx.emit_minor_rust_step("🌼 resolve artifact", |ctx| {
                let path = path.claim(ctx);
                let result = result.claim(ctx);
                |rt| {
                    let path = rt.read(path);
                    let path = if let Some(tar_gz_name) = tar_gz_name {
                        let tar_gz_path = path.join(tar_gz_name);
                        let r = std::process::Command::new("tar")
                            .arg("-xf")
                            .arg(&tar_gz_path)
                            .output()
                            .expect("failed to launch tar");
                        if !r.status.success() {
                            panic!("failed to extract artifact contents: {r:?}");
                        }
                        ".".as_ref()
                    } else {
                        path.as_ref()
                    };
                    let value = super::fs_to_json(path).expect("failed to read artifact contents");
                    rt.write(result, &value);
                }
            });

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fs_to_json;
    use crate::pipeline::artifact::json_to_fs;
    use serde_json::Value;
    use std::path::Path;

    fn make_abs(root: &Path, value: Value) -> Value {
        match value {
            Value::String(v) => Value::String(
                std::path::absolute(root.join(v))
                    .unwrap()
                    .into_os_string()
                    .into_string()
                    .ok()
                    .unwrap(),
            ),
            Value::Array(values) => {
                Value::Array(values.into_iter().map(|v| make_abs(root, v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, make_abs(root, v)))
                    .collect(),
            ),
            v => v,
        }
    }

    #[test]
    fn test_fs_to_json() {
        let dir = tempfile::TempDir::new().unwrap();
        fs_err::write(dir.path().join("foo"), "").unwrap();
        fs_err::create_dir(dir.path().join("bar")).unwrap();
        fs_err::write(dir.path().join("bar/baz"), "").unwrap();
        fs_err::create_dir(dir.path().join("bar/quux")).unwrap();
        fs_err::write(dir.path().join("bar/.artifact-dir.quux"), "").unwrap();
        fs_err::write(dir.path().join("bar/quux/0"), "").unwrap();
        fs_err::write(dir.path().join("bar/quux/1"), "").unwrap();
        let json = fs_to_json(dir.path()).unwrap();
        let expected = make_abs(
            dir.path(),
            serde_json::json!({
                "foo": "foo",
                "bar": {
                    "baz": "bar/baz",
                    "quux": "bar/quux",
                }
            }),
        );
        assert_eq!(json, expected);
    }

    #[test]
    fn test_json_to_fs() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let f_path = f.path().to_str().unwrap();

        let d = tempfile::TempDir::new().unwrap();
        fs_err::write(d.path().join("foo"), "").unwrap();
        fs_err::create_dir(d.path().join("bar")).unwrap();
        fs_err::write(d.path().join("bar/baz"), "").unwrap();
        let d_path = d.path().to_str().unwrap();

        let json = serde_json::json!({
            "foo": f_path,
            "bar": {
                "baz": f_path,
                "quux": d_path,
            }
        });
        let dir = tempfile::TempDir::new().unwrap();
        json_to_fs(json, dir.path()).unwrap();
        let assert_exists = |p: &str| {
            let is_dir = p.ends_with('/');
            let m = fs_err::metadata(dir.path().join(p)).unwrap();
            if is_dir {
                assert!(m.is_dir(), "file {p} is not a directory");
            } else {
                assert!(m.is_file(), "file {p} is not a file");
            }
        };
        assert_exists("foo");
        assert_exists("bar/");
        assert_exists("bar/baz");
        assert_exists("bar/quux/");
        assert_exists("bar/.artifact-dir.quux");
    }
}
