// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements IGVM file diffing by extracting constituent parts and running diffoscope.

use anyhow::Context;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use zerocopy::IntoBytes;

const PAGE_SIZE_4K: u64 = 4096;

/// A named region from the IGVM map file.
#[derive(Debug, Clone)]
struct MapEntry {
    start_gpa: u64,
    end_gpa: u64,
    name: String,
}

/// Parse an IGVM .map file, extracting all layout entries across all isolation sections.
/// Deduplicates by (start_gpa, end_gpa).
fn parse_map_file(path: &Path) -> anyhow::Result<Vec<MapEntry>> {
    let content = fs_err::read_to_string(path).context("reading map file")?;
    let mut entries: Vec<MapEntry> = Vec::new();
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut in_layout = false;

    for line in content.lines() {
        if line.starts_with("IGVM file layout:") {
            in_layout = true;
            continue;
        }
        if in_layout {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("IGVM file required memory:")
                || trimmed.starts_with("IGVM file relocatable regions:")
                || trimmed.starts_with("IGVM file isolation:")
            {
                in_layout = false;
                continue;
            }
            // Parse: "  0000000000100000 - 0000000000700000 (0x600000 bytes) uefi-image"
            if let Some(entry) = parse_map_line(trimmed) {
                if seen.insert((entry.start_gpa, entry.end_gpa)) {
                    entries.push(entry);
                }
            }
        }
    }

    entries.sort_by_key(|e| e.start_gpa);
    Ok(entries)
}

fn parse_map_line(line: &str) -> Option<MapEntry> {
    // Format: "0000000000100000 - 0000000000700000 (0x600000 bytes) uefi-image"
    let parts: Vec<&str> = line.splitn(2, ')').collect();
    if parts.len() != 2 {
        return None;
    }
    let name = parts[1].trim().to_string();
    let addr_part = parts[0]; // "0000000000100000 - 0000000000700000 (0x600000 bytes"
    let tokens: Vec<&str> = addr_part.split_whitespace().collect();
    if tokens.len() < 3 || tokens[1] != "-" {
        return None;
    }
    let start_gpa = u64::from_str_radix(tokens[0], 16).ok()?;
    let end_gpa = u64::from_str_radix(tokens[2], 16).ok()?;
    Some(MapEntry {
        start_gpa,
        end_gpa,
        name,
    })
}

