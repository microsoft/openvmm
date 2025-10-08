// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use pal_async::DefaultPool;
use pal_async::socket::PolledSocket;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

include!(concat!(env!("OUT_DIR"), "/proto.rs"));

fn do_fuzz(input: &[u8]) {
    fuzz_eprintln!("{input:X?}");

    let (_cancel_send, cancel) = mesh::oneshot();
    let mut server = mesh_rpc::Server::new();

    let mut recv = server.add_service();

    DefaultPool::run_with(async |driver| {
        let control_listener = tempfile::Builder::new()
            .make(|path| UnixListener::bind(path))
            .unwrap();
        let mut control_sender = PolledSocket::new(
            &driver,
            UnixStream::connect(control_listener.path()).unwrap(),
        )
        .unwrap();

        control_sender.write_all(input).await.unwrap();
        fuzz_eprintln!("Wrote {:?} bytes", input.len());
        control_sender.close().await.unwrap();

        let mut server = std::pin::pin!(server.run(&driver, control_listener.as_file(), cancel));

        loop {
            let mut data = Vec::new();
            let msg = futures::select_biased! {
                _ = server.as_mut().fuse() => unreachable!("server should never complete"),
                msg = recv.next().fuse() => msg,
                _ = control_sender.read_to_end(&mut data).fuse() => break,

            };
            // Leave all the branches in for use by coverage tools.
            match msg {
                Some((_c, msg)) => match msg {
                    S::A(_, _) => {}
                    S::B(_, _) => {}
                    S::C(_, _) => {}
                    S::D(_, _) => {}
                    S::E(_, _) => {}
                    S::F(_, _) => {}
                    S::G(_, _) => {}
                    S::H(_, _) => {}
                    S::I(_, _) => {}
                    S::J(_, _) => {}
                    S::K(_, _) => {}
                    S::L(_, _) => {}
                    S::M(_, _) => {}
                    S::N(_, _) => {}
                    S::O(_, _) => {}
                    S::P(_, _) => {}
                    S::Q(_, _) => {}
                    S::R(_, _) => {}
                    S::T(_, _) => {}
                    S::U(_, _) => {}
                    S::V(_, _) => {}
                    S::W(_, _) => {}
                },
                None => panic!("closed"),
            }
        }
    });
}

fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});
