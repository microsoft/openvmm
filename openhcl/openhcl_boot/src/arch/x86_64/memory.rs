// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Routines to prepare VTL2 memory for launching the kernel.

use super::address_space::LocalMap;
use super::address_space::init_local_map;
use crate::AddressSpaceManager;
use crate::ShimParams;
use crate::arch::TdxHypercallPage;
use crate::arch::x86_64::address_space::tdx_share_large_page;
use crate::host_params::PartitionInfo;
use crate::host_params::shim_params::IsolationType;
use crate::hypercall::hvcall;
use crate::memory::AllocationPolicy;
use crate::memory::AllocationType;
use crate::off_stack;
use crate::single_threaded::SingleThreaded;
use arrayvec::ArrayVec;
use core::cell::RefCell;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use page_table::x64::MappedRange;
use page_table::x64::PAGE_TABLE_MAX_BYTES;
use page_table::x64::PAGE_TABLE_MAX_COUNT;
use page_table::x64::PageTable;
use page_table::x64::PageTableBuilder;
use sha2::Digest;
use sha2::Sha384;
use static_assertions::const_assert;
use x86defs::X64_LARGE_PAGE_SIZE;
use x86defs::tdx::TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
use zerocopy::FromZeros;

// ============================================================================
// Diagnostic instrumentation for the "Imported regions hash mismatch" panic.
// ============================================================================
//
// This is a temporary debugging aid intended for local investigation of a rare
// mismatch seen on SNP boots. It computes SHA-384 hashes at three points to
// isolate which phase corruption occurs in:
//   Phase A: bytes captured out of the shared (host-visible) page into
//            `ram_buffer` just before the shared -> private transition.
//   Phase B: bytes as read from the same GPA immediately after the
//            transition (via the C=1 identity map), i.e. after accept +
//            copy-back.
//   Phase C: bytes as read from the same GPA at final verify time.
//
// Granularity:
//   - Phase A capture: 2 MB accept-chunk (records one SHA-384 per chunk) plus
//     a running combined hash for compare against `imported_regions_hash()`.
//   - Phase A -> B compare: 4 KB PAGE granularity (per Jon's feedback). On
//     mismatch we emit a bitmap of corrupt pages, RLE ranges, per-page pre/
//     post SHA-384 (capped), and a full 4 KB hex dump of the first bad page.
//   - Phase A -> C compare: per-chunk (interim). Once the loader is updated
//     to emit per-page expected hashes, Phase C can also do per-page.
//
// Full-page dumps are strictly one-shot per soak (see DIAG_FULL_PAGE_DUMPED)
// -- one sample is enough to eyeball whether corruption is a bit flip, a
// zeroed page, a substituted page, etc., and we don't want to spam COM3 with
// 64 lines per mismatched chunk.
//
// Not intended for check-in.

/// Diagnostic page size == HV page size (4 KB).
const DIAG_PAGE_SIZE: usize = hvdef::HV_PAGE_SIZE as usize;

/// Max pages we can bitmap in a single 2 MB accept chunk (2 MB / 4 KB = 512).
const DIAG_MAX_PAGES_PER_CHUNK: usize = X64_LARGE_PAGE_SIZE as usize / DIAG_PAGE_SIZE;
const _: () = assert!(DIAG_MAX_PAGES_PER_CHUNK <= 512);

/// Bitmap word count (u64s) for one 2 MB chunk.
const DIAG_BITMAP_WORDS: usize = DIAG_MAX_PAGES_PER_CHUNK / 64;

/// Cap on how many per-bad-page SHA-384 lines we emit per Phase B mismatch,
/// so a wholly-corrupt chunk doesn't spam thousands of log lines.
const DIAG_MAX_BAD_PAGE_HASHES: usize = 32;

/// Maximum number of 2 MB chunks we track. Debug builds hash roughly
/// kernel + initrd (~80 MB), giving ~40 chunks; leave generous headroom.
const DIAG_MAX_CHUNKS: usize = 256;

/// Maximum number of individual 4 KB pages we can track for per-page
/// expected-hash comparison. Sized with headroom over the current
/// shared-page count observed in soaks (~20 K pages = 40 x 2 MB chunks).
const DIAG_MAX_HASH_PAGES: usize = 32 * 1024;
const DIAG_HASH_BITMAP_WORDS: usize = DIAG_MAX_HASH_PAGES / 64;

/// Number of corrupt pages for which we save the full 4 KB contents so
/// we can hex-dump them on final report.
const DIAG_MAX_SAVED_BAD_PAGES: usize = 3;

#[derive(Copy, Clone)]
struct DiagChunkHash {
    gpa: u64,
    len: u32,
    phase_a_hash: [u8; 48],
}

/// One corrupt page's full 4 KB contents plus the shim vs loader hashes,
/// captured during Phase A when we first noticed the mismatch.
#[derive(Copy, Clone)]
struct DiagSavedBadPage {
    /// Global page index in the shim's iteration order.
    page_idx: u32,
    /// Guest physical address of the page.
    gpa: u64,
    /// SHA-384 the shim computed over the host-loaded bytes.
    shim_hash: [u8; 48],
    /// SHA-384 the loader recorded at IGVM build time.
    expected_hash: [u8; 48],
    /// Full 4 KB page contents (as the shim saw them at Phase A).
    contents: [u8; DIAG_PAGE_SIZE],
}

const DIAG_EMPTY_SAVED_PAGE: DiagSavedBadPage = DiagSavedBadPage {
    page_idx: 0,
    gpa: 0,
    shim_hash: [0; 48],
    expected_hash: [0; 48],
    contents: [0; DIAG_PAGE_SIZE],
};

