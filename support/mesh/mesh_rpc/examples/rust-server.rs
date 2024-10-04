// Copyright (C) Microsoft Corporation. All rights reserved.

use anyhow::Context;
use futures::executor::block_on;
use pal_async::local::block_with_io;
use unix_socket::UnixListener;

mod items {
    // Generated types use these crates, reference them here to ensure they are
    // not removed by automated tooling.
    use prost as _;
    include!(concat!(env!("OUT_DIR"), "/ttrpc.example.v1.rs"));
}

fn main() {
    let mut args = std::env::args();
    args.next();
    if let Err(err) = server(&args.next().unwrap_or_else(|| "test-socket".to_owned())) {
        eprintln!("error: {:#}", err);
        std::process::exit(1);
    }
}

fn server(path: &str) -> anyhow::Result<()> {
    env_logger::init();
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).context("bind failed")?;
    tracing::info!(path, "listening");
    let mut server = mesh_rpc::Server::new();
    let (send, mut recv) = mesh::channel::<(mesh::CancelContext, items::Example)>();
    let (_s, stop_listening) = mesh::oneshot();
    server.add_service(send);
    let thread = std::thread::spawn(move || {
        block_with_io(
            |driver| async move { drop(server.run(&driver, listener, stop_listening).await) },
        )
    });
    block_on(async {
        while let Ok((_, message)) = recv.recv().await {
            match message {
                items::Example::Method1(req, response) => {
                    response.send(Ok(items::Method1Response {
                        foo: req.foo + "_fizz",
                        bar: req.bar + "_buzz",
                    }));
                }
                items::Example::Method2(_req, _response) => {}
            }
        }
        drop(recv);
    });
    let _ = thread.join();
    Ok(())
}
