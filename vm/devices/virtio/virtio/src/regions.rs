// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for working with data regions defined by virtio descriptors.

use std::borrow::Borrow;

/// A data-carrying region extracted from a descriptor chain.
///
/// Each entry represents one contiguous GPA range.
pub struct DataRegion {
    pub addr: u64,
    pub len: u64,
}

/// Extract the data-carrying regions from a descriptor chain.
///
/// Returns an iterator that filters descriptors by direction (`writable`),
/// skips `skip_bytes` (the request header for writes), and limits the
/// total to `data_len` (which excludes the status byte for reads).
pub fn data_regions(
    payloads: &[crate::queue::VirtioQueuePayload],
    writable: bool,
    skip_bytes: u64,
    data_len: u64,
) -> DataRegions<'_> {
    DataRegions {
        payloads: payloads.iter(),
        writable,
        skip: skip_bytes,
        remaining: data_len,
    }
}

/// Iterator over data-carrying regions from a descriptor chain.
///
/// Created by [`data_regions`].
pub struct DataRegions<'a> {
    payloads: core::slice::Iter<'a, crate::queue::VirtioQueuePayload>,
    writable: bool,
    skip: u64,
    remaining: u64,
}

impl Iterator for DataRegions<'_> {
    type Item = DataRegion;

    fn next(&mut self) -> Option<DataRegion> {
        while self.remaining > 0 {
            let payload = self.payloads.next()?;
            if payload.writeable != self.writable {
                continue;
            }
            let mut addr = payload.address;
            let mut plen = payload.length as u64;
            if self.skip > 0 {
                let s = self.skip.min(plen);
                addr += s;
                plen -= s;
                self.skip -= s;
            }
            if plen == 0 {
                continue;
            }
            let chunk = plen.min(self.remaining);
            self.remaining -= chunk;
            return Some(DataRegion { addr, len: chunk });
        }
        None
    }
}