/// Per-page expected-hash tracking state. Populated during Phase A capture
/// as each 4 KB shared page is compared against the loader's per-page
/// SHA-384 baked into the measured expected-page-hashes region.
struct DiagPerPageState {
    /// Cached slice from `ShimParams::expected_page_hashes()`. `None` if
    /// the IGVM has no expected-page-hashes region (older loader) or the
    /// region magic/version mismatched -- per-page compare is disabled.
    expected: Option<&'static [loader_defs::paravisor::ExpectedPageHash]>,
    /// Number of 4 KB pages Phase A has processed so far.
    seen: u32,
    /// Number of pages whose Phase A hash did not match `expected[i]`.
    bad: u32,
    /// True if the shim hashed more pages than the loader emitted hashes
    /// for; per-page compare stops for the tail of the walk when this
    /// flips true.
    overflow: bool,
    /// Bitmap of mismatched pages indexed by shim page index (bit i set
    /// = page i differed from `expected[i]`). Capped at
    /// `DIAG_MAX_HASH_PAGES` positions -- anything beyond is not
    /// tracked in the bitmap (still counted in `bad`).
    bitmap: [u64; DIAG_HASH_BITMAP_WORDS],
    /// Number of entries populated in `saved`.
    saved_count: u32,
    /// Full 4 KB contents of the first `DIAG_MAX_SAVED_BAD_PAGES` bad
    /// pages, so we can hex-dump them for byte-level inspection.
    saved: [DiagSavedBadPage; DIAG_MAX_SAVED_BAD_PAGES],
}

impl DiagPerPageState {
    const fn new_const() -> Self {
        Self {
            expected: None,
            seen: 0,
            bad: 0,
            overflow: false,
            bitmap: [0; DIAG_HASH_BITMAP_WORDS],
            saved_count: 0,
            saved: [DIAG_EMPTY_SAVED_PAGE; DIAG_MAX_SAVED_BAD_PAGES],
        }
    }
}

static DIAG_CHUNK_HASHES: SingleThreaded<RefCell<ArrayVec<DiagChunkHash, DIAG_MAX_CHUNKS>>> =
    SingleThreaded(RefCell::new(ArrayVec::new_const()));

static DIAG_RUNNING_A: SingleThreaded<RefCell<Option<Sha384>>> = SingleThreaded(RefCell::new(None));

/// One-shot latch guarding the full 4 KB hex dump of a corrupted page.
/// Whichever phase (B or C) trips a mismatch first gets to dump; every
/// subsequent detection just logs the header/bitmap/hashes and skips the
/// full dump. Keeps COM3 quiet even when many chunks are corrupted.
static DIAG_FULL_PAGE_DUMPED: SingleThreaded<core::cell::Cell<bool>> =
    SingleThreaded(core::cell::Cell::new(false));

/// Per-page expected-hash tracking (populated in `diag_record_phase_a` and
/// reported in `diag_report_per_page_expected`).
static DIAG_PER_PAGE: SingleThreaded<RefCell<DiagPerPageState>> =
    SingleThreaded(RefCell::new(DiagPerPageState::new_const()));

/// Returns true and latches on the first call; returns false thereafter.
fn diag_claim_full_page_dump() -> bool {
    if DIAG_FULL_PAGE_DUMPED.0.get() {
        return false;
    }
    DIAG_FULL_PAGE_DUMPED.0.set(true);
    true
}

/// `core::fmt::Display` adapter that prints a byte slice as lowercase hex,
/// no separators. Convenient for hashes in log lines.
struct HexBytes<'a>(&'a [u8]);
impl core::fmt::Display for HexBytes<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// `core::fmt::Display` adapter that prints a page-bitmap (little-endian
/// per byte within each u64 word) as a compact hex string. One hex char per
/// 4 pages, so a full 2 MB / 4 KB = 512-page chunk fits in 128 hex chars.
struct BitmapHex<'a> {
    bitmap: &'a [u64],
    total_pages: usize,
}
impl core::fmt::Display for BitmapHex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bits = self.total_pages.min(self.bitmap.len() * 64);
        let bytes = bits.div_ceil(8);
        for byte_idx in 0..bytes {
            let word = byte_idx / 8;
            let byte_in_word = byte_idx % 8;
            let b = ((self.bitmap[word] >> (byte_in_word * 8)) & 0xff) as u8;
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// `core::fmt::Display` adapter that walks a page-bitmap and emits corrupt
/// page-index ranges in the form `0x8-0xa,0x11,0x20-0x21`. Human-readable
/// alternative to the raw hex bitmap.
struct RleRanges<'a> {
    bitmap: &'a [u64],
    total_pages: usize,
}
impl core::fmt::Display for RleRanges<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        let mut run_start: Option<usize> = None;
        let mut prev_bit: bool = false;
        for idx in 0..self.total_pages {
            let word = idx / 64;
            let bit = idx % 64;
            let set = word < self.bitmap.len() && (self.bitmap[word] >> bit) & 1 == 1;
            if set && !prev_bit {
                run_start = Some(idx);
            }
            if !set && prev_bit {
                let start = run_start.unwrap();
                let end = idx - 1;
                if !first {
                    write!(f, ",")?;
                }
                first = false;
                if start == end {
                    write!(f, "{:#x}", start)?;
                } else {
                    write!(f, "{:#x}-{:#x}", start, end)?;
                }
                run_start = None;
            }
            prev_bit = set;
        }
        // Handle a run that reaches the last page.
        if prev_bit {
            let start = run_start.unwrap();
            let end = self.total_pages - 1;
            if !first {
                write!(f, ",")?;
            }
            if start == end {
                write!(f, "{:#x}", start)?;
            } else {
                write!(f, "{:#x}-{:#x}", start, end)?;
            }
        }
        Ok(())
    }
}

