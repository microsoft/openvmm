// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`IoRanges`], which models a linear address-space with certain
//! regions "claimed" by [`ChipsetDevice`]s.
//!
//! - e.g: `IoRanges<u64>` can be used to model 64-bit MMIO.
//! - e.g: `IoRanges<u16>` can be used to model 16-bit x86 port IO.

mod address_filter;

use self::address_filter::RangeKey;
use chipset_device::ChipsetDevice;
use closeable_mutex::CloseableMutex;
use inspect::Inspect;
use inspect_counters::SharedCounter;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;

struct IoRangesInner<T> {
    map: RangeMap<T, RangeEntry>,
    trace_on: address_filter::AddressFilter<T>,
    break_on: address_filter::AddressFilter<T>,
    // Starts off a `Some(Vec::new())`, and then set to `None` as part of
    // Chipset finalization
    static_registration_conflicts: Option<Vec<IoRangeConflict<T>>>,
    fallback_device: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
}

#[derive(Debug, Clone)]
pub struct IoRangeConflict<T> {
    existing_dev_region: (Arc<str>, Arc<str>, RangeInclusive<T>),
    conflict_dev_region: (Arc<str>, Arc<str>, RangeInclusive<T>),
}

impl<T> std::error::Error for IoRangeConflict<T> where T: std::fmt::LowerHex + core::fmt::Debug {}
impl<T> std::fmt::Display for IoRangeConflict<T>
where
    T: std::fmt::LowerHex + core::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}:{:#x?} conflicts with existing {}/{}:{:#x?}",
            self.conflict_dev_region.0,
            self.conflict_dev_region.1,
            self.conflict_dev_region.2,
            self.existing_dev_region.0,
            self.existing_dev_region.1,
            self.existing_dev_region.2,
        )
    }
}

#[derive(Inspect)]
struct RangeEntry {
    region_name: Arc<str>,
    dev_name: Arc<str>,
    #[inspect(rename = "device_is_init", with = "|x| x.upgrade().is_some()")]
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    read_count: SharedCounter,
    write_count: SharedCounter,
}

#[derive(Clone)]
pub struct IoRanges<T> {
    inner: Arc<RwLock<IoRangesInner<T>>>,
}

impl<T: RangeKey> IoRanges<T> {
    pub fn new(
        trace_on_unknown: bool,
        fallback_device: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(IoRangesInner {
                map: RangeMap::new(),
                trace_on: address_filter::AddressFilter::new(trace_on_unknown),
                break_on: address_filter::AddressFilter::new(false),
                static_registration_conflicts: Some(Vec::new()),
                fallback_device,
            })),
        }
    }

    pub fn register(
        &self,
        start: T,
        end: T,
        region_name: Arc<str>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        dev_name: Arc<str>,
    ) -> Result<(), IoRangeConflict<T>> {
        let mut inner = self.inner.write();
        match inner.map.entry(start..=end) {
            range_map_vec::Entry::Vacant(entry) => {
                entry.insert(RangeEntry {
                    region_name,
                    dev,
                    dev_name,
                    read_count: Default::default(),
                    write_count: Default::default(),
                });
                Ok(())
            }
            range_map_vec::Entry::Overlapping(entry) => {
                let existing_dev_region = {
                    let (start, end, entry) = entry.get();
                    (
                        entry.dev_name.clone(),
                        entry.region_name.clone(),
                        *start..=*end,
                    )
                };
                let conflict = IoRangeConflict {
                    existing_dev_region,
                    conflict_dev_region: (dev_name, region_name, start..=end),
                };

                if let Some(v) = inner.static_registration_conflicts.as_mut() {
                    v.push(conflict.clone())
                }

                Err(conflict)
            }
        }
    }

    pub fn revoke(&self, start: T) {
        let mut inner = self.inner.write();
        inner.map.remove(&start);
    }

    pub fn lookup(&self, addr: T, is_read: bool) -> LookupResult {
        static UNKNOWN_DEVICE: OnceLock<Arc<CloseableMutex<dyn ChipsetDevice>>> = OnceLock::new();
        static UNKNOWN_DEVICE_NAME: OnceLock<Arc<str>> = OnceLock::new();
        static UNKNOWN_RANGE: OnceLock<Arc<str>> = OnceLock::new();

        let inner = self.inner.read();
        let entry = inner.map.get(&addr);
        if let Some(entry) = entry {
            if is_read {
                entry.read_count.increment()
            } else {
                entry.write_count.increment()
            }
        }

        let (dev, dev_name) =
            entry
                .and_then(|e| e.dev.upgrade().map(|d| (d, e.dev_name.clone())))
                .unwrap_or_else(|| {
                    (
                        inner.fallback_device.clone().unwrap_or_else(|| {
                            UNKNOWN_DEVICE
                        .get_or_init(|| {
                            Arc::new(CloseableMutex::new(missing_dev::MissingDev::from_manifest(
                                missing_dev::MissingDevManifest::new(),
                                &mut chipset_device::mmio::ExternallyManagedMmioIntercepts,
                                &mut chipset_device::pio::ExternallyManagedPortIoIntercepts,
                            )))
                        })
                        .clone()
                        }),
                        UNKNOWN_DEVICE_NAME
                            .get_or_init(|| "<unknown>".into())
                            .clone(),
                    )
                });

        let trace = inner.trace_on.filtered(&addr, entry.is_some());
        let trace = trace.then(|| {
            entry.map_or_else(
                || UNKNOWN_RANGE.get_or_init(|| "<unknown>".into()).clone(),
                |e| e.region_name.clone(),
            )
        });
        let debug_break = inner.break_on.filtered(&addr, entry.is_some());
        LookupResult {
            dev,
            dev_name,
            trace,
            debug_break,
        }
    }

    pub fn take_static_registration_conflicts(&mut self) -> Vec<IoRangeConflict<T>> {
        self.inner
            .write()
            .static_registration_conflicts
            .take()
            .expect("must only be called once")
    }
}

pub struct LookupResult {
    pub dev: Arc<CloseableMutex<dyn ChipsetDevice>>,
    pub dev_name: Arc<str>,
    pub trace: Option<Arc<str>>,
    pub debug_break: bool,
}

impl<T: RangeKey> Inspect for IoRanges<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        let mut inner = self.inner.write();
        resp.field_mut("trace_on", &mut inner.trace_on)
            .field_mut("break_on", &mut inner.break_on);
        for (range, entry) in inner.map.iter() {
            resp.field(&format!("{:#x}-{:#x}", range.start(), range.end()), entry);
        }
    }
}
