// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Inspectable types for implementing performance counters.

#![forbid(unsafe_code)]

use inspect::Inspect;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// A simple 64-bit counter.
#[derive(Debug, Default, Clone)]
pub struct Counter(u64);

impl Counter {
    /// Returns an empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments the counter by one.
    pub fn increment(&mut self) {
        self.add(1);
    }

    /// Adds `n` to the counter, wrapping on overflow.
    pub fn add(&mut self, n: u64) {
        self.0 = self.0.wrapping_add(n);
    }

    /// Gets the current counter value.
    pub fn get(&self) -> u64 {
        self.0
    }
}

impl Inspect for Counter {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.with_counter_format().value(self.0)
    }
}

/// A 64-bit counter that can be concurrently accessed by multiple threads.
///
/// Prefer [`Counter`] for counters that are not accessed concurrently.
#[derive(Debug, Default)]
pub struct SharedCounter(AtomicU64);

impl SharedCounter {
    /// Returns an empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Increments the counter by one.
    pub fn increment(&self) {
        self.add(1);
    }

    /// Adds `n` to the counter, wrapping on overflow.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// Gets the current counter value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Inspect for SharedCounter {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.with_counter_format()
            .value(self.0.load(Ordering::Relaxed))
    }
}

/// A power-of-two histogram with `N` buckets.
#[derive(Clone, Debug)]
pub struct Histogram<const N: usize>([u64; N]);

impl<const N: usize> Default for Histogram<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Histogram<N> {
    /// Returns an empty histogram.
    pub fn new() -> Self {
        assert!(N > 2);
        assert!(N < BUCKETS.len());
        Self([0; N])
    }

    /// Adds a sample to the histogram.
    pub fn add_sample(&mut self, n: impl Into<u64>) {
        self.0[(64 - n.into().leading_zeros() as usize).min(N - 1)] += 1;
    }
}

static BUCKETS: &[&str] = &[
    "0",
    "1",
    "2-3",
    "4-7",
    "8-15",
    "16-31",
    "32-63",
    "64-127",
    "128-255",
    "256-511",
    "512-1023",
    "1024-2047",
    "2048-4095",
    "4096-8191",
    "8192-16383",
    "16384-32767",
    "32768-65535",
];

static WIDTH: &[usize] = &[1, 1, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 4, 5, 5];

impl<const N: usize> Inspect for Histogram<N> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for (i, &n) in self.0[..N - 1].iter().enumerate() {
            resp.counter(BUCKETS[i], n);
        }
        resp.counter(&BUCKETS[N - 1][..WIDTH[N - 1] + 1], self.0[N - 1]);
    }
}