/// Record the Phase-A hash of a chunk that was just copied out of a shared
/// page into `ram_buffer`. Also feeds the bytes into a running combined
/// Phase-A hasher so it can be compared against `imported_regions_hash()`
/// on final mismatch. Additionally walks the chunk at 4 KB page
/// granularity and compares each page's SHA-384 against the loader-emitted
/// per-page expected hash (if `diag_init_expected_hashes` cached one) --
/// mismatches are recorded in a bitmap plus a small buffer of the first N
/// bad pages' full contents so `diag_report_per_page_expected` can dump
/// them on final panic.
fn diag_record_phase_a(gpa: u64, data: &[u8]) {
    {
        let mut running = DIAG_RUNNING_A.0.borrow_mut();
        if running.is_none() {
            *running = Some(Sha384::new());
        }
        running.as_mut().unwrap().update(data);
    }

    let mut h = Sha384::new();
    h.update(data);
    let hash: [u8; 48] = h.finalize().into();

    let mut chunks = DIAG_CHUNK_HASHES.0.borrow_mut();
    if chunks
        .try_push(DiagChunkHash {
            gpa,
            len: data.len() as u32,
            phase_a_hash: hash,
        })
        .is_err()
    {
        log::error!(
            "DIAG_CHUNK_OVERFLOW gpa={:#x} len={:#x} (increase DIAG_MAX_CHUNKS)",
            gpa,
            data.len(),
        );
    }
    drop(chunks);

    // Per-page comparison against the loader-emitted expected hashes.
    // Each 4 KB page in the chunk gets its own SHA-384; on mismatch we
    // set a bit in the bitmap and, for the first N cases, snapshot the
    // full 4 KB of host-loaded bytes so we can hex-dump them at report
    // time. Skips silently if `diag_init_expected_hashes` did not cache
    // a slice (older IGVM or region magic/version mismatch).
    let mut state = DIAG_PER_PAGE.0.borrow_mut();
    let expected_opt = state.expected;
    if let Some(expected) = expected_opt {
        for (page_off, page) in data.chunks(DIAG_PAGE_SIZE).enumerate() {
            if page.len() < DIAG_PAGE_SIZE {
                // Runt tail (shouldn't happen for page-aligned shared
                // regions, but be defensive).
                break;
            }
            let idx = state.seen as usize;
            let page_gpa = gpa + (page_off * DIAG_PAGE_SIZE) as u64;

            let mut ph = Sha384::new();
            ph.update(page);
            let shim_hash: [u8; 48] = ph.finalize().into();

            if idx >= expected.len() {
                state.overflow = true;
            } else {
                let expected_hash = expected[idx].sha384_hash;
                if shim_hash != expected_hash {
                    state.bad = state.bad.saturating_add(1);
                    if idx < DIAG_MAX_HASH_PAGES {
                        let word = idx / 64;
                        let bit = idx % 64;
                        state.bitmap[word] |= 1u64 << bit;
                    }
                    let slot_idx = state.saved_count as usize;
                    if slot_idx < DIAG_MAX_SAVED_BAD_PAGES {
                        let slot = &mut state.saved[slot_idx];
                        slot.page_idx = idx as u32;
                        slot.gpa = page_gpa;
                        slot.shim_hash = shim_hash;
                        slot.expected_hash = expected_hash;
                        slot.contents.copy_from_slice(page);
                        state.saved_count += 1;
                    }
                }
            }
            state.seen = state.seen.saturating_add(1);
        }
    }
}

/// Emit a full 4 KB page as hex, one log line per 64-byte cache line, with
/// pre and post side by side. Used when we have both Phase-A and Phase-C
/// bytes for the first corrupted page.
fn diag_dump_full_page_diff(page_gpa: u64, phase: &str, pre: &[u8], post: &[u8]) {
    debug_assert_eq!(pre.len(), DIAG_PAGE_SIZE);
    debug_assert_eq!(post.len(), DIAG_PAGE_SIZE);
    log::error!(
        "DIAG_FIRST_BAD_PAGE_BEGIN page_gpa={:#x} phase={} len={:#x}",
        page_gpa,
        phase,
        DIAG_PAGE_SIZE,
    );
    for (line_idx, (pre_line, post_line)) in pre.chunks(64).zip(post.chunks(64)).enumerate() {
        let offset = line_idx * 64;
        log::error!(
            "DIAG_FIRST_BAD_PAGE_LINE page_gpa={:#x} offset={:#06x} pre={} post={}",
            page_gpa,
            offset,
            HexBytes(pre_line),
            HexBytes(post_line),
        );
    }
    log::error!("DIAG_FIRST_BAD_PAGE_END page_gpa={:#x}", page_gpa);
}

/// Emit a full 4 KB page as hex, one log line per 64-byte cache line. Used
/// when only the current (Phase-D) bytes are available for the first bad
/// page and there is no pre-image to compare side by side.
fn diag_dump_full_page_single(page_gpa: u64, phase: &str, data: &[u8]) {
    debug_assert!(data.len() >= DIAG_PAGE_SIZE);
    log::error!(
        "DIAG_FIRST_BAD_PAGE_BEGIN page_gpa={:#x} phase={} len={:#x}",
        page_gpa,
        phase,
        DIAG_PAGE_SIZE,
    );
    for (line_idx, line) in data[..DIAG_PAGE_SIZE].chunks(64).enumerate() {
        let offset = line_idx * 64;
        log::error!(
            "DIAG_FIRST_BAD_PAGE_LINE page_gpa={:#x} offset={:#06x} bytes={}",
            page_gpa,
            offset,
            HexBytes(line),
        );
    }
    log::error!("DIAG_FIRST_BAD_PAGE_END page_gpa={:#x}", page_gpa);
}

