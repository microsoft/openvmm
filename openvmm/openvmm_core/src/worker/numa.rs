// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NUMA topology validation and VP assignment resolution.

use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::VpAssignment;

/// Validates the NUMA topology and resolves VP-to-vnode assignments in a
/// single pass.
///
/// Returns a `Vec<u32>` of length `proc_count` where `result[vp_index]` is
/// the vnode for that VP.
///
/// Validation checks:
/// - At least one node exists
/// - All nodes use the same VP assignment mode (no mixing)
/// - When `Explicit`, VP lists are disjoint, complete, and in range
/// - Distance entries reference valid nodes, have values >= 10, and
///   self-distances are exactly 10
///
/// `FromTopology` assigns VPs by `(vp_index / vps_per_socket) % num_nodes`.
/// `Explicit` uses the specified VP-to-node assignments directly.
pub fn resolve_numa_vp_assignment(
    topology: &NumaTopology,
    proc_count: u32,
    vps_per_socket: u32,
) -> anyhow::Result<Vec<u32>> {
    let num_nodes = topology.nodes.len();
    anyhow::ensure!(num_nodes >= 1, "NUMA topology must have at least one node");

    // Classify nodes and build the vp-to-vnode map in one pass.
    let mut explicit_count = 0usize;
    let mut vp_to_vnode = vec![0u32; proc_count as usize];
    let mut assigned = vec![false; proc_count as usize];

    for (i, node) in topology.nodes.iter().enumerate() {
        match &node.vps {
            VpAssignment::Explicit(vps) => {
                explicit_count += 1;
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
                    vp_to_vnode[vp as usize] = i as u32;
                }
            }
            VpAssignment::FromTopology => {}
        }
    }

    if explicit_count > 0 {
        // All nodes must be explicit — no mixing.
        anyhow::ensure!(
            explicit_count == num_nodes,
            "cannot mix Explicit and FromTopology VP assignments; \
             all nodes must use the same mode"
        );
        // Every VP must be assigned.
        for (vp, &is_assigned) in assigned.iter().enumerate() {
            anyhow::ensure!(is_assigned, "VP {vp} not assigned to any NUMA node");
        }
    } else {
        // All FromTopology: assign by socket round-robin.
        let num_nodes = num_nodes as u32;
        for vp in 0..proc_count {
            vp_to_vnode[vp as usize] = (vp / vps_per_socket) % num_nodes;
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

    Ok(vp_to_vnode)
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

    /// Helper: validate-only (uses vps_per_socket=1 as default).
    fn validate(topo: &NumaTopology, proc_count: u32) -> anyhow::Result<Vec<u32>> {
        resolve_numa_vp_assignment(topo, proc_count, 1)
    }

    #[test]
    fn valid_single_node() {
        validate(&single_node(), 4).unwrap();
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
        validate(&topo, 4).unwrap();
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
        validate(&topo, 4).unwrap();
    }

    #[test]
    fn empty_nodes_rejected() {
        let topo = NumaTopology {
            nodes: Vec::new(),
            distances: Vec::new(),
        };
        assert!(validate(&topo, 4).is_err());
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
        let err = validate(&topo, 4).unwrap_err();
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
        let err = validate(&topo, 4).unwrap_err();
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
        let err = validate(&topo, 4).unwrap_err();
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
        assert!(validate(&topo, 4).is_err());
    }

    #[test]
    fn distance_below_minimum_rejected() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 0,
            distance: 5,
        });
        assert!(validate(&topo, 4).is_err());
    }

    #[test]
    fn self_distance_must_be_10() {
        let mut topo = single_node();
        topo.distances.push(NumaDistance {
            src: 0,
            dst: 0,
            distance: 15,
        });
        let err = validate(&topo, 4).unwrap_err();
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
        validate(&topo, 4).unwrap();
    }

    #[test]
    fn resolve_single_node_from_topology() {
        let topo = single_node();
        let map = resolve_numa_vp_assignment(&topo, 4, 2).unwrap();
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
        let map = resolve_numa_vp_assignment(&topo, 4, 2).unwrap();
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
        let map = resolve_numa_vp_assignment(&topo, 6, 1).unwrap();
        // (vp / 1) % 3: 0,1,2,0,1,2
        assert_eq!(map, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn mixed_assignment_modes_rejected() {
        let topo = NumaTopology {
            nodes: vec![
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::FromTopology,
                },
                NumaNode {
                    mem: mem(1024 * 1024 * 1024),
                    vps: VpAssignment::Explicit(vec![2, 3]),
                },
            ],
            distances: Vec::new(),
        };
        let err = validate(&topo, 4).unwrap_err();
        assert!(err.to_string().contains("cannot mix"), "{err}");
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
        let map = resolve_numa_vp_assignment(&topo, 4, 2).unwrap();
        assert_eq!(map, vec![0, 1, 1, 0]);
    }
}
