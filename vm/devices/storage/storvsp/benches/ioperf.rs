// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! StorVSP IO loop performance testing.

#![expect(missing_docs)]

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::async_executor::AsyncExecutor;
use criterion::criterion_group;
use criterion::criterion_main;
use pal_async::DefaultPool;
use std::cell::Cell;
use std::cell::RefCell;

struct WrappedExecutor(RefCell<DefaultPool>);

impl AsyncExecutor for &'_ WrappedExecutor {
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.0.borrow_mut().run_until(future)
    }
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut pool = DefaultPool::new();
    let driver = pool.driver();
    let tester = Cell::new(Some(
        pool.run_until(storvsp::ioperf::PerfTester::new(driver)),
    ));
    let runner = WrappedExecutor(RefCell::new(pool));
    let mut group = c.benchmark_group("read");
    for count in [1, 4, 16] {
        group
            .throughput(criterion::Throughput::Elements(count))
            .bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
                b.to_async(&runner).iter(async || {
                    let mut x = tester.take().unwrap();
                    x.read(count as usize).await;
                    tester.set(Some(x));
                })
            });
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