/// Emit a saved corrupt page (captured in Phase A) as hex, one log line
/// per 64-byte cache line, along with the shim vs loader hashes.
fn diag_dump_saved_page(saved: &DiagSavedBadPage) {
    log::error!(
        "DIAG_PAGE_HASH_BAD_BEGIN idx={} gpa={:#x} shim_hash={} expected_hash={} len={:#x}",
        saved.page_idx,
        saved.gpa,
        HexBytes(&saved.shim_hash),
        HexBytes(&saved.expected_hash),
        DIAG_PAGE_SIZE,
    );
    for (line_idx, line) in saved.contents.chunks(64).enumerate() {
        let offset = line_idx * 64;
        log::error!(
            "DIAG_PAGE_HASH_BAD_LINE idx={} gpa={:#x} offset={:#06x} bytes={}",
            saved.page_idx,
            saved.gpa,
            offset,
            HexBytes(line),
        );
    }
    log::error!(
        "DIAG_PAGE_HASH_BAD_END idx={} gpa={:#x}",
        saved.page_idx,
        saved.gpa,
    );
}

/// Cache the loader-emitted per-page expected hashes so `diag_record_phase_a`
/// can compare each 4 KB page against them. Must be called once, before the
/// acceptance loop in `setup_vtl2_memory`. If the IGVM doesn't have the
/// expected-page-hashes region (older loader) or the region magic/version
/// mismatched, per-page compare is left disabled and `diag_record_phase_a`
/// silently skips the per-page work.
fn diag_init_expected_hashes(shim_params: &ShimParams) {
    let expected = shim_params.expected_page_hashes();
    if expected.is_empty() {
        log::info!(
            "DIAG_EXPECTED_HASHES_META loader_count=0 \
             (region absent or magic/version mismatch; per-page compare disabled)"
        );
        return;
    }
    DIAG_PER_PAGE.0.borrow_mut().expected = Some(expected);
    log::info!("DIAG_EXPECTED_HASHES_META loader_count={}", expected.len(),);
}

/// Emit the per-page expected-hash comparison summary, corrupt-page bitmap,
/// RLE ranges, and full 4 KB dumps of the first `DIAG_MAX_SAVED_BAD_PAGES`
/// corrupt pages. Called from `diag_report_phase_c` (i.e. after the
/// combined hash mismatch has been detected and just before panic).
fn diag_report_per_page_expected() {
    let state = DIAG_PER_PAGE.0.borrow();
    let loader_count = state.expected.map(|s| s.len()).unwrap_or(0);
    log::error!(
        "DIAG_PAGE_HASH_SUMMARY shim_seen={} bad={} loader_count={} overflow={}",
        state.seen,
        state.bad,
        loader_count,
        state.overflow,
    );
    if state.expected.is_none() {
        // Per-page compare was disabled; nothing more to report.
        return;
    }
    let bitmap_total = (state.seen as usize).min(DIAG_MAX_HASH_PAGES);
    log::error!(
        "DIAG_PAGE_HASH_BITMAP total_pages={} bad={} bitmap={}",
        bitmap_total,
        state.bad,
        BitmapHex {
            bitmap: &state.bitmap,
            total_pages: bitmap_total,
        },
    );
    log::error!(
        "DIAG_PAGE_HASH_RANGES total_pages={} bad={} ranges={}",
        bitmap_total,
        state.bad,
        RleRanges {
            bitmap: &state.bitmap,
            total_pages: bitmap_total,
        },
    );
    for i in 0..(state.saved_count as usize) {
        diag_dump_saved_page(&state.saved[i]);
    }
}

