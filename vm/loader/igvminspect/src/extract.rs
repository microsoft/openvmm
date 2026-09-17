// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Extracts the constituent parts of an IGVM file into a directory tree.

use anyhow::Context;
use anyhow::ensure;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use igvm::IgvmPlatformHeader;
use igvm_defs::IgvmPlatformType;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use zerocopy::IntoBytes;

const PAGE_SIZE_4K: u64 = 4096;

/// A named region from the IGVM map file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MapEntry {
    compatibility_mask: u32,
    start_gpa: u64,
    end_gpa: u64,
    name: String,
}

fn parse_map_file(path: &Path, platforms: &[IgvmPlatformHeader]) -> anyhow::Result<Vec<MapEntry>> {
    let content = fs_err::read_to_string(path).context("reading map file")?;
    parse_map(&content, platforms)
}

fn parse_map(content: &str, platforms: &[IgvmPlatformHeader]) -> anyhow::Result<Vec<MapEntry>> {
    let mut entries = Vec::new();
    let mut compatibility_mask = None;
    let mut in_layout = false;

    for (line_number, line) in content.lines().enumerate() {
        if let Some(isolation) = line.strip_prefix("IGVM file isolation: ") {
            let platform_type = match isolation.split_whitespace().next() {
                Some("None" | "Vbs") => IgvmPlatformType::VSM_ISOLATION,
                Some("Snp") => IgvmPlatformType::SEV_SNP,
                Some("Tdx") => IgvmPlatformType::TDX,
                _ => anyhow::bail!("unknown map isolation on line {}", line_number + 1),
            };
            let mask = platforms
                .iter()
                .filter_map(|platform| {
                    let IgvmPlatformHeader::SupportedPlatform(info) = platform;
                    (info.platform_type == platform_type).then_some(info.compatibility_mask)
                })
                .fold(0, |mask, next| mask | next);
            ensure!(
                mask != 0,
                "map isolation {isolation} is not present in the IGVM file"
            );
            compatibility_mask = Some(mask);
            in_layout = false;
            continue;
        }
        if line.starts_with("IGVM file layout:") {
            ensure!(
                compatibility_mask.is_some(),
                "map layout has no isolation section"
            );
            in_layout = true;
            continue;
        }
        if in_layout {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("IGVM file ") {
                in_layout = false;
                continue;
            }
            let entry = parse_map_line(trimmed)
                .with_context(|| format!("invalid map layout on line {}", line_number + 1))?;
            for mask in mask_bits(compatibility_mask.context("map layout has no isolation")?) {
                entries.push(MapEntry {
                    compatibility_mask: mask,
                    ..entry.clone()
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        (a.compatibility_mask, a.start_gpa, a.end_gpa, &a.name).cmp(&(
            b.compatibility_mask,
            b.start_gpa,
            b.end_gpa,
            &b.name,
        ))
    });
    entries.dedup();
    for pair in entries.windows(2) {
        ensure!(
            pair[0].compatibility_mask != pair[1].compatibility_mask
                || pair[0].end_gpa <= pair[1].start_gpa,
            "overlapping map regions for mask {:#x}: {:?} and {:?}",
            pair[0].compatibility_mask,
            pair[0].name,
            pair[1].name,
        );
    }
    Ok(entries)
}

fn parse_map_line(line: &str) -> anyhow::Result<MapEntry> {
    // Format: "0000000000100000 - 0000000000700000 (0x600000 bytes) uefi-image"
    let (addresses, name) = line.split_once(')').context("missing region name")?;
    let name = name.trim().to_string();
    ensure!(!name.is_empty(), "empty region name");
    let tokens: Vec<&str> = addresses.split_whitespace().collect();
    ensure!(
        tokens.len() >= 3 && tokens[1] == "-",
        "invalid region addresses"
    );
    let start_gpa = u64::from_str_radix(tokens[0], 16).context("invalid start GPA")?;
    let end_gpa = u64::from_str_radix(tokens[2], 16).context("invalid end GPA")?;
    ensure!(
        start_gpa < end_gpa
            && start_gpa.is_multiple_of(PAGE_SIZE_4K)
            && end_gpa.is_multiple_of(PAGE_SIZE_4K),
        "map region must be nonempty and page aligned"
    );
    Ok(MapEntry {
        compatibility_mask: 0,
        start_gpa,
        end_gpa,
        name,
    })
}

/// Look up which map entry a GPA belongs to. Returns the entry name, or None.
fn lookup_map_name(map: &[MapEntry], compatibility_mask: u32, gpa: u64) -> Option<&str> {
    let idx =
        map.partition_point(|e| (e.compatibility_mask, e.start_gpa) <= (compatibility_mask, gpa));
    let entry = map.get(idx.checked_sub(1)?)?;
    (entry.compatibility_mask == compatibility_mask && gpa < entry.end_gpa)
        .then_some(entry.name.as_str())
}

fn mask_bits(mask: u32) -> impl Iterator<Item = u32> {
    (0..32)
        .map(|bit| 1u32 << bit)
        .filter(move |bit| mask & bit != 0)
}

/// A collected PageData entry, used for coalescing into contiguous regions.
struct PageDataEntry<'a> {
    compatibility_mask: u32,
    gpa: u64,
    end_gpa: u64,
    flags: String,
    data_type: String,
    data: &'a [u8],
    component: String, // from map lookup, or "unmapped"
}

/// Extract an IGVM file's logical parts into a directory tree.
fn extract_igvm_to_dir(igvm: &IgvmFile, dir: &Path, map: &[MapEntry]) -> anyhow::Result<()> {
    let mut page_data_entries = Vec::new();
    for directive in igvm.directives() {
        if let IgvmDirectiveHeader::PageData {
            gpa,
            compatibility_mask,
            flags,
            data_type,
            data,
        } = directive
        {
            ensure!(!flags.is_2mb_page(), "2MB page data is not supported");
            ensure!(data.len() <= PAGE_SIZE_4K as usize, "page data exceeds 4KB");
            let end_gpa = gpa
                .checked_add(PAGE_SIZE_4K)
                .context("page end GPA overflows")?;
            // Keep each platform's page stream, even when payloads happen to match.
            // A zero mask is retained as well, rather than silently dropping data.
            for mask in
                mask_bits(*compatibility_mask).chain((*compatibility_mask == 0).then_some(0))
            {
                page_data_entries.push(PageDataEntry {
                    compatibility_mask: mask,
                    gpa: *gpa,
                    end_gpa,
                    flags: format!("{flags:?}"),
                    data_type: format!("{data_type:?}"),
                    data,
                    component: lookup_map_name(map, mask, *gpa)
                        .unwrap_or("unmapped")
                        .to_string(),
                });
            }
        }
    }

    // Exclusive directory creation rejects stale output and pre-existing symlinks.
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs_err::create_dir_all(parent)?;
    }
    fs_err::create_dir(dir).context("output directory must not already exist")?;
    let headers_dir = dir.join("headers");
    let regions_dir = dir.join("regions");
    let vp_context_dir = dir.join("vp_context");
    let parameter_areas_dir = dir.join("parameter_areas");

    fs_err::create_dir(&headers_dir)?;
    fs_err::create_dir(&regions_dir)?;
    fs_err::create_dir(&vp_context_dir)?;
    fs_err::create_dir(&parameter_areas_dir)?;

    // Write platform headers
    {
        let mut f = create_file(&headers_dir.join("platforms.txt"))?;
        for (i, p) in igvm.platforms().iter().enumerate() {
            writeln!(f, "[{i}] {p:#?}")?;
        }
    }

    // Write initialization headers
    {
        let mut f = create_file(&headers_dir.join("initializations.txt"))?;
        for (i, h) in igvm.initializations().iter().enumerate() {
            writeln!(f, "[{i}] {h:#?}")?;
        }
    }

    let mut metadata_lines: Vec<String> = Vec::new();
    let mut snp_vp_count: u32 = 0;
    let mut native_vp_count: u32 = 0;
    let mut x64_vbs_vtl_count: HashMap<String, u32> = HashMap::new();
    let mut aarch64_vbs_vtl_count: HashMap<String, u32> = HashMap::new();

    for (directive_index, directive) in igvm.directives().iter().enumerate() {
        match directive {
            IgvmDirectiveHeader::PageData {
                gpa,
                compatibility_mask,
                flags,
                data_type,
                data: _,
            } => {
                metadata_lines.push(format!(
                    "[{directive_index}] PageData {{ gpa: {gpa:#x}, compatibility_mask: {compatibility_mask:#x}, flags: {flags:?}, data_type: {data_type:?} }}"
                ));
            }
            IgvmDirectiveHeader::ParameterArea {
                number_of_bytes,
                parameter_area_index,
                initial_data,
            } => {
                let name = format!("area_{parameter_area_index:04}_{directive_index:04}.bin");
                write_file(&parameter_areas_dir.join(&name), initial_data)?;
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
                write_file(&vp_context_dir.join(&name), vmsa.as_bytes())?;
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
                write_file(&vp_context_dir.join(&name), context.as_bytes())?;
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
                let mut f = create_file(&vp_context_dir.join(&name))?;
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
                let mut f = create_file(&vp_context_dir.join(&name))?;
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
        let mut f = create_file(&dir.join("metadata.txt"))?;
        for line in &metadata_lines {
            writeln!(f, "{line}")?;
        }
    }

    // Coalesce PageData into map-aware named regions and write them
    write_coalesced_regions(&page_data_entries, &regions_dir, &dir.join("regions.txt"))?;

    // Remove empty directories to keep the tree clean
    remove_dir_if_empty(&vp_context_dir)?;
    remove_dir_if_empty(&parameter_areas_dir)?;

    Ok(())
}

fn create_file(path: &Path) -> std::io::Result<fs_err::File> {
    fs_err::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    create_file(path)?.write_all(data)
}

fn remove_dir_if_empty(dir: &Path) -> std::io::Result<()> {
    if fs_err::read_dir(dir)?.next().is_none() {
        fs_err::remove_dir(dir)?;
    }
    Ok(())
}

/// Return a file extension appropriate for the component's actual content format.
fn extension_for_component(component: &str) -> &'static str {
    match component {
        "underhill-initrd" => "cpio.gz",
        "underhill-command-line" | "underhill-vtl0-linux-command-line" => "txt",
        "underhill-device-tree" => "dtb",
        _ => "bin",
    }
}

/// Coalesce sorted PageData entries into contiguous regions, splitting at
/// component boundaries from the map file. Write binary files + index.
fn write_coalesced_regions(
    entries: &[PageDataEntry<'_>],
    regions_dir: &Path,
    index_path: &Path,
) -> anyhow::Result<()> {
    let mut sorted: Vec<usize> = (0..entries.len()).collect();
    sorted.sort_by_key(|&i| (entries[i].compatibility_mask, entries[i].gpa));

    struct Region {
        compatibility_mask: u32,
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
        let previous = regions.last_mut().filter(|last| {
            entry.gpa == last.end_gpa
                && entry.compatibility_mask == last.compatibility_mask
                && entry.component == last.component
                && entry.flags == last.flags
                && entry.data_type == last.data_type
        });

        if let Some(last) = previous {
            let new_len = last
                .data
                .len()
                .checked_add(PAGE_SIZE_4K as usize)
                .context("region data size overflows")?;
            last.data.extend_from_slice(entry.data);
            last.data.resize(new_len, 0);
            last.end_gpa = entry.end_gpa;
            last.page_count += 1;
        } else {
            let mut data = entry.data.to_vec();
            data.resize(PAGE_SIZE_4K as usize, 0);
            regions.push(Region {
                compatibility_mask: entry.compatibility_mask,
                start_gpa: entry.gpa,
                end_gpa: entry.end_gpa,
                page_count: 1,
                flags: entry.flags.clone(),
                data_type: entry.data_type.clone(),
                component: entry.component.clone(),
                data,
            });
        }
    }

    let mut index = String::new();

    for (idx, region) in regions.iter().enumerate() {
        let ext = extension_for_component(&region.component);
        // An independent numeric prefix prevents collisions between encoded labels,
        // including on case-insensitive filesystems.
        let filename = format!("{idx:04}_{}.{ext}", encode_component(&region.component));

        write_file(&regions_dir.join(&filename), &region.data)?;
        writeln!(
            index,
            "{filename}: compatibility_mask={:#x} gpa=0x{:08x}..0x{:08x} pages={} flags={} data_type={} component={:?}",
            region.compatibility_mask,
            region.start_gpa,
            region.end_gpa,
            region.page_count,
            region.flags,
            region.data_type,
            region.component,
        )?;
    }

    write_file(index_path, index.as_bytes())?;

    Ok(())
}

fn encode_component(component: &str) -> String {
    let mut encoded = String::new();
    for byte in component.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

/// Extract an IGVM file's constituent parts into a directory tree.
///
/// Parses the IGVM binary and an optional `.bin.map` file, then writes
/// headers, regions, VP context, parameter areas, and metadata into the
/// given output directory. The map file provides human-readable component
/// names for memory regions; if no map is provided, all regions are labeled
/// "unmapped".
pub fn extract_igvm_file(
    igvm_path: &Path,
    map_path: Option<&Path>,
    output_dir: &Path,
) -> anyhow::Result<()> {
    let data = crate::read_igvm_image(igvm_path)?;
    let igvm = IgvmFile::new_from_binary(&data, None)
        .with_context(|| format!("parsing IGVM file {}", igvm_path.display()))?;

    let map = match map_path {
        Some(p) => parse_map_file(p, igvm.platforms()).context("parsing map file")?,
        None => Vec::new(),
    };

    extract_igvm_to_dir(&igvm, output_dir, &map).context("extracting IGVM file")?;

    eprintln!("Extracted to: {}", output_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use igvm::IgvmRevision;
    use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
    use igvm_defs::IgvmPageDataFlags;
    use igvm_defs::IgvmPageDataType;
    use test_with_tracing::test;

    fn platforms() -> Vec<IgvmPlatformHeader> {
        [
            (1, IgvmPlatformType::VSM_ISOLATION),
            (2, IgvmPlatformType::SEV_SNP),
        ]
        .into_iter()
        .map(|(compatibility_mask, platform_type)| {
            IgvmPlatformHeader::SupportedPlatform(IGVM_VHS_SUPPORTED_PLATFORM {
                compatibility_mask,
                highest_vtl: 0,
                platform_type,
                platform_version: 1,
                shared_gpa_boundary: 0,
            })
        })
        .collect()
    }

    fn page(gpa: u64, compatibility_mask: u32, byte: u8) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::PageData {
            gpa,
            compatibility_mask,
            flags: IgvmPageDataFlags::new(),
            data_type: IgvmPageDataType::NORMAL,
            data: vec![byte; PAGE_SIZE_4K as usize],
        }
    }

    fn image(directives: Vec<IgvmDirectiveHeader>) -> IgvmFile {
        IgvmFile::new(IgvmRevision::V1, platforms(), vec![], directives).unwrap()
    }

    fn extract_to(dir: &Path, directives: Vec<IgvmDirectiveHeader>, map: &[MapEntry]) {
        extract_igvm_to_dir(&image(directives), dir, map).unwrap();
    }

    fn region_files(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut paths: Vec<_> = fs_err::read_dir(dir.join("regions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn preserves_platform_payloads_and_coalesces_within_each_mask() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let input = temp.path().join("image.bin");
        let igvm = image(vec![
            page(0x1000, 1, 0x11),
            page(0x1000, 2, 0x22),
            page(0x2000, 3, 0x33),
        ]);
        let mut binary = Vec::new();
        igvm.serialize(&mut binary).unwrap();
        fs_err::write(&input, binary).unwrap();
        extract_igvm_file(&input, None, &output).unwrap();

        let files = region_files(&output);
        assert_eq!(files.len(), 2);
        for (file, byte) in files.iter().zip([0x11, 0x22]) {
            let data = fs_err::read(file).unwrap();
            assert_eq!(&data[..4096], &[byte; 4096]);
            assert_eq!(&data[4096..], &[0x33; 4096]);
        }
        let index = fs_err::read_to_string(output.join("regions.txt")).unwrap();
        assert!(index.contains("compatibility_mask=0x1"));
        assert!(index.contains("compatibility_mask=0x2"));
        assert!(index.contains("pages=2"));
        let metadata = fs_err::read_to_string(output.join("metadata.txt")).unwrap();
        assert!(metadata.contains("compatibility_mask: 0x3"));
    }

    #[test]
    fn preserves_pages_with_different_flags_and_types() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let mut unmeasured = page(0x1000, 2, 0x44);
        let IgvmDirectiveHeader::PageData {
            flags, data_type, ..
        } = &mut unmeasured
        else {
            unreachable!()
        };
        *flags = flags.with_unmeasured(true);
        *data_type = IgvmPageDataType::CPUID_DATA;
        extract_to(&output, vec![page(0x1000, 1, 0x44), unmeasured], &[]);
        assert_eq!(region_files(&output).len(), 2);
        let index = fs_err::read_to_string(output.join("regions.txt")).unwrap();
        assert!(index.contains("CPUID_DATA"));
        assert!(index.contains("unmeasured: true"));
    }

    #[test]
    fn keeps_overlapping_platform_maps_separate() {
        let map = parse_map(
            "IGVM file isolation: None\n\
             IGVM file layout:\n\
             0000000000001000 - 0000000000009000 (0x8000 bytes) vbs\n\
             IGVM file isolation: Snp { policy: 0 }\n\
             IGVM file layout:\n\
             0000000000002000 - 0000000000003000 (0x1000 bytes) snp\n\
             000000000000a000 - 000000000000b000 (0x1000 bytes) snp-tail\n\
             IGVM file reported ranges:\n\
             0000000000001000 - 0000000000009000 (0x8000 bytes) not-layout\n",
            &platforms(),
        )
        .unwrap();
        assert_eq!(lookup_map_name(&map, 1, 0x4000), Some("vbs"));
        assert_eq!(lookup_map_name(&map, 2, 0x4000), None);
        assert_eq!(lookup_map_name(&map, 2, 0x2000), Some("snp"));
        assert_eq!(lookup_map_name(&map, 1, 0x9000), None);
        assert_eq!(lookup_map_name(&map, 0, 0x2000), None);
    }

    #[test]
    fn shared_pages_use_platform_specific_names() {
        let map = parse_map(
            "IGVM file isolation: None\n\
             IGVM file layout:\n\
             0000000000001000 - 0000000000002000 (0x1000 bytes) vbs\n\
             IGVM file isolation: Snp { policy: 0 }\n\
             IGVM file layout:\n\
             0000000000001000 - 0000000000002000 (0x1000 bytes) snp\n",
            &platforms(),
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        extract_to(&output, vec![page(0x1000, 3, 0x11)], &map);
        assert_eq!(
            fs_err::read(output.join("regions/0000_vbs.bin")).unwrap(),
            vec![0x11; 4096]
        );
        assert_eq!(
            fs_err::read(output.join("regions/0001_snp.bin")).unwrap(),
            vec![0x11; 4096]
        );
    }

    #[test]
    fn rejects_invalid_or_ambiguous_maps() {
        for lines in [
            "not a layout entry",
            "0000000000002000 - 0000000000001000 (0 bytes) reversed",
            "0000000000001001 - 0000000000002000 (0xfff bytes) unaligned",
            "0000000000001000 - 0000000000002000 (0x1000 bytes) ",
            "0000000000001000 - 0000000000003000 (0x2000 bytes) first\n\
             0000000000002000 - 0000000000004000 (0x2000 bytes) second",
        ] {
            let content = format!("IGVM file isolation: None\nIGVM file layout:\n{lines}\n");
            assert!(parse_map(&content, &platforms()).is_err(), "{content}");
        }
        assert!(parse_map("IGVM file layout:\n", &platforms()).is_err());
        assert!(parse_map("IGVM file isolation: Unknown\n", &platforms()).is_err());
    }

    #[test]
    fn encodes_untrusted_names_and_avoids_filename_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let sentinel = temp.path().join("image.bin");
        fs_err::write(&sentinel, b"original").unwrap();
        let names = [
            "../../image",
            "/absolute",
            r"C:\image",
            "foo",
            "foo",
            "foo_0",
            "FOO",
        ];
        let mut map = Vec::new();
        let mut pages = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let gpa = (i as u64 + 1) * 0x2000;
            map.push(MapEntry {
                compatibility_mask: 1,
                start_gpa: gpa,
                end_gpa: gpa + 4096,
                name: name.to_string(),
            });
            pages.push(page(gpa, 1, i as u8));
        }
        extract_to(&output, pages, &map);
        assert_eq!(fs_err::read(&sentinel).unwrap(), b"original");
        let files = region_files(&output);
        assert_eq!(files.len(), names.len());
        for (i, file) in files.iter().enumerate() {
            assert_eq!(file.parent(), Some(output.join("regions").as_path()));
            assert_eq!(fs_err::read(file).unwrap(), vec![i as u8; 4096]);
        }
        assert_eq!(encode_component("../../image"), "%2E%2E%2F%2E%2E%2Fimage");
        let index = fs_err::read_to_string(output.join("regions.txt")).unwrap();
        assert!(index.contains("component=\"../../image\""));
    }

    #[test]
    fn refuses_existing_output_without_modifying_it() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        extract_to(&output, vec![page(0x1000, 1, 1), page(0x3000, 1, 2)], &[]);
        let index = fs_err::read(output.join("regions.txt")).unwrap();
        assert!(extract_igvm_to_dir(&image(vec![]), &output, &[]).is_err());
        assert_eq!(fs_err::read(output.join("regions.txt")).unwrap(), index);
        assert_eq!(region_files(&output).len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_output_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(extract_igvm_to_dir(&image(vec![]), &link, &[]).is_err());
        assert!(!temp.path().join("headers").exists());

        let target = temp.path().join("target");
        let file_link = temp.path().join("file-link");
        fs_err::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &file_link).unwrap();
        assert!(write_file(&file_link, b"replacement").is_err());
        assert_eq!(fs_err::read(&target).unwrap(), b"original");
    }

    #[test]
    fn rejects_overflow_before_creating_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let igvm = image(vec![page(u64::MAX - 4095, 1, 0)]);
        let error = extract_igvm_to_dir(&igvm, &output, &[]).unwrap_err();
        assert!(error.to_string().contains("overflows"));
        assert!(!output.exists());
        extract_to(&output, vec![page(u64::MAX - 8191, 1, 0)], &[]);
    }

    #[test]
    fn empty_images_have_an_empty_region_index() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        extract_to(&output, vec![], &[]);
        assert!(fs_err::read(output.join("regions.txt")).unwrap().is_empty());
        assert!(!output.join("vp_context").exists());
        assert!(!output.join("parameter_areas").exists());
    }
}
