// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use criterion::Criterion;
use criterion::async_executor::FuturesExecutor;
use criterion::criterion_group;
use criterion::criterion_main;
use mesh_channel::OneshotSender;
use mesh_channel::Sender;
use mesh_node::local_node::Port;

fn bench_channel(c: &mut Criterion) {
    c.bench_function("channel_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            mesh_channel::channel::<u64>,
            async |(send, mut recv)| {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("channel_rt_large", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            mesh_channel::channel::<[u8; 1000]>,
            async |(send, mut recv)| {
                send.send([20; 1000]);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("channel_rt_large_boxed", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            mesh_channel::channel::<Box<[u8; 1000]>>,
            async |(send, mut recv)| {
                send.send(Box::new([20; 1000]));
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("channel_rt_through_ports", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::channel::<u64>();
                let send = Sender::<u64>::from(Port::from(send));
                (send, recv)
            },
            async |(send, mut recv)| {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("channel_rt_through_ports_force_serialize", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::channel::<u64>();
                let send = Sender::<i64>::from(Port::from(send));
                (send, recv)
            },
            async |(send, mut recv)| {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("channel_and_rt", |b| {
        b.to_async(FuturesExecutor).iter(async || {
            let (send, mut recv) = mesh_channel::channel::<u64>();
            send.send(20);
            recv.recv().await.unwrap();
        });
    });
    c.bench_function("oneshot_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            mesh_channel::oneshot::<u64>,
            async |(send, recv)| {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("oneshot_rt_through_ports", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                let send = OneshotSender::<u64>::from(Port::from(send));
                (send, recv)
            },
            async |(send, recv)| {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("oneshot_rt_through_ports_force_serialize", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                let send = OneshotSender::<i64>::from(Port::from(send));
                (send, recv)
            },
            async |(send, recv)| {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    })
    .bench_function("oneshot_and_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || (),
            async |()| {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_channel);
criterion_main!(benches);