/// Immediately after the shared -> private transition and copy-back, compare
/// the same chunk (via the identity map) against the Phase-A bytes we captured
/// before the transition, at 4 KB PAGE granularity. On mismatch, emits:
///  - `DIAG_TRANSITION_MISMATCH` header with total/bad page counts and per-
///    chunk pre/post SHA-384.
///  - `DIAG_TRANSITION_BITMAP` (hex) and `DIAG_TRANSITION_PAGES` (RLE) so we
///    can see the distribution of corrupt pages within the chunk.
///  - `DIAG_BAD_PAGE_HASH` per corrupt page (capped at
///    DIAG_MAX_BAD_PAGE_HASHES) with SHA-384 of both pre and post.
///  - Full 4 KB hex dump of the FIRST corrupt page via
///    `DIAG_FIRST_BAD_PAGE_{BEGIN,LINE,END}` -- but ONLY if no previous
///    call (from any chunk or from Phase C) has already claimed the
///    one-shot dump latch. One sample is enough to characterise the
///    corruption pattern.
fn diag_verify_phase_b(gpa: u64, pre: &[u8], post: &[u8]) {
    if pre.len() != post.len() {
        log::error!(
            "DIAG_TRANSITION_LEN_MISMATCH gpa={:#x} pre_len={:#x} post_len={:#x}",
            gpa,
            pre.len(),
            post.len(),
        );
        return;
    }
    if pre == post {
        return;
    }

    // Bitmap of mismatched pages within this chunk. Chunks are <= 2 MB
    // = 512 pages, so DIAG_BITMAP_WORDS u64s suffice.
    let mut bitmap = [0u64; DIAG_BITMAP_WORDS];
    let mut bad_count: usize = 0;
    let mut first_bad_page_idx: Option<usize> = None;
    let mut bad_hashes: ArrayVec<(u64, [u8; 48], [u8; 48]), DIAG_MAX_BAD_PAGE_HASHES> =
        ArrayVec::new();
    let total_pages = pre.len().div_ceil(DIAG_PAGE_SIZE);

    for (idx, (pre_pg, post_pg)) in pre
        .chunks(DIAG_PAGE_SIZE)
        .zip(post.chunks(DIAG_PAGE_SIZE))
        .enumerate()
    {
        if pre_pg == post_pg {
            continue;
        }
        bad_count += 1;
        if first_bad_page_idx.is_none() {
            first_bad_page_idx = Some(idx);
        }
        let word = idx / 64;
        let bit = idx % 64;
        if word < bitmap.len() {
            bitmap[word] |= 1u64 << bit;
        }

        if bad_hashes.len() < DIAG_MAX_BAD_PAGE_HASHES {
            let mut ha = Sha384::new();
            ha.update(pre_pg);
            let hash_a: [u8; 48] = ha.finalize().into();
            let mut hc = Sha384::new();
            hc.update(post_pg);
            let hash_c: [u8; 48] = hc.finalize().into();
            let _ = bad_hashes.try_push((gpa + (idx * DIAG_PAGE_SIZE) as u64, hash_a, hash_c));
        }
    }

    // Overall pre/post SHA-384 for the whole chunk, for quick fingerprinting.
    let mut ha = Sha384::new();
    ha.update(pre);
    let chunk_hash_a: [u8; 48] = ha.finalize().into();
    let mut hb = Sha384::new();
    hb.update(post);
    let chunk_hash_b: [u8; 48] = hb.finalize().into();

    log::error!(
        "DIAG_TRANSITION_MISMATCH gpa={:#x} chunk_len={:#x} total_pages={} bad_pages={} \
         chunk_phase_a={} chunk_phase_b={}",
        gpa,
        pre.len(),
        total_pages,
        bad_count,
        HexBytes(&chunk_hash_a),
        HexBytes(&chunk_hash_b),
    );
    log::error!(
        "DIAG_TRANSITION_BITMAP gpa={:#x} total_pages={} bitmap={}",
        gpa,
        total_pages,
        BitmapHex {
            bitmap: &bitmap,
            total_pages,
        },
    );
    log::error!(
        "DIAG_TRANSITION_PAGES gpa={:#x} count={} ranges={}",
        gpa,
        bad_count,
        RleRanges {
            bitmap: &bitmap,
            total_pages,
        },
    );

    for (page_gpa, ha, hb) in &bad_hashes {
        log::error!(
            "DIAG_BAD_PAGE_HASH phase=A_vs_B chunk_gpa={:#x} page_gpa={:#x} phase_a={} phase_b={}",
            gpa,
            page_gpa,
            HexBytes(ha),
            HexBytes(hb),
        );
    }
    if bad_count > bad_hashes.len() {
        log::error!(
            "DIAG_BAD_PAGE_HASH_TRUNCATED chunk_gpa={:#x} shown={} total_bad={}",
            gpa,
            bad_hashes.len(),
            bad_count,
        );
    }

    if let Some(idx) = first_bad_page_idx {
        if diag_claim_full_page_dump() {
            let offset = idx * DIAG_PAGE_SIZE;
            let pre_pg = &pre[offset..offset + DIAG_PAGE_SIZE];
            let post_pg = &post[offset..offset + DIAG_PAGE_SIZE];
            diag_dump_full_page_diff(gpa + offset as u64, "A_vs_B", pre_pg, post_pg);
        }
    }
}

/// Called from `verify_imported_regions_hash` when the combined Phase-C hash
/// does not match the expected measured value. Reports:
/// - Whether the running combined Phase-A hash matches expected. If it does,
///   the host supplied correct bytes and something in the shim corrupted them.
///   If it doesn't, the host loaded bad data.
/// - Per-chunk Phase-C vs Phase-A comparison to identify which chunk(s) drifted
///   between the shared read and the final verify, plus a full 4 KB hex dump
///   of the first page of the FIRST mismatched chunk (subject to the same
///   one-shot latch as Phase B -- one sample is enough). Once the loader is
///   updated to emit per-page expected hashes we can do per-page here too.
fn diag_report_phase_c(expected_combined: &[u8]) {
    let combined_a = DIAG_RUNNING_A.0.borrow_mut().take().map(|h| {
        let out: [u8; 48] = h.finalize().into();
        out
    });

    match combined_a {
        Some(combined_a) => {
            if combined_a.as_slice() == expected_combined {
                log::error!(
                    "DIAG_VERDICT combined_phase_a matches expected: {} \
                     (host-supplied shared data was correct; corruption occurred at or after acceptance)",
                    HexBytes(&combined_a),
                );
            } else {
                log::error!(
                    "DIAG_VERDICT combined_phase_a differs from expected: \
                     phase_a={} expected={} \
                     (host loaded incorrect data into shared pages)",
                    HexBytes(&combined_a),
                    HexBytes(expected_combined),
                );
            }
        }
        None => {
            log::error!("DIAG_VERDICT combined_phase_a not captured (no chunks recorded)");
        }
    }

    let chunks = DIAG_CHUNK_HASHES.0.borrow();
    let total = chunks.len();
    let mut mismatches: usize = 0;
    for (idx, c) in chunks.iter().enumerate() {
        // SAFETY: The GPA and length were recorded from a range that the shim
        // itself just accepted as private VTL2 RAM, and remain identity-mapped
        // for the duration of the shim.
        let data = unsafe { core::slice::from_raw_parts(c.gpa as *const u8, c.len as usize) };
        let mut h = Sha384::new();
        h.update(data);
        let hash_c: [u8; 48] = h.finalize().into();
        if hash_c != c.phase_a_hash {
            mismatches += 1;
            log::error!(
                "DIAG_POST_ACCEPT_MISMATCH idx={} gpa={:#x} len={:#x} phase_a={} phase_c={}",
                idx,
                c.gpa,
                c.len,
                HexBytes(&c.phase_a_hash),
                HexBytes(&hash_c),
            );
            // We no longer have the Phase-A bytes (they lived in `ram_buffer`
            // which has been reused). Full 4 KB dump of the first page of the
            // first mismatched chunk lets us eyeball whether the chunk was
            // zeroed, substituted with a different page, or has a subtle bit
            // flip. One-shot: once we've dumped a page (here or in Phase B),
            // subsequent mismatches only log the hash lines.
            // TODO(item 5): once the loader emits per-page expected hashes,
            // iterate the chunk at 4 KB granularity here and identify exactly
            // which pages diverged (like diag_verify_phase_b does).
            if data.len() >= DIAG_PAGE_SIZE && diag_claim_full_page_dump() {
                diag_dump_full_page_single(c.gpa, "C", data);
            }
        }
    }
    if mismatches == 0 {
        log::error!(
            "DIAG_VERDICT all {} tracked chunks unchanged phase_a -> phase_c \
             (combined mismatch is likely a layout/order/hashing disagreement)",
            total,
        );
    } else {
        log::error!(
            "DIAG_VERDICT {} of {} tracked chunks changed between phase_a and phase_c",
            mismatches,
            total,
        );
    }

    // Per-page comparison against the loader-emitted expected hashes.
    // Emits the shim-vs-loader page count, the corrupt-page bitmap and
    // RLE ranges, and full 4 KB dumps of the first
    // `DIAG_MAX_SAVED_BAD_PAGES` corrupt pages.
    diag_report_per_page_expected();
}