/// Try to build a single `PagedRange` GPN list from the data regions.
///
/// Returns `Some((gpns, offset, len))` if every region boundary falls on
/// a page boundary (or regions are GPA-contiguous), so the whole chain
/// can be expressed as one [`guestmem::ranges::PagedRange`]. Returns `None` if any
/// interior boundary violates the constraint.
pub fn try_build_gpn_list(
    regions: impl IntoIterator<Item = impl Borrow<DataRegion>>,
) -> Option<(Vec<u64>, usize, usize)> {
    const PAGE_SIZE: u64 = guestmem::PAGE_SIZE as u64;

    let mut gpns = Vec::new();
    let mut total_len: u64 = 0;
    let mut first_offset: Option<usize> = None;
    let mut prev_end: Option<u64> = None;

    for region in regions {
        let region = region.borrow();
        let addr = region.addr;
        let len = region.len;
        if len == 0 {
            continue;
        }

        let first_gpn = addr / PAGE_SIZE;
        let last_gpn = (addr + len - 1) / PAGE_SIZE;

        if let Some(pe) = prev_end {
            if addr == pe {
                // GPA-contiguous with the previous region.
                // The shared page (if any) is already in gpns.
                let last_gpn_in_list = *gpns.last().unwrap();
                if first_gpn == last_gpn_in_list {
                    // Same page — just add any new pages beyond it.
                    for gpn in (first_gpn + 1)..=last_gpn {
                        gpns.push(gpn);
                    }
                } else {
                    // Previous region ended exactly at a page boundary,
                    // so first_gpn is the next page.
                    for gpn in first_gpn..=last_gpn {
                        gpns.push(gpn);
                    }
                }
            } else {
                // Not GPA-contiguous. Both the previous end and this
                // start must be page-aligned to avoid a gap or overlap
                // within a page slot.
                if pe % PAGE_SIZE != 0 || addr % PAGE_SIZE != 0 {
                    return None;
                }
                for gpn in first_gpn..=last_gpn {
                    gpns.push(gpn);
                }
            }
        } else {
            // First region.
            first_offset = Some((addr % PAGE_SIZE) as usize);
            for gpn in first_gpn..=last_gpn {
                gpns.push(gpn);
            }
        }

        prev_end = Some(addr + len);
        total_len += len;
    }

    let offset = first_offset.unwrap_or(0);
    Some((gpns, offset, total_len as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::VirtioQueuePayload;
    use guestmem::ranges::PagedRange;

    fn payload(writeable: bool, address: u64, length: u32) -> VirtioQueuePayload {
        VirtioQueuePayload {
            writeable,
            address,
            length,
        }
    }

    // ---- data_regions tests ----

    #[test]
    fn data_regions_read_single_descriptor() {
        // Read: writable descriptors carry data, skip=0, exclude 1 byte for status.
        let payloads = vec![payload(true, 0x1000, 4097)];
        let regions: Vec<_> = data_regions(&payloads, true, 0, 4096).collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x1000);
        assert_eq!(regions[0].len, 4096);
    }

    #[test]
    fn data_regions_write_skips_header() {
        // Write: readable descriptors carry data, skip header (16 bytes).
        let payloads = vec![
            payload(false, 0x1000, 16),  // header
            payload(false, 0x2000, 512), // data
        ];
        let regions: Vec<_> = data_regions(&payloads, false, 16, 512).collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x2000);
        assert_eq!(regions[0].len, 512);
    }

    #[test]
    fn data_regions_write_header_spans_descriptors() {
        // Header split across two descriptors.
        let payloads = vec![
            payload(false, 0x1000, 8),   // first 8 bytes of header
            payload(false, 0x2000, 520), // remaining 8 header + 512 data
        ];
        let regions: Vec<_> = data_regions(&payloads, false, 16, 512).collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x2008); // 0x2000 + 8 skipped
        assert_eq!(regions[0].len, 512);
    }

    #[test]
    fn data_regions_filters_by_direction() {
        // Readable and writable descriptors interleaved.
        let payloads = vec![
            payload(false, 0x1000, 16),  // readable: header
            payload(true, 0x3000, 4097), // writable: data + status
        ];
        // Extract writable regions (read path).
        let regions: Vec<_> = data_regions(&payloads, true, 0, 4096).collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].addr, 0x3000);
        assert_eq!(regions[0].len, 4096);
    }

    #[test]
    fn data_regions_empty_payload() {
        let payloads: Vec<VirtioQueuePayload> = vec![];
        let regions: Vec<_> = data_regions(&payloads, true, 0, 4096).collect();
        assert!(regions.is_empty());
    }

    // ---- try_build_gpn_list tests ----

    #[test]
    fn gpn_list_single_page_aligned_region() {
        let regions = vec![DataRegion {
            addr: 0x1000,
            len: 4096,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1]); // GPN 1 = addr 0x1000
        assert_eq!(offset, 0);
        assert_eq!(len, 4096);
    }

    #[test]
    fn gpn_list_single_region_with_offset() {
        let regions = vec![DataRegion {
            addr: 0x1200,
            len: 512,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1]);
        assert_eq!(offset, 0x200);
        assert_eq!(len, 512);
    }

    #[test]
    fn gpn_list_single_region_spanning_pages() {
        // 8192 bytes starting at page boundary → 2 pages.
        let regions = vec![DataRegion {
            addr: 0x2000,
            len: 8192,
        }];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![2, 3]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_two_page_aligned_non_contiguous_regions() {
        // Two regions on different pages, both page-aligned boundaries.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x5000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 5]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_two_gpa_contiguous_regions() {
        // Two regions that are GPA-contiguous (end of first == start of second).
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x2000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 2]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_contiguous_mid_page_boundary() {
        // Two GPA-contiguous regions sharing a page in the middle.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200
            DataRegion {
                addr: 0x2200,
                len: 3584,
            }, // starts at 0x2200, ends at 0x3000
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 2]);
        assert_eq!(offset, 0);
        assert_eq!(len, 8192);
    }

    #[test]
    fn gpn_list_non_contiguous_non_aligned_fails() {
        // Two non-contiguous regions where the boundary isn't page-aligned.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200, not page-aligned
            DataRegion {
                addr: 0x5200,
                len: 512,
            }, // different location, not page-aligned start
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_non_contiguous_first_aligned_second_not() {
        // First ends page-aligned, but second starts mid-page.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            }, // ends at 0x2000 (aligned)
            DataRegion {
                addr: 0x5200,
                len: 512,
            }, // starts at 0x5200 (not aligned)
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_non_contiguous_first_not_aligned_second_aligned() {
        // First ends mid-page, second starts page-aligned.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4608,
            }, // ends at 0x2200 (not aligned)
            DataRegion {
                addr: 0x5000,
                len: 4096,
            }, // starts page-aligned
        ];
        assert!(try_build_gpn_list(&regions).is_none());
    }

    #[test]
    fn gpn_list_empty_regions() {
        let regions: Vec<DataRegion> = vec![];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert!(gpns.is_empty());
        assert_eq!(offset, 0);
        assert_eq!(len, 0);
    }

    #[test]
    fn gpn_list_three_page_aligned_regions() {
        // Three separate page-aligned regions.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x3000,
                len: 4096,
            },
            DataRegion {
                addr: 0x7000,
                len: 4096,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 3, 7]);
        assert_eq!(offset, 0);
        assert_eq!(len, 12288);
    }

    #[test]
    fn gpn_list_first_region_with_offset_second_page_aligned() {
        // First region starts mid-page but ends at page boundary,
        // second region starts at a different page boundary.
        let regions = vec![
            DataRegion {
                addr: 0x1800,
                len: 2048,
            }, // 0x1800..0x2000
            DataRegion {
                addr: 0x5000,
                len: 4096,
            }, // 0x5000..0x6000
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        assert_eq!(gpns, vec![1, 5]);
        assert_eq!(offset, 0x800);
        assert_eq!(len, 6144);
    }

    #[test]
    fn gpn_list_validates_paged_range_construction() {
        // Verify that the returned values actually produce a valid PagedRange.
        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 4096,
            },
            DataRegion {
                addr: 0x5000,
                len: 8192,
            },
        ];
        let (gpns, offset, len) = try_build_gpn_list(&regions).unwrap();
        let range = PagedRange::new(offset, len, &gpns);
        assert!(range.is_some());
        assert_eq!(range.unwrap().len(), 12288);
    }
}