/// Look up which map entry a GPA belongs to. Returns the entry name, or None.
fn lookup_map_name(map: &[MapEntry], gpa: u64) -> Option<&str> {
    // Binary search for the entry containing this GPA
    match map.binary_search_by(|e| {
        if gpa < e.start_gpa {
            std::cmp::Ordering::Greater
        } else if gpa >= e.end_gpa {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(idx) => Some(&map[idx].name),
        Err(_) => None,
    }
}

/// A collected PageData entry, used for coalescing into contiguous regions.
struct PageDataEntry {
    gpa: u64,
    flags: String,
    data_type: String,
    data: Vec<u8>,
    component: String, // from map lookup, or "unmapped"
}

/// Extract an IGVM file's logical parts into a directory tree.
fn extract_igvm_to_dir(igvm: &IgvmFile, dir: &Path, map: &[MapEntry]) -> anyhow::Result<()> {
    let headers_dir = dir.join("headers");
    let regions_dir = dir.join("regions");
    let vp_context_dir = dir.join("vp_context");
    let parameter_areas_dir = dir.join("parameter_areas");

    fs_err::create_dir_all(&headers_dir)?;
    fs_err::create_dir_all(&regions_dir)?;
    fs_err::create_dir_all(&vp_context_dir)?;
    fs_err::create_dir_all(&parameter_areas_dir)?;

    // Write platform headers
    {
        let mut f = fs_err::File::create(headers_dir.join("platforms.txt"))?;
        for (i, p) in igvm.platforms().iter().enumerate() {
            writeln!(f, "[{i}] {p:#?}")?;
        }
    }

    // Write initialization headers
    {
        let mut f = fs_err::File::create(headers_dir.join("initializations.txt"))?;
        for (i, h) in igvm.initializations().iter().enumerate() {
            writeln!(f, "[{i}] {h:#?}")?;
        }
    }

    let mut page_data_entries: Vec<PageDataEntry> = Vec::new();
    let mut metadata_lines: Vec<String> = Vec::new();
    let mut snp_vp_count: u32 = 0;
    let mut native_vp_count: u32 = 0;
    let mut x64_vbs_vtl_count: HashMap<String, u32> = HashMap::new();
    let mut aarch64_vbs_vtl_count: HashMap<String, u32> = HashMap::new();

    for directive in igvm.directives() {
        match directive {
            IgvmDirectiveHeader::PageData {
                gpa,
                compatibility_mask: _,
                flags,
                data_type,
                data,
            } => {
                let component = lookup_map_name(map, *gpa).unwrap_or("unmapped").to_string();
                // Deduplicate pages at the same GPA (different compatibility masks
                // produce duplicate PageData entries with identical content).
                if !page_data_entries.iter().any(|e| e.gpa == *gpa) {
                    page_data_entries.push(PageDataEntry {
                        gpa: *gpa,
                        flags: format!("{flags:?}"),
                        data_type: format!("{data_type:?}"),
                        data: data.clone(),
                        component,
                    });
                }
            }
            IgvmDirectiveHeader::ParameterArea {
                number_of_bytes,
                parameter_area_index,
                initial_data,
            } => {
                let name = format!("area_{parameter_area_index:04}.bin");
                fs_err::write(parameter_areas_dir.join(&name), initial_data)?;
                metadata_lines.push(format!(
                    "ParameterArea {{ index: {parameter_area_index}, number_of_bytes: {number_of_bytes} }}"
                ));
            }
            IgvmDirectiveHeader::SnpVpContext {
                gpa,
                compatibility_mask,
                vp_index,
                vmsa,
            } => {
                let name = format!("snp_vp{snp_vp_count}.bin");
                snp_vp_count += 1;
                fs_err::write(vp_context_dir.join(&name), vmsa.as_bytes())?;
                metadata_lines.push(format!(
                    "SnpVpContext {{ gpa: {gpa:#x}, compatibility_mask: {compatibility_mask:#x}, vp_index: {vp_index} }}"
                ));
            }
            IgvmDirectiveHeader::X64NativeVpContext {
                compatibility_mask,
                vp_index,
                context,
            } => {
                let name = format!("x64_native_vp{native_vp_count}.bin");
                native_vp_count += 1;
                fs_err::write(vp_context_dir.join(&name), context.as_bytes())?;
                metadata_lines.push(format!(
                    "X64NativeVpContext {{ compatibility_mask: {compatibility_mask:#x}, vp_index: {vp_index} }}"
                ));
            }
            IgvmDirectiveHeader::X64VbsVpContext {
                vtl,
                registers,
                compatibility_mask,
            } => {
                let vtl_str = format!("{vtl:?}");
                let count = x64_vbs_vtl_count.entry(vtl_str.clone()).or_insert(0);
                let name = format!("x64_vbs_{vtl_str}_vp{count}.txt");
                *count += 1;
                let mut f = fs_err::File::create(vp_context_dir.join(&name))?;
                writeln!(f, "compatibility_mask: {compatibility_mask:#x}")?;
                writeln!(f, "vtl: {vtl:?}")?;
                writeln!(f, "registers:")?;
                for reg in registers {
                    writeln!(f, "  {reg:#?}")?;
                }
            }
            IgvmDirectiveHeader::AArch64VbsVpContext {
                vtl,
                registers,
                compatibility_mask,
            } => {
                let vtl_str = format!("{vtl:?}");
                let count = aarch64_vbs_vtl_count.entry(vtl_str.clone()).or_insert(0);
                let name = format!("aarch64_vbs_{vtl_str}_vp{count}.txt");
                *count += 1;
                let mut f = fs_err::File::create(vp_context_dir.join(&name))?;
                writeln!(f, "compatibility_mask: {compatibility_mask:#x}")?;
                writeln!(f, "vtl: {vtl:?}")?;
                writeln!(f, "registers:")?;
                for reg in registers {
                    writeln!(f, "  {reg:#?}")?;
                }
            }
            // All other directives go to metadata.txt as debug-formatted text
            other => {
                metadata_lines.push(format!("{other:#?}"));
            }
        }
    }

    // Write metadata.txt
    {
        let mut f = fs_err::File::create(dir.join("metadata.txt"))?;
        for line in &metadata_lines {
            writeln!(f, "{line}")?;
        }
    }

    // Coalesce PageData into map-aware named regions and write them
    write_coalesced_regions(&page_data_entries, &regions_dir)?;

    // Remove empty directories to keep the tree clean
    let _ = remove_dir_if_empty(&vp_context_dir);
    let _ = remove_dir_if_empty(&parameter_areas_dir);

    Ok(())
}

fn remove_dir_if_empty(dir: &Path) -> std::io::Result<()> {
    if fs_err::read_dir(dir)?.next().is_none() {
        fs_err::remove_dir(dir)?;
    }
    Ok(())
}

/// Coalesce sorted PageData entries into contiguous regions, splitting at
/// component boundaries from the map file. Write binary files + index.
fn write_coalesced_regions(entries: &[PageDataEntry], regions_dir: &Path) -> anyhow::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    // Sort by GPA
    let mut sorted: Vec<usize> = (0..entries.len()).collect();
    sorted.sort_by_key(|&i| entries[i].gpa);

    struct Region {
        start_gpa: u64,
        end_gpa: u64,
        page_count: u64,
        flags: String,
        data_type: String,
        component: String,
        data: Vec<u8>,
    }

    let mut regions: Vec<Region> = Vec::new();

    for &idx in &sorted {
        let entry = &entries[idx];
        let page_data_len = entry.data.len().max(PAGE_SIZE_4K as usize);

        // Merge only if contiguous, same component, and same flags/data_type
        let can_merge = if let Some(last) = regions.last() {
            entry.gpa == last.end_gpa
                && entry.component == last.component
                && entry.flags == last.flags
                && entry.data_type == last.data_type
        } else {
            false
        };

        if can_merge {
            let last = regions.last_mut().unwrap();
            let expected_len = (last.page_count as usize) * (PAGE_SIZE_4K as usize);
            last.data.resize(expected_len, 0);
            let mut page_buf = entry.data.clone();
            page_buf.resize(page_data_len, 0);
            last.data.extend_from_slice(&page_buf);
            last.end_gpa = entry.gpa + PAGE_SIZE_4K;
            last.page_count += 1;
        } else {
            let mut data = entry.data.clone();
            data.resize(page_data_len, 0);
            regions.push(Region {
                start_gpa: entry.gpa,
                end_gpa: entry.gpa + PAGE_SIZE_4K,
                page_count: 1,
                flags: entry.flags.clone(),
                data_type: entry.data_type.clone(),
                component: entry.component.clone(),
                data,
            });
        }
    }

    // Assign filenames: use component name, with a counter for disambiguation
    // when the same component produces multiple regions.
    let mut name_counts: BTreeMap<String, u32> = BTreeMap::new();
    for region in &regions {
        *name_counts.entry(region.component.clone()).or_insert(0) += 1;
    }

    let mut name_indices: HashMap<String, u32> = HashMap::new();
    let mut index = String::new();

    for region in &regions {
        let total = name_counts[&region.component];
        let idx = name_indices.entry(region.component.clone()).or_insert(0);
        let filename = if total == 1 {
            format!("{}.bin", region.component)
        } else {
            format!("{}_{}.bin", region.component, idx)
        };
        *idx += 1;

        fs_err::write(regions_dir.join(&filename), &region.data)?;
        writeln!(
            index,
            "{filename}: gpa=0x{:08x}..0x{:08x} pages={} flags={} data_type={}",
            region.start_gpa, region.end_gpa, region.page_count, region.flags, region.data_type,
        )?;
    }

    fs_err::write(regions_dir.parent().unwrap().join("regions.txt"), &index)?;

    Ok(())
}

/// Diff two IGVM files by extracting their parts and running diffoscope.
pub fn diff_igvm_files(
    left: &Path,
    right: &Path,
    left_map_path: &Path,
    right_map_path: &Path,
    keep_extracted: bool,
    diffoscope_args: &[String],
) -> anyhow::Result<()> {
    // Parse map files
    let left_map = parse_map_file(left_map_path).context("parsing left map file")?;
    let right_map = parse_map_file(right_map_path).context("parsing right map file")?;

    // Parse both IGVM files
    let left_data = fs_err::read(left).context("reading left IGVM file")?;
    let right_data = fs_err::read(right).context("reading right IGVM file")?;

    let left_igvm =
        IgvmFile::new_from_binary(&left_data, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let right_igvm =
        IgvmFile::new_from_binary(&right_data, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;

    // Create temp directories
    let left_tmp = tempfile::Builder::new()
        .prefix("igvm-diff-left-")
        .tempdir()
        .context("creating left temp dir")?;
    let right_tmp = tempfile::Builder::new()
        .prefix("igvm-diff-right-")
        .tempdir()
        .context("creating right temp dir")?;

    let left_dir = left_tmp.path();
    let right_dir = right_tmp.path();

    // Extract both IGVM files
    extract_igvm_to_dir(&left_igvm, left_dir, &left_map).context("extracting left IGVM file")?;
    extract_igvm_to_dir(&right_igvm, right_dir, &right_map)
        .context("extracting right IGVM file")?;

    eprintln!("Left extracted to:  {}", left_dir.display());
    eprintln!("Right extracted to: {}", right_dir.display());

    // Run diffoscope
    let mut cmd = std::process::Command::new("diffoscope");
    cmd.arg("--exclude-directory-metadata=yes");
    cmd.arg(left_dir).arg(right_dir);
    for arg in diffoscope_args {
        cmd.arg(arg);
    }

    let status = cmd
        .status()
        .context("running diffoscope (is it installed? try: pip install diffoscope)")?;

    if keep_extracted {
        let left_path = left_tmp.keep();
        let right_path = right_tmp.keep();
        eprintln!("Keeping extracted directories:");
        eprintln!("  left:  {}", left_path.display());
        eprintln!("  right: {}", right_path.display());
    }

    // diffoscope exits 0 if identical, 1 if differences found
    match status.code() {
        Some(0) => {
            eprintln!("No differences found.");
            Ok(())
        }
        Some(1) => {
            // Differences found is not an error — it's expected behavior
            Ok(())
        }
        Some(code) => {
            anyhow::bail!("diffoscope exited with code {code}");
        }
        None => {
            anyhow::bail!("diffoscope was terminated by signal");
        }
    }
}
