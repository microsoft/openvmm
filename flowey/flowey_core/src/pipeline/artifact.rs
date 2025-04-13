// UNSAFETY: using internal macros which use linkme.
#![expect(unsafe_code)]

use anyhow::Context as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

pub trait Artifact: Serialize + DeserializeOwned {}

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
                fs_err::copy(src_path, &path)?;
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
        if path.is_dir() {
            let value = fs_to_json(&path)?;
            map.insert(file_name, value);
        } else if path.is_file() {
            let path = path
                .into_os_string()
                .into_string()
                .ok()
                .context("non-utf8 path")?;
            let value = serde_json::Value::String(path);
            map.insert(file_name, value);
        }
    }
    Ok(serde_json::Value::Object(map))
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
            let Request { value, path, done } = request;

            ctx.emit_minor_rust_step("🌼 copy artifact contents", |ctx| {
                let path = path.claim(ctx);
                let value = value.claim(ctx);
                done.claim(ctx);
                |rt| {
                    let path = rt.read(path);
                    let value = rt.read(value);
                    super::json_to_fs(value, &path).expect("failed to copy artifact contents")
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
            result: WriteVar<serde_json::Value>,
        }
    }

    impl Request {
        pub fn new<T: Artifact>(path: ReadVar<PathBuf>, result: WriteVar<T>) -> Self {
            Self {
                path,
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
            let Request { path, result } = request;

            ctx.emit_minor_rust_step("🌼 resolve artifact", |ctx| {
                let path = path.claim(ctx);
                let result = result.claim(ctx);
                |rt| {
                    let path = rt.read(path);
                    let value = super::fs_to_json(&path).expect("failed to read artifact contents");
                    rt.write(result, &value);
                }
            });

            Ok(())
        }
    }
}
