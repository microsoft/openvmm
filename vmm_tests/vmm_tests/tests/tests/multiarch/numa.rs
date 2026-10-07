// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NUMA topology integration tests.

use anyhow::Context;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaDistance;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::VpAssignment;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::PipetteClient;
use pipette_client::cmd;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;

const SIZE_1_GB: u64 = 1024 * 1024 * 1024;
const SIZE_2_GB: u64 = 2 * SIZE_1_GB;

fn make_mem(size: u64, private_memory: bool) -> MemoryConfig {
    MemoryConfig {
        mem_size: size,
        prefetch_memory: false,
        private_memory,
        transparent_hugepages: false,
        hugepages: false,
        hugepage_size: None,
        host_numa_node: None,
    }
}

/// Read the number of NUMA nodes visible in the guest.
async fn guest_numa_node_count(agent: &PipetteClient) -> anyhow::Result<usize> {
    let sh = agent.unix_shell();
    let output = cmd!(sh, "sh -c 'ls -d /sys/devices/system/node/node* | wc -l'")
        .read()
        .await
        .context("listing NUMA nodes")?;
    Ok(output.trim().parse()?)
}

/// Read the CPU list for a given NUMA node (e.g. "0-1" or "0,3").
async fn guest_node_cpulist(agent: &PipetteClient, node: u32) -> anyhow::Result<String> {
    let sh = agent.unix_shell();
    let path = format!("/sys/devices/system/node/node{node}/cpulist");
    let output = sh
        .read_file(&path)
        .await
        .with_context(|| format!("reading cpulist for node {node}"))?;
    Ok(output.trim().to_string())
}

/// Read the memory size (in bytes) for a given NUMA node from its meminfo.
async fn guest_node_mem_bytes(agent: &PipetteClient, node: u32) -> anyhow::Result<u64> {
    let sh = agent.unix_shell();
    let path = format!("/sys/devices/system/node/node{node}/meminfo");
    let output = sh
        .read_file(&path)
        .await
        .with_context(|| format!("reading meminfo for node {node}"))?;
    // Parse "Node N MemTotal:     XXXXX kB"
    for line in output.lines() {
        if line.contains("MemTotal") {
            let kb: u64 = line
                .split_whitespace()
                .rev()
                .nth(1) // second-to-last token is the number
                .context("parsing MemTotal")?
                .parse()
                .context("parsing MemTotal value")?;
            return Ok(kb * 1024);
        }
    }
    anyhow::bail!("MemTotal not found in node {node} meminfo")
}

/// Read the NUMA distance row for a given node.
async fn guest_node_distances(agent: &PipetteClient, node: u32) -> anyhow::Result<Vec<u32>> {
    let sh = agent.unix_shell();
    let path = format!("/sys/devices/system/node/node{node}/distance");
    let output = sh
        .read_file(&path)
        .await
        .with_context(|| format!("reading distance for node {node}"))?;
    output
        .split_whitespace()
        .map(|s| s.parse().context("parsing distance"))
        .collect()
}

fn check_acpi_header<'a>(table: &'a [u8], signature: &[u8; 4]) -> anyhow::Result<&'a [u8]> {
    anyhow::ensure!(
        table.len() >= 36,
        "truncated ACPI header: expected at least 36 bytes, actual {}",
        table.len()
    );
    anyhow::ensure!(
        &table[..4] == signature,
        "unexpected ACPI signature: expected {:?}, actual {:?}",
        String::from_utf8_lossy(signature),
        String::from_utf8_lossy(&table[..4])
    );
    let length = u32::from_le_bytes(table[4..8].try_into()?) as usize;
    anyhow::ensure!(
        length == table.len(),
        "incorrect ACPI length: declared {length} bytes, actual {}",
        table.len()
    );
    anyhow::ensure!(
        table.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) == 0,
        "invalid ACPI checksum"
    );
    Ok(&table[36..])
}

petri::test_sync!(acpi_header_diagnostics, |_| Some(()));