/// On isolated systems, transitions all VTL2 RAM to be private and accepted, with the appropriate
/// VTL permissions applied.
pub fn setup_vtl2_memory(
    shim_params: &ShimParams,
    partition_info: &PartitionInfo,
    address_space: &mut AddressSpaceManager,
) {
    // Only if the partition is VBS-isolated, accept memory and apply vtl 2 protections here.
    // Non-isolated partitions can undergo servicing, and additional information
    // would be needed to determine whether vtl 2 protections should be applied
    // or skipped, since the operation is expensive.
    // TODO: if applying vtl 2 protections for non-isolated VMs moves to the
    // boot shim, apply them here.
    if let IsolationType::None = shim_params.isolation_type {
        return;
    }

    // DIAG: cache the loader-emitted per-page expected hashes (from the
    // measured expected-page-hashes region) so that as Phase A captures
    // each 4 KB shared page we can compare it against the loader's
    // baseline. Silently no-ops on older IGVMs without the region.
    diag_init_expected_hashes(shim_params);

    if let IsolationType::Vbs = shim_params.isolation_type {
        // Enable VTL protection so that vtl 2 protections can be applied. All other config
        // should be set by the user mode
        let vsm_config = hvdef::HvRegisterVsmPartitionConfig::new()
            .with_default_vtl_protection_mask(0xF)
            .with_enable_vtl_protection(true);

        hvcall()
            .set_register(
                hvdef::HvX64RegisterName::VsmPartitionConfig.into(),
                hvdef::HvRegisterValue::from(u64::from(vsm_config)),
            )
            .expect("setting vsm config shouldn't fail");

        // VBS isolated VMs need to apply VTL2 protections to pages that were already accepted to
        // prevent VTL0 access. Only those pages that belong to the VTL2 RAM region should have
        // these protections applied - certain pages belonging to VTL0 are also among the accepted
        // regions and should not be processed here.
        let accepted_ranges =
            shim_params
                .imported_regions()
                .filter_map(|(imported_range, already_accepted)| {
                    already_accepted.then_some(imported_range)
                });
        for range in memory_range::overlapping_ranges(
            partition_info.vtl2_ram.iter().map(|entry| entry.range),
            accepted_ranges,
        ) {
            hvcall()
                .apply_vtl2_protections(range)
                .expect("applying vtl 2 protections cannot fail");
        }
    }

    // Initialize the local_map
    // TODO: Consider moving this to ShimParams to pass around.
    let mut local_map = match shim_params.isolation_type {
        IsolationType::Snp | IsolationType::Tdx => Some(init_local_map(
            loader_defs::paravisor::PARAVISOR_LOCAL_MAP_VA,
        )),
        _ => None,
    };

    // Make sure imported regions are in increasing order.
    let mut last_range_end = None;
    for (imported_range, _) in shim_params.imported_regions() {
        assert!(last_range_end.is_none() || imported_range.start() > last_range_end.unwrap());
        last_range_end = Some(imported_range.end() - hvdef::HV_PAGE_SIZE);
    }

    // Iterate over all VTL2 RAM that is not part of an imported region and
    // accept it with appropriate VTL protections.
    for range in memory_range::subtract_ranges(
        partition_info.vtl2_ram.iter().map(|e| e.range),
        shim_params.imported_regions().map(|(r, _)| r),
    ) {
        accept_vtl2_memory(shim_params, &mut local_map, range);
    }

    let ram_buffer = if let Some(bounce_buffer) = shim_params.bounce_buffer {
        assert!(bounce_buffer.start() % X64_LARGE_PAGE_SIZE == 0);
        assert!(bounce_buffer.len() >= X64_LARGE_PAGE_SIZE);

        for range in memory_range::subtract_ranges(
            core::iter::once(bounce_buffer),
            partition_info.vtl2_ram.iter().map(|e| e.range),
        ) {
            accept_vtl2_memory(shim_params, &mut local_map, range);
        }

        // SAFETY: The bounce buffer is trusted as it is obtained from measured
        // shim parameters. The bootloader is identity mapped, and the PA is
        // guaranteed to be mapped as the pagetable is prebuilt and measured.
        unsafe {
            core::slice::from_raw_parts_mut(
                bounce_buffer.start() as *mut u8,
                bounce_buffer.len() as usize,
            )
        }
    } else {
        &mut []
    };

    // Iterate over all imported regions that are not already accepted. They must be accepted here.
    // TODO: No VTL0 memory is currently marked as pending.
    for (imported_range, already_accepted) in shim_params.imported_regions() {
        if !already_accepted {
            accept_pending_vtl2_memory(shim_params, &mut local_map, ram_buffer, imported_range);
        }
    }

    // TDX has specific memory initialization logic. Create a set of page tables for the APs
    // to use during the mailbox spinloop, and carve out memory for TDCALL based hypercalls
    if shim_params.isolation_type == IsolationType::Tdx {
        // Allocate a range of memory for AP page tables
        let page_table_region = address_space
            .allocate_aligned(
                None,
                PAGE_TABLE_MAX_BYTES as u64,
                AllocationType::TdxPageTables,
                AllocationPolicy::LowMemory,
                X64_LARGE_PAGE_SIZE,
            )
            .expect("allocation of space for TDX page tables must succeed");

        // The local map will map a single 2MB PTE per allocation
        const_assert!((PAGE_TABLE_MAX_BYTES as u64) < X64_LARGE_PAGE_SIZE);
        assert_eq!(page_table_region.range.start() % X64_LARGE_PAGE_SIZE, 0);

        let mut local_map = local_map.expect("must be present on TDX");
        let page_table_region_mapping = local_map.map_pages(page_table_region.range, false);
        page_table_region_mapping.data.fill(0);

        const MAX_RANGE_COUNT: usize = 64;
        let mut ranges = off_stack!(
            ArrayVec::<MappedRange, MAX_RANGE_COUNT>,
            ArrayVec::new_const()
        );

        // All VTL2_RAM ranges should be present as R+X in the AP page table mappings, the mailbox
        // wakeup vector will be somewhere in this range, below the 4GB boundary
        const AP_MEMORY_BOUNDARY: u64 = 4 * 1024 * 1024 * 1024;
        let vtl2_ram = address_space
            .vtl2_ranges()
            .filter_map(|(range, typ)| match typ {
                MemoryVtlType::VTL2_RAM => {
                    if range.start() < AP_MEMORY_BOUNDARY {
                        let end = if range.end() < AP_MEMORY_BOUNDARY {
                            range.end()
                        } else {
                            AP_MEMORY_BOUNDARY
                        };
                        Some(MappedRange::new(range.start(), end).read_only())
                    } else {
                        None
                    }
                }
                _ => None,
            });

        ranges.extend(vtl2_ram);

        // Map the reset vector as executable and writable, as the mailbox protocol uses offsets
        // in the reset vector to communicate with the kernel
        const PAGE_SIZE: u64 = 0x1000;
        ranges.push(MappedRange::new(
            x86defs::tdx::RESET_VECTOR_PAGE,
            x86defs::tdx::RESET_VECTOR_PAGE + PAGE_SIZE,
        ));

        ranges.sort_by_key(|r| r.start());

        let mut page_table_work_buffer =
            off_stack!(ArrayVec<PageTable, PAGE_TABLE_MAX_COUNT>, ArrayVec::new_const());
        for _ in 0..PAGE_TABLE_MAX_COUNT {
            page_table_work_buffer.push(PageTable::new_zeroed());
        }

        PageTableBuilder::new(
            page_table_region.range.start(),
            page_table_work_buffer.as_mut_slice(),
            page_table_region_mapping.data,
            ranges.as_slice(),
        )
        .expect("page table builder must return no error")
        .build()
        .expect("page table construction must succeed");

        crate::arch::tdx::tdx_prepare_ap_trampoline(page_table_region.range.start());

        // For TDVMCALL based hypercalls, take the first 2 MB region from ram_buffer for
        // hypercall IO pages. ram_buffer must not be used again beyond this point
        // TODO: find an approach that does not require re-using the ram_buffer
        let free_buffer = ram_buffer.as_mut_ptr() as u64;
        assert!(free_buffer.is_multiple_of(X64_LARGE_PAGE_SIZE));
        // SAFETY: The bottom 2MB region of the ram_buffer is unused by the shim
        // The region is aligned to 2MB, and mapped as a large page
        let tdx_io_page = unsafe {
            tdx_share_large_page(free_buffer);
            TdxHypercallPage::new(free_buffer)
        };
        hvcall().initialize_tdx(tdx_io_page);
    }
}

