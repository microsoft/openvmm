// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NUMA topology validation.

use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::VpAssignment;

/// Resolves a NUMA topology's VP assignments to a flat vp-to-vnode map.
///
/// Returns a `Vec<u32>` of length `proc_count` where `result[vp_index]` is the
/// vnode for that VP.
///
/// `FromTopology` uses `(vp_index / vps_per_socket) % num_nodes`.
/// `Explicit` uses the specified VP-to-node assignments directly.
///
/// Call [`validate_numa_topology`] first to ensure the topology is valid.
pub fn resolve_vp_to_vnode(
    topology: &NumaTopology,
    proc_count: u32,
    vps_per_socket: u32,
) -> Vec<u32> {
    let num_nodes = topology.nodes.len() as u32;
    let has_explicit = topology
        .nodes
        .iter()
        .any(|n| matches!(n.vps, VpAssignment::Explicit(_)));

    if has_explicit {
        let mut vp_to_vnode = vec![0u32; proc_count as usize];
        for (node_idx, node) in topology.nodes.iter().enumerate() {
            if let VpAssignment::Explicit(ref vps) = node.vps {
                for &vp in vps {
                    vp_to_vnode[vp as usize] = node_idx as u32;
                }
            }
        }
        vp_to_vnode
    } else {
        // FromTopology: (vp_index / vps_per_socket) % num_nodes
        (0..proc_count)
            .map(|vp| (vp / vps_per_socket) % num_nodes)
            .collect()
    }
}