fn acpi_header_diagnostics(_: petri::PetriTestParams<'_>, _: ()) -> anyhow::Result<()> {
    let mut table = [0u8; 36];
    table[..4].copy_from_slice(b"SLIT");
    table[4..8].copy_from_slice(&36u32.to_le_bytes());
    table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)));
    assert!(check_acpi_header(&table, b"SLIT")?.is_empty());
    for length in [0, 35] {
        assert_eq!(
            check_acpi_header(&table[..length], b"SLIT")
                .unwrap_err()
                .to_string(),
            format!("truncated ACPI header: expected at least 36 bytes, actual {length}")
        );
    }
    assert_eq!(
        check_acpi_header(&table, b"SRAT").unwrap_err().to_string(),
        "unexpected ACPI signature: expected \"SRAT\", actual \"SLIT\""
    );
    for length in [0u32, 35, 37] {
        let mut invalid = table;
        invalid[4..8].copy_from_slice(&length.to_le_bytes());
        assert_eq!(
            check_acpi_header(&invalid, b"SLIT")
                .unwrap_err()
                .to_string(),
            format!("incorrect ACPI length: declared {length} bytes, actual 36")
        );
    }
    table[9] = table[9].wrapping_add(1);
    assert_eq!(
        check_acpi_header(&table, b"SLIT").unwrap_err().to_string(),
        "invalid ACPI checksum"
    );
    Ok(())
}

/// Checks the guest's two-locality SLIT, SRAT domain and memory coverage, and
/// Linux NUMA placement and distances against the configured topology.
/// Matching bytes alone do not prove which generation path was used.
async fn check_openhcl_slit(agent: &PipetteClient) -> anyhow::Result<()> {
    let sh = agent.unix_shell();
    let slit = sh.read_file_raw("/sys/firmware/acpi/tables/SLIT").await?;
    let body = check_acpi_header(&slit, b"SLIT")?;
    anyhow::ensure!(
        &slit[10..16] == b"HVLITE" && &slit[16..24] == b"HVLITETB" && &slit[28..32] == b"MSHV",
        "SLIT does not have the generated ACPI identity"
    );
    anyhow::ensure!(slit[8] == 1 && body.len() == 12, "unexpected SLIT format");
    anyhow::ensure!(
        u64::from_le_bytes(body[..8].try_into()?) == 2,
        "expected two SLIT localities"
    );
    anyhow::ensure!(body[8..] == [10, 17, 29, 10], "SLIT distances changed");

    let srat = sh.read_file_raw("/sys/firmware/acpi/tables/SRAT").await?;
    let body = check_acpi_header(&srat, b"SRAT")?;
    anyhow::ensure!(body.len() >= 12, "truncated SRAT header");
    let mut entries = &body[12..];
    let mut cpu_domains = std::collections::BTreeSet::new();
    let mut memory_domains = std::collections::BTreeSet::new();
    let mut memory_bytes = [0u64; 2];
    while !entries.is_empty() {
        anyhow::ensure!(entries.len() >= 2, "truncated SRAT record");
        let length = entries[1] as usize;
        anyhow::ensure!(
            (2..=entries.len()).contains(&length),
            "invalid SRAT record length"
        );
        let record = &entries[..length];
        let (domain, enabled, cpu) = match record[0] {
            0 => {
                anyhow::ensure!(length == 16, "invalid SRAT APIC record");
                (
                    u32::from_le_bytes([record[2], record[9], record[10], record[11]]),
                    u32::from_le_bytes(record[4..8].try_into()?) & 1 != 0,
                    true,
                )
            }
            1 => {
                anyhow::ensure!(length == 40, "invalid SRAT memory record");
                (
                    u32::from_le_bytes(record[2..6].try_into()?),
                    u32::from_le_bytes(record[28..32].try_into()?) & 1 != 0,
                    false,
                )
            }
            2 => {
                anyhow::ensure!(length == 24, "invalid SRAT x2APIC record");
                (
                    u32::from_le_bytes(record[4..8].try_into()?),
                    u32::from_le_bytes(record[12..16].try_into()?) & 1 != 0,
                    true,
                )
            }
            other => anyhow::bail!("unexpected SRAT record {other}"),
        };
        if enabled {
            anyhow::ensure!(domain < 2, "SRAT domain exceeds SLIT");
            if cpu {
                cpu_domains.insert(domain);
            } else {
                memory_domains.insert(domain);
                let size = u64::from_le_bytes(record[16..24].try_into()?);
                memory_bytes[domain as usize] = memory_bytes[domain as usize]
                    .checked_add(size)
                    .context("SRAT memory size overflow")?;
            }
        }
        entries = &entries[length..];
    }
    anyhow::ensure!(
        cpu_domains == [0, 1].into() && memory_domains == [0, 1].into(),
        "missing SRAT domains"
    );
    assert_eq!(guest_numa_node_count(agent).await?, 2);
    assert_eq!(guest_node_cpulist(agent, 0).await?, "0-1");
    assert_eq!(guest_node_cpulist(agent, 1).await?, "2-3");
    assert_eq!(guest_node_distances(agent, 0).await?, vec![10, 17]);
    assert_eq!(guest_node_distances(agent, 1).await?, vec![29, 10]);
    for node in 0..2 {
        let memory = guest_node_mem_bytes(agent, node).await?;
        let expected = memory_bytes[node as usize];
        anyhow::ensure!(
            expected > 0 && expected <= SIZE_2_GB,
            "unexpected SRAT memory on node {node}: {expected}"
        );
        anyhow::ensure!(
            memory > expected * 85 / 100 && memory <= expected,
            "unexpected memory on node {node}: {memory}, SRAT bytes: {expected}"
        );
    }
    Ok(())
}

