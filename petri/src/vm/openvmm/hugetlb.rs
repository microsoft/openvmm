// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for OpenVMM hugetlb-backed memory tests.

use anyhow::Context;

/// Size of a 2 MiB hugetlb page.
pub const HUGETLB_2MB_PAGE_SIZE: u64 = 2 * 1024 * 1024;

const REQUIRE_2MB_HUGETLB_ENV: &str = "OPENVMM_REQUIRE_2MB_HUGETLB";

fn require_2mb_hugetlb() -> bool {
    std::env::var_os(REQUIRE_2MB_HUGETLB_ENV).is_some()
}

fn read_hugetlb_counter(name: &str) -> anyhow::Result<Option<u64>> {
    let path = format!("/sys/kernel/mm/hugepages/hugepages-2048kB/{name}");
    let value = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {path}")),
    };
    Ok(Some(
        value
            .trim()
            .parse()
            .with_context(|| format!("failed to parse {path}"))?,
    ))
}

/// Returns whether the host appears to have enough 2 MiB hugetlb pages available.
///
/// By default, missing or insufficient host support returns `Ok(false)` after
/// logging a clear warning so local developer runs can skip tests cleanly. If
/// `OPENVMM_REQUIRE_2MB_HUGETLB` is set, missing or insufficient host support is
/// an error.
pub fn ensure_2mb_hugetlb_pages(required_pages: u64) -> anyhow::Result<bool> {
    let missing_message = "host does not have 2 MiB hugetlb support configured";

    let Some(free_pages) = read_hugetlb_counter("free_hugepages")? else {
        if require_2mb_hugetlb() {
            anyhow::bail!(missing_message);
        }
        tracing::warn!(missing_message);
        return Ok(false);
    };
    let Some(overcommit_pages) = read_hugetlb_counter("nr_overcommit_hugepages")? else {
        if require_2mb_hugetlb() {
            anyhow::bail!(missing_message);
        }
        tracing::warn!(missing_message);
        return Ok(false);
    };
    let Some(surplus_pages) = read_hugetlb_counter("surplus_hugepages")? else {
        if require_2mb_hugetlb() {
            anyhow::bail!(missing_message);
        }
        tracing::warn!(missing_message);
        return Ok(false);
    };

    let available_pages = free_pages + overcommit_pages.saturating_sub(surplus_pages);
    if available_pages < required_pages {
        let message = format!(
            "host has {available_pages} available 2 MiB hugetlb pages, but {required_pages} are required; configure /sys/kernel/mm/hugepages/hugepages-2048kB/nr_overcommit_hugepages before running this test"
        );
        if require_2mb_hugetlb() {
            anyhow::bail!(message);
        }
        tracing::warn!(message);
        return Ok(false);
    }

    Ok(true)
}