/// Validates the NUMA topology configuration.
///
/// Checks:
/// - At least one node exists
/// - When any node uses `VpAssignment::Explicit`, the VP lists are disjoint,
///   complete (cover `0..proc_count`), and contain valid indices
/// - Distance entries reference valid node indices, have values ≥ 10, and
///   self-distances are exactly 10
pub fn validate_numa_topology(topology: &NumaTopology, proc_count: u32) -> anyhow::Result<()> {
    let num_nodes = topology.nodes.len();
    anyhow::ensure!(num_nodes >= 1, "NUMA topology must have at least one node");

    // Validate VP assignment: when Explicit, lists must be disjoint,
    // complete (cover all VPs), and contain valid indices.
    let has_explicit = topology
        .nodes
        .iter()
        .any(|n| matches!(n.vps, VpAssignment::Explicit(_)));
    if has_explicit {
        let mut assigned = vec![false; proc_count as usize];
        for (i, node) in topology.nodes.iter().enumerate() {
            if let VpAssignment::Explicit(ref vps) = node.vps {
                for &vp in vps {
                    anyhow::ensure!(
                        (vp as usize) < proc_count as usize,
                        "node {i}: VP index {vp} out of range (proc_count={proc_count})"
                    );
                    anyhow::ensure!(
                        !assigned[vp as usize],
                        "node {i}: VP {vp} assigned to multiple nodes"
                    );
                    assigned[vp as usize] = true;
                }
            }
        }
        for (vp, is_assigned) in assigned.iter().enumerate() {
            anyhow::ensure!(*is_assigned, "VP {vp} not assigned to any NUMA node");
        }
    }

    // Validate NUMA distances.
    for d in &topology.distances {
        anyhow::ensure!(
            (d.src as usize) < num_nodes,
            "NUMA distance src node {} out of range (num_nodes={num_nodes})",
            d.src
        );
        anyhow::ensure!(
            (d.dst as usize) < num_nodes,
            "NUMA distance dst node {} out of range (num_nodes={num_nodes})",
            d.dst
        );
        anyhow::ensure!(
            d.distance >= 10,
            "NUMA distance {}->{} value {} is below minimum 10",
            d.src,
            d.dst,
            d.distance
        );
        if d.src == d.dst {
            anyhow::ensure!(
                d.distance == 10,
                "NUMA self-distance for node {} must be 10, got {}",
                d.src,
                d.distance
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openvmm_defs::config::MemoryConfig;
    use openvmm_defs::config::NumaDistance;
    use openvmm_defs::config::NumaNode;

    fn mem(size: u64) -> Option<MemoryConfig> {
        Some(MemoryConfig {
            mem_size: size,
            prefetch_memory: false,
            private_memory: false,
            transparent_hugepages: false,
            hugepages: false,
            hugepage_size: None,
            host_numa_node: None,
        })
    }

    fn single_node() -> NumaTopology {
        NumaTopology {
            nodes: vec![NumaNode {
                mem: mem(1024 * 1024 * 1024),
                vps: VpAssignment::FromTopology,
            }],
            distances: Vec::new(),
        }
    }

    #[test]
    fn valid_single_node() {
        validate_numa_topology(&single_node(), 4).unwrap();
    }

    #[test]
    fn valid_two_nodes_from_topology() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
            ],
            distances: vec![
                NumaDistance {
                    src: 0,
                    dst: 1,
                    distance: 20,
                },
                NumaDistance {
                    src: 1,
                    dst: 0,
                    distance: 20,
                },
            ],
        };
        validate_numa_topology(&topo, 4).unwrap();
    }

    #[test]
    fn valid_explicit_vps() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![0, 1]),
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![2, 3]),
                },
            ],
            distances: Vec::new(),
        };
        validate_numa_topology(&topo, 4).unwrap();
    }

    #[test]
    fn empty_nodes_rejected() {
        let topo = NumaTopology {
            nodes: Vec::new(),
            distances: Vec::new(),
        };
        assert!(validate_numa_topology(&topo, 4).is_err());
    }

    #[test]
    fn duplicate_vp_rejected() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![0, 1]),
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![1, 2, 3]),
                },
            ],
            distances: Vec::new(),
        };
        let err = validate_numa_topology(&topo, 4).unwrap_err();
        assert!(err.to_string().contains("VP 1"), "{err}");
    }

    #[test]
    fn missing_vp_rejected() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![0, 1]),
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![3]),
                },
            ],
            distances: Vec::new(),
        };
        let err = validate_numa_topology(&topo, 4).unwrap_err();
        assert!(err.to_string().contains("VP 2"), "{err}");
    }

    #[test]
    fn vp_out_of_range_rejected() {
        let topo = NumaTopology {
            nodes: vec![NumaNode {
                mem: mem(1024 * 1024 * 1024),
                vps: VpAssignment::Explicit(vec![0, 1, 2, 99]),
            }],
            distances: Vec::new(),
        };
        let err = validate_numa_topology(&topo, 4).unwrap_err();
        assert!(err.to_string().contains("99"), "{err}");
    }

    #[test]
    fn distance_invalid_node_rejected() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 5,
            distance: 20,
        });
        assert!(validate_numa_topology(&topo, 4).is_err());
    }

    #[test]
    fn distance_below_minimum_rejected() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 0,
            distance: 5,
        });
        assert!(validate_numa_topology(&topo, 4).is_err());
    }

    #[test]
    fn self_distance_must_be_10() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 0,
            distance: 15,
        });
        let err = validate_numa_topology(&topo, 4).unwrap_err();
        assert!(err.to_string().contains("must be 10"), "{err}");
    }

    #[test]
    fn self_distance_10_accepted() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 0,
            distance: 10,
        });
        validate_numa_topology(&topo, 4).unwrap();
    }

    #[test]
    fn resolve_single_node_from_topology() {
        let topo = single_node();
        let map = resolve_vp_to_vnode(&topo, 4, 2);
        // All VPs in one node: (vp / 2) % 1 == 0 for all.
        assert_eq!(map, vec![0, 0, 0, 0]);
    }

    #[test]
    fn resolve_two_nodes_from_topology() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
            ],
            distances: Vec::new(),
        };
        // vps_per_socket=2: vp0,1 -> socket 0 -> node 0; vp2,3 -> socket 1 -> node 1
        let map = resolve_vp_to_vnode(&topo, 4, 2);
        assert_eq!(map, vec![0, 0, 1, 1]);
    }

    #[test]
    fn resolve_from_topology_round_robin() {
        // 3 nodes, vps_per_socket=1: each VP is its own socket, round-robin.
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
            ],
            distances: Vec::new(),
        };
        let map = resolve_vp_to_vnode(&topo, 6, 1);
        // (vp / 1) % 3: 0,1,2,0,1,2
        assert_eq!(map, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn resolve_explicit_vps() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![0, 3]),
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![1, 2]),
                },
            ],
            distances: Vec::new(),
        };
        let map = resolve_vp_to_vnode(&topo, 4, 2);
        assert_eq!(map, vec![0, 1, 1, 0]);
    }
}
