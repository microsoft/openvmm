// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent guest listener used to test virtio-vsock transport resets.

use anyhow::Context;
use std::io::Read;
use std::io::Write;
use vmsocket::VmAddress;
use vmsocket::VmListener;

pub fn run(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let port: u32 = args
        .next()
        .context("missing vsock listener port")?
        .parse()?;
    anyhow::ensure!(port != 0 && port != u32::MAX, "invalid vsock listener port");
    anyhow::ensure!(args.next().is_none(), "unexpected vsock listener argument");
    let listener = VmListener::bind(VmAddress::vsock_any(port))?;
    println!("listening {}", std::process::id());
    std::io::stdout().flush()?;
    let mut connections = 0;
    loop {
        let (mut stream, _) = listener.accept()?;
        connections += 1;
        println!("accepted {connections}");
        std::io::stdout().flush()?;
        let mut buffer = [0; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    println!("eof {connections}");
                    break;
                }
                Ok(size) => stream.write_all(&buffer[..size])?,
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                    println!("reset {connections}");
                    break;
                }
                Err(err) => return Err(err).context("reading vsock listener connection"),
            }
        }
        std::io::stdout().flush()?;
    }
}