/// Verify IGVM SLIT reaches Linux through both OpenHCL ACPI loading paths.
#[openvmm_test(openhcl_linux_direct_x64, openhcl_uefi_x64(vhd(alpine_3_23_x64)))]
async fn openhcl_acpi_slit(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_memory(petri::MemoryConfig {
            startup_bytes: SIZE_2_GB * 2,
            numa_mem_sizes: Some(vec![SIZE_2_GB, SIZE_2_GB]),
            ..Default::default()
        })
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 4,
            vps_per_socket: Some(2),
            enable_smt: Some(false),
            ..Default::default()
        })
        .modify_backend(|backend| {
            backend.with_custom_config(|config| {
                config.numa.distances = vec![
                    NumaDistance {
                        src: 0,
                        dst: 1,
                        distance: 17,
                    },
                    NumaDistance {
                        src: 1,
                        dst: 0,
                        distance: 29,
                    },
                ];
            })
        })
        .run()
        .await?;
    let result = check_openhcl_slit(&agent).await;
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    result
}

/// Boot a 2-node NUMA VM and verify the guest sees the correct topology.
///
/// Two nodes, 2 GB each, 4 VPs with 2 per socket. `FromTopology` assigns
/// VPs 0,1 to node 0 and VPs 2,3 to node 1. Default SLIT distances.
#[openvmm_test(linux_direct_x64, linux_direct_aarch64)]
async fn boot_numa_two_nodes(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_memory(petri::MemoryConfig {
            startup_bytes: SIZE_2_GB * 2,
            numa_mem_sizes: Some(vec![SIZE_2_GB, SIZE_2_GB]),
            ..Default::default()
        })
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 4,
            vps_per_socket: Some(2),
            ..Default::default()
        })
        .run()
        .await?;

    // Verify 2 NUMA nodes.
    assert_eq!(guest_numa_node_count(&agent).await?, 2);

    // Verify CPU assignment: node 0 has VPs 0-1, node 1 has VPs 2-3.
    assert_eq!(guest_node_cpulist(&agent, 0).await?, "0-1");
    assert_eq!(guest_node_cpulist(&agent, 1).await?, "2-3");

    // Verify each node has approximately 2 GB (allow 10% tolerance for
    // kernel reservations).
    for node in 0..2 {
        let mem = guest_node_mem_bytes(&agent, node).await?;
        assert!(
            mem > SIZE_2_GB * 85 / 100,
            "node {node} memory too low: {mem}"
        );
        assert!(mem <= SIZE_2_GB, "node {node} memory too high: {mem}");
    }

    // Verify default SLIT distances: 10 self, 20 cross-node.
    assert_eq!(guest_node_distances(&agent, 0).await?, vec![10, 20]);
    assert_eq!(guest_node_distances(&agent, 1).await?, vec![20, 10]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot a 4-node NUMA VM with asymmetric memory, a CPU-only node, a
/// memory-only node, explicit VP assignment, and custom SLIT distances.
///
/// - Node 0: 1 GB RAM, VPs [0, 1] (private memory)
/// - Node 1: 2 GB RAM, VPs [2, 3] (shared memory)
/// - Node 2: no memory, VPs [4, 5] (CPU-only)
/// - Node 3: 1 GB RAM, no VPs (memory-only, private memory)
///
/// The mix of private and shared per-node backing also exercises the
/// memory manager building heterogeneous RAM backings in one VM.
///
/// Custom distances: 10 self, 15/25/30 between select pairs.
#[openvmm_test(linux_direct_x64, linux_direct_aarch64)]
async fn boot_numa_complex_topology(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 6,
            vps_per_socket: Some(2),
            ..Default::default()
        })
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.numa = NumaTopology {
                    nodes: vec![
                        // Node 0: 1 GB, VPs 0-1 (private memory)
                        NumaNode {
                            mem: Some(make_mem(SIZE_1_GB, true)),
                            vps: VpAssignment::Explicit(vec![0, 1]),
                        },
                        // Node 1: 2 GB, VPs 2-3 (shared memory)
                        NumaNode {
                            mem: Some(make_mem(SIZE_2_GB, false)),
                            vps: VpAssignment::Explicit(vec![2, 3]),
                        },
                        // Node 2: CPU-only (no memory), VPs 4-5
                        NumaNode {
                            mem: None,
                            vps: VpAssignment::Explicit(vec![4, 5]),
                        },
                        // Node 3: memory-only (1 GB), no VPs (private memory)
                        NumaNode {
                            mem: Some(make_mem(SIZE_1_GB, true)),
                            vps: VpAssignment::Empty,
                        },
                    ],
                    distances: vec![
                        // Explicit asymmetric cross-node distances.
                        NumaDistance {
                            src: 0,
                            dst: 1,
                            distance: 15,
                        },
                        NumaDistance {
                            src: 1,
                            dst: 0,
                            distance: 15,
                        },
                        NumaDistance {
                            src: 0,
                            dst: 2,
                            distance: 25,
                        },
                        NumaDistance {
                            src: 2,
                            dst: 0,
                            distance: 25,
                        },
                        NumaDistance {
                            src: 0,
                            dst: 3,
                            distance: 30,
                        },
                        NumaDistance {
                            src: 3,
                            dst: 0,
                            distance: 30,
                        },
                        NumaDistance {
                            src: 1,
                            dst: 2,
                            distance: 20,
                        },
                        NumaDistance {
                            src: 2,
                            dst: 1,
                            distance: 20,
                        },
                        NumaDistance {
                            src: 1,
                            dst: 3,
                            distance: 25,
                        },
                        NumaDistance {
                            src: 3,
                            dst: 1,
                            distance: 25,
                        },
                        NumaDistance {
                            src: 2,
                            dst: 3,
                            distance: 15,
                        },
                        NumaDistance {
                            src: 3,
                            dst: 2,
                            distance: 15,
                        },
                    ],
                };
            })
        })
        .run()
        .await?;

    // Verify 4 NUMA nodes.
    assert_eq!(guest_numa_node_count(&agent).await?, 4);

    // Verify CPU assignment.
    assert_eq!(guest_node_cpulist(&agent, 0).await?, "0-1");
    assert_eq!(guest_node_cpulist(&agent, 1).await?, "2-3");
    assert_eq!(guest_node_cpulist(&agent, 2).await?, "4-5");
    // Node 3 has no VPs — cpulist should be empty.
    assert_eq!(guest_node_cpulist(&agent, 3).await?, "");

    // Verify memory sizes (with 10% tolerance for kernel reservations).
    let mem0 = guest_node_mem_bytes(&agent, 0).await?;
    assert!(mem0 > SIZE_1_GB * 85 / 100, "node 0 memory too low: {mem0}");
    assert!(mem0 <= SIZE_1_GB, "node 0 memory too high: {mem0}");

    let mem1 = guest_node_mem_bytes(&agent, 1).await?;
    assert!(mem1 > SIZE_2_GB * 85 / 100, "node 1 memory too low: {mem1}");
    assert!(mem1 <= SIZE_2_GB, "node 1 memory too high: {mem1}");

    // Node 2 has no memory.
    let mem2 = guest_node_mem_bytes(&agent, 2).await?;
    assert_eq!(mem2, 0, "node 2 should have no memory, got {mem2}");

    let mem3 = guest_node_mem_bytes(&agent, 3).await?;
    assert!(mem3 > SIZE_1_GB * 85 / 100, "node 3 memory too low: {mem3}");
    assert!(mem3 <= SIZE_1_GB, "node 3 memory too high: {mem3}");

    // Verify custom SLIT distances.
    assert_eq!(guest_node_distances(&agent, 0).await?, vec![10, 15, 25, 30]);
    assert_eq!(guest_node_distances(&agent, 1).await?, vec![15, 10, 20, 25]);
    assert_eq!(guest_node_distances(&agent, 2).await?, vec![25, 20, 10, 15]);
    assert_eq!(guest_node_distances(&agent, 3).await?, vec![30, 25, 15, 10]);

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// Boot a 2-node NUMA VM with a PCIe root complex on node 1 and verify
/// that the guest sees the correct NUMA affinity on the PCIe device.
///
/// Linux populates `/sys/bus/pci/devices/<BDF>/numa_node` from the ACPI
/// `_PXM` object on the host bridge.
#[openvmm_test(linux_direct_x64, linux_direct_aarch64)]
async fn pcie_device_numa_affinity(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let nvme_subsystem_id = guid::guid!("a1b2c3d4-e5f6-7890-abcd-ef0123456789");

    let (vm, agent) = config
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 4,
            vps_per_socket: Some(2),
            ..Default::default()
        })
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.numa = NumaTopology {
                    nodes: vec![
                        NumaNode {
                            mem: Some(make_mem(SIZE_2_GB, true)),
                            vps: VpAssignment::Explicit(vec![0, 1]),
                        },
                        NumaNode {
                            mem: Some(make_mem(SIZE_2_GB, true)),
                            vps: VpAssignment::Explicit(vec![2, 3]),
                        },
                    ],
                    distances: vec![],
                };

                // Add a PCIe root complex on NUMA node 1.
                c.pcie_root_complexes.push(PcieRootComplexConfig {
                    index: 0,
                    name: "rc0".to_string(),
                    segment: 0,
                    start_bus: 0,
                    end_bus: 255,
                    low_mmio: PcieMmioRangeConfig::Dynamic {
                        size: 64 * 1024 * 1024,
                    },
                    high_mmio: PcieMmioRangeConfig::Dynamic {
                        size: 1024 * 1024 * 1024,
                    },
                    cxl: None,
                    ports: vec![PciePortConfig {
                        name: "rp0".to_string(),
                        devfn: None,
                        hotplug: false,
                        acs_capabilities_supported: None,
                        cxl: false,
                        pasid: false,
                    }],
                    iommu: None,
                    vnode: Some(1),
                    preserve_bars: false,
                });

                // Attach an NVMe device to the root port.
                c.pcie_devices.push(PcieDeviceConfig {
                    port_name: "rp0".to_string(),
                    resource: nvme_resources::NvmeControllerHandle {
                        subsystem_id: nvme_subsystem_id,
                        max_io_queues: 64,
                        msix_count: 64,
                        namespaces: vec![nvme_resources::NamespaceDefinition {
                            nsid: 1,
                            disk: disk_backend_resources::LayeredDiskHandle::single_layer(
                                disk_backend_resources::layer::RamDiskLayerHandle {
                                    len: Some(1024 * 1024),
                                    sector_size: None,
                                },
                            )
                            .into_resource(),
                            read_only: false,
                        }],
                        requests: None,
                    }
                    .into_resource(),
                });
            })
        })
        .run()
        .await?;

    // Verify 2 NUMA nodes are visible.
    assert_eq!(guest_numa_node_count(&agent).await?, 2);

    // Find PCI devices and check their numa_node attribute.
    let sh = agent.unix_shell();
    let devices = cmd!(sh, "ls /sys/bus/pci/devices/").read().await?;
    let mut found_nvme = false;
    for bdf in devices.split_whitespace() {
        // Read the class to identify NVMe (class 0x010802).
        let class_path = format!("/sys/bus/pci/devices/{bdf}/class");
        let class = sh.read_file(&class_path).await.unwrap_or_default();
        let class = class.trim();
        if class == "0x010802" {
            let numa_path = format!("/sys/bus/pci/devices/{bdf}/numa_node");
            let numa_node = sh
                .read_file(&numa_path)
                .await
                .with_context(|| format!("reading numa_node for {bdf}"))?;
            let numa_node: i32 = numa_node.trim().parse()?;
            assert_eq!(
                numa_node, 1,
                "NVMe device {bdf} should be on NUMA node 1, got {numa_node}"
            );
            found_nvme = true;
        }
    }
    assert!(found_nvme, "no NVMe device found in guest PCI devices");

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}
