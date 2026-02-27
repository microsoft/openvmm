// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Get the version of a cargo crate

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub path: ReadVar<PathBuf>,
        pub version: WriteVar<Option<String>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { path, version } = request;

        ctx.emit_rust_step("get cargo crate version", |ctx| {
            let path = path.claim(ctx);
            let write_version = version.claim(ctx);

            move |rt| {
                let path = rt.read(path).join("Cargo.toml");
                let toml = fs_err::read_to_string(&path)
                    .context("failed to read Cargo.toml")?
                    .parse::<toml_edit::DocumentMut>()
                    .context("failed to parse Cargo.toml")?;
                let package = toml.get("package").context("no package section")?;
                let name = package
                    .get("name")
                    .context("missing name")?
                    .as_str()
                    .context("invalid name")?
                    .to_owned();
                let version = package
                    .get("version")
                    .map(|x| {
                        Ok::<String, anyhow::Error>(
                            x.as_str().context("invalid version")?.to_owned(),
                        )
                    })
                    .transpose()?;
                log::info!("package {name} has version {version:?}");
                rt.write(write_version, &version);

                Ok(())
            }
        });

        Ok(())
    }
}