/// Accepts VTL2 memory in the specified gpa range.
fn accept_vtl2_memory(
    shim_params: &ShimParams,
    local_map: &mut Option<LocalMap<'_>>,
    range: MemoryRange,
) {
    match shim_params.isolation_type {
        IsolationType::Vbs => {
            hvcall()
                .accept_vtl2_pages(range, hvdef::hypercall::AcceptMemoryType::RAM)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Snp => {
            super::snp::set_page_acceptance(local_map.as_mut().unwrap(), range, true)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Tdx => {
            super::tdx::accept_pages(range).expect("accepting vtl2 memory must not fail")
        }
        _ => unreachable!(),
    }
}

/// Accepts VTL2 memory in the specified range that is currently marked as pending, i.e. not
/// yet assigned as exclusive and private.
fn accept_pending_vtl2_memory(
    shim_params: &ShimParams,
    local_map: &mut Option<LocalMap<'_>>,
    ram_buffer: &mut [u8],
    range: MemoryRange,
) {
    let isolation_type = shim_params.isolation_type;

    match isolation_type {
        IsolationType::Vbs => {
            hvcall()
                .accept_vtl2_pages(range, hvdef::hypercall::AcceptMemoryType::RAM)
                .expect("accepting vtl 2 memory must not fail");
        }
        IsolationType::Snp | IsolationType::Tdx => {
            let local_map = local_map.as_mut().unwrap();
            // Accepting pending memory for SNP is somewhat more complicated. The pending regions
            // are unencrypted pages. Accepting them would result in their contents being scrambled.
            // Instead their contents must be copied out to a private region, then copied back once
            // the pages have been accepted. Additionally, the access to the unencrypted pages must
            // happen with the C-bit cleared.
            let mut remaining = range;
            while !remaining.is_empty() {
                // Copy up to the next 2MB boundary.
                let range = MemoryRange::new(
                    remaining.start()
                        ..remaining.end().min(
                            (remaining.start() + X64_LARGE_PAGE_SIZE) & !(X64_LARGE_PAGE_SIZE - 1),
                        ),
                );
                remaining = MemoryRange::new(range.end()..remaining.end());

                let ram_buffer = &mut ram_buffer[..range.len() as usize];

                // Map the pages as shared and copy the necessary number to the buffer.
                {
                    let map_range = if isolation_type == IsolationType::Tdx {
                        // set vtom on the page number
                        MemoryRange::new(
                            range.start() | TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT
                                ..range.end() | TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT,
                        )
                    } else {
                        range
                    };

                    let mapping = local_map.map_pages(map_range, false);
                    ram_buffer.copy_from_slice(mapping.data);

                    // On SNP, evict the shared (C=0) cache lines for these
                    // pages while the C=0 mapping is still live.
                    if isolation_type == IsolationType::Snp {
                        let mapping_va = mapping.data.as_ptr() as u64;
                        for page_offset in
                            (0..mapping.data.len() as u64).step_by(hvdef::HV_PAGE_SIZE as usize)
                        {
                            super::snp::cache_lines_flush_page(mapping_va + page_offset);
                        }
                    }
                }

                // DIAG: record the SHA-384 of this chunk while it's still the
                // shared/host-loaded content, and feed it into a running
                // combined Phase-A hash for later comparison against the
                // measured expected hash.
                diag_record_phase_a(range.start(), &ram_buffer[..]);

                // Change visibility on the pages for this iteration.
                match isolation_type {
                    IsolationType::Snp => {
                        super::snp::Ghcb::change_page_visibility(range, false);
                    }
                    IsolationType::Tdx => {
                        super::tdx::change_page_visibility(range, false);
                    }
                    _ => unreachable!(),
                }

                // accept the pages.
                match isolation_type {
                    IsolationType::Snp => {
                        super::snp::set_page_acceptance(local_map, range, true)
                            .expect("accepting vtl 2 memory must not fail");
                    }
                    IsolationType::Tdx => {
                        super::tdx::accept_pages(range)
                            .expect("accepting vtl 2 memory must not fail");
                    }
                    _ => unreachable!(),
                }

                // Copy the buffer back. Use the identity map now that the memory has been accepted.
                {
                    // SAFETY: Known memory region that was just accepted.
                    let mapping = unsafe {
                        core::slice::from_raw_parts_mut(
                            range.start() as *mut u8,
                            range.len() as usize,
                        )
                    };

                    mapping.copy_from_slice(ram_buffer);
                }

                // DIAG: re-hash the chunk from the freshly written private
                // page (Phase B) and compare against the Phase-A bytes still
                // sitting in `ram_buffer`. A mismatch here means the accept/
                // copy-back path corrupted this chunk; per-page bitmap, RLE
                // ranges, per-corrupt-page SHA-384s, and (once, globally) a
                // full 4 KB dump of the first bad page are logged.
                {
                    // SAFETY: Same memory just written above; identity mapped.
                    let post = unsafe {
                        core::slice::from_raw_parts(
                            range.start() as *const u8,
                            range.len() as usize,
                        )
                    };
                    diag_verify_phase_b(range.start(), &ram_buffer[..post.len()], post);
                }
            }
        }
        _ => unreachable!(),
    }
}

// Verify the SHA384 hash of pages that were imported as unaccepted/shared. Compare against the
// desired hash that is passed in as a measured parameter. Failures result in a panic.
pub fn verify_imported_regions_hash(shim_params: &ShimParams) {
    // Non isolated VMs can undergo servicing, and thus the hash might no longer be valid,
    // as the memory regions can change during runtime.
    if let IsolationType::None = shim_params.isolation_type {
        return;
    }

    // If all imported pages are already accepted, there is no need to verify the hash.
    if shim_params
        .imported_regions()
        .all(|(_, already_accepted)| already_accepted)
    {
        return;
    }

    let mut hasher = Sha384::new();
    shim_params
        .imported_regions()
        .filter(|(_, already_accepted)| !already_accepted)
        .for_each(|(range, _)| {
            // SAFETY: The location and identity of the range is trusted as it is obtained from
            // measured shim parameters.
            let mapping = unsafe {
                core::slice::from_raw_parts(range.start() as *const u8, range.len() as usize)
            };
            hasher.update(mapping);
        });

    let final_hash: [u8; 48] = hasher.finalize().into();
    let expected = shim_params.imported_regions_hash();
    if final_hash.as_slice() != expected {
        log::error!(
            "DIAG_COMBINED_PHASE_C combined_phase_c={} expected={}",
            HexBytes(&final_hash),
            HexBytes(expected),
        );
        diag_report_phase_c(expected);
        panic!("Imported regions hash mismatch");
    }
}
