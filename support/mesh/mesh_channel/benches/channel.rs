// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use criterion::async_executor::FuturesExecutor;
use criterion::criterion_group;
use criterion::criterion_main;
use criterion::Criterion;
use mesh_channel::OneshotSender;
use mesh_channel::Sender;
use mesh_node::local_node::Port;

fn bench_channel(c: &mut Criterion) {
    c.bench_function("channel_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || mesh_channel::channel::<u64>(),
            |(send, mut recv)| async move {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("channel_rt_large", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || mesh_channel::channel::<[u8; 1000]>(),
            |(send, mut recv)| async move {
                send.send([20; 1000]);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("channel_rt_large_boxed", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || mesh_channel::channel::<Box<[u8; 1000]>>(),
            |(send, mut recv)| async move {
                send.send(Box::new([20; 1000]));
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("channel_rt_through_ports", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::channel::<u64>();
                let send = Sender::<u64>::from(Port::from(send));
                (send, recv)
            },
            |(send, mut recv)| async move {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("channel_rt_through_ports_force_serialize", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::channel::<u64>();
                let send = Sender::<i64>::from(Port::from(send));
                (send, recv)
            },
            |(send, mut recv)| async move {
                send.send(20);
                recv.recv().await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("channel_and_rt", |b| {
        b.to_async(FuturesExecutor).iter(|| async move {
            let (send, mut recv) = mesh_channel::channel::<u64>();
            send.send(20);
            recv.recv().await.unwrap();
        });
    });
    c.bench_function("oneshot_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || mesh_channel::oneshot::<u64>(),
            |(send, recv)| async move {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("oneshot_rt_through_ports", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                let send = OneshotSender::<u64>::from(Port::from(send));
                (send, recv)
            },
            |(send, recv)| async move {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("oneshot_rt_through_ports_force_serialize", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                let send = OneshotSender::<i64>::from(Port::from(send));
                (send, recv)
            },
            |(send, recv)| async move {
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    })
    .bench_function("oneshot_and_rt", |b| {
        b.to_async(FuturesExecutor).iter_batched(
            || (),
            |()| async move {
                let (send, recv) = mesh_channel::oneshot::<u64>();
                send.send(20);
                recv.await.unwrap();
            },
            criterion::BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, bench_channel);
criterion_main!(benches);
