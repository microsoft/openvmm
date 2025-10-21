// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generally we hold the position that inspect paths are completely unstable,
//! may change at any time, and can not be depended on by anything. However, we
//! at Microsoft have implemented stable, versioned, supported, internal
//! diagnostic tools that depend on inspect paths under the hood. This would be
//! fine so long as we are guaranteed that our diagnostic tooling and OpenHCL
//! builds are always in sync, and indeed they are most of the time. However
//! there are cases where they may diverge temporarily, such as during servicing.
//! We still want to be able to provide as much information as possible during
//! these time periods, but in order for that to be possible we need some kind
//! of stable-ish interface to talk to OpenHCL with.
//!
//! This module provides that interface by creating known, controlled inspect
//! paths that are unlikely to change accidentally. The implementation details
//! of these paths are free to change in order to preserve their interface, and
//! all diagnostics commands should have extensive tests to ensure compatibility
//! with a matching build of OpenHCL.
//!
//! This approach is not particularly scalable, especially if other parties want
//! to add their own stabilized diagnostics and versioning schemes. If such a
//! need arises we should consider a more general and extensible solution.
//!
//! At the time of writing, our support policy is that these interfaces will be
//! preserved for at least 2 internal releases. This means that diagnostic
//! commands may fail in any way they see fit if the version of OpenHCL in the
//! VM is more than 2 releases old.
//!
//! In the future we may choose a different interface for these commands,
//! and this code may be deleted, however such a change will still follow the
//! above support policy.

use inspect::Deferred;
use inspect::InspectionBuilder;
use inspect::Node;
use inspect::Request;
use inspect::Response;
use inspect::SensitivityLevel;
use mesh::Sender;
use pal_async::DefaultDriver;
use pal_async::task::Spawn;

pub(crate) fn inspect_internal_diagnostics(
    req: Request<'_>,
    reinspect: &Sender<Deferred>,
    driver: &DefaultDriver,
) {
    req.respond()
        .sensitivity_field("build_info", SensitivityLevel::Safe, build_info::get())
        .sensitivity_child("net", SensitivityLevel::Safe, |req| {
            net(req, reinspect.clone(), driver)
        });
}

fn net(req: Request<'_>, reinspect: Sender<Deferred>, driver: &DefaultDriver) {
    let defer = req.defer();
    let driver2 = driver.clone();
    driver
        .spawn("inspect-diagnostics-net", async move {
            // Note the use of Sensitive here so we can inspect under the VM node,
            // which isn't Safe. The data we produce will still use the underlying
            // sensitivity of the data nodes, so nothing will be improperly exposed.
            let mut vm_inspection = InspectionBuilder::new("vm")
                .depth(Some(0))
                .sensitivity(Some(SensitivityLevel::Sensitive))
                .inspect(inspect::send(&reinspect, |x| x));
            vm_inspection.resolve().await;

            let Node::Dir(nodes) = vm_inspection.results() else {
                return defer.value("Error: No VM node.");
            };

            defer.respond(|resp| {
                for nic_entry in nodes
                    .into_iter()
                    .filter(|entry| entry.name.starts_with("net:f8615163-"))
                {
                    // Inspect node names for MANA nics are in the format:
                    // net:f8615163-0000-1000-2000-<mac address>
                    // So the mac address string starts at index 28
                    let mac_name = nic_entry.name[28..].to_owned();

                    // The existence of a mac address is always known to the host, so this can always be Safe.
                    resp.sensitivity_child(&mac_name, SensitivityLevel::Safe, |req| {
                        net_nic(req, nic_entry.name, reinspect.clone(), &driver2);
                    });
                }
            })
        })
        .detach();
}

// net/mac_address
// Format for mac address is no separators, lowercase letters, e.g. 00155d121212.
fn net_nic(req: Request<'_>, name: String, reinspect: Sender<Deferred>, driver: &DefaultDriver) {
    let defer = req.defer();
    driver
        .spawn("inspect-diagnostics-net-nic", async move {
            // Note the use of Sensitive here so we can inspect under the VM node,
            // which isn't Safe. The data we produce will still use the underlying
            // sensitivity of the data nodes, so nothing will be improperly exposed.
            let mut vm_inspection = InspectionBuilder::new(&format!("vm/{name}"))
                .depth(Some(5))
                .sensitivity(Some(SensitivityLevel::Sensitive))
                .inspect(inspect::send(&reinspect, |req| req));
            vm_inspection.resolve().await;

            if let Node::Dir(nodes) = vm_inspection.results() {
                defer.respond(|resp| {
                    for entry in nodes {
                        let sensitivity = entry.sensitivity;
                        if [
                            "endpoint",
                            "ndis_config",
                            "offload_support",
                            "primary_channel_state",
                        ]
                        .contains(&&*entry.name)
                        {
                            flatten_with_prefix(resp, &entry.name, entry.node, sensitivity, &[]);
                        } else if entry.name == "queues" {
                            let Node::Dir(queues) = entry.node else {
                                continue;
                            };
                            resp.sensitivity_child("queues", sensitivity, |req| {
                                let mut resp = req.respond();
                                for queue_entry in queues {
                                    let queue_sensitivity = queue_entry.sensitivity;
                                    resp.sensitivity_child(
                                        &queue_entry.name,
                                        queue_sensitivity,
                                        |req| {
                                            flatten_with_prefix(
                                                &mut req.respond(),
                                                "",
                                                queue_entry.node,
                                                queue_sensitivity,
                                                &["ring"],
                                            );
                                        },
                                    );
                                }
                            });
                        }
                    }
                })
            } else {
                defer.value(format!("Unexpected node when looking for NIC {name}."));
            }
        })
        .detach();
}

fn flatten_with_prefix(
    resp: &mut Response<'_>,
    prefix: &str,
    node: Node,
    sensitivity: SensitivityLevel,
    ignore_list: &[&str],
) {
    match node {
        Node::Dir(d) => {
            for entry in d {
                if ignore_list.contains(&&*entry.name) {
                    continue;
                }
                let next_prefix = if !prefix.is_empty() {
                    format!("{}_{}", prefix, entry.name)
                } else {
                    entry.name
                };
                // Since we're traversing multiple nodes and emitting only one,
                // emit the final node as the highest sensitivity of all the
                // nodes we traversed, to be safe.
                flatten_with_prefix(
                    resp,
                    &next_prefix,
                    entry.node,
                    sensitivity.max(entry.sensitivity),
                    ignore_list,
                );
            }
        }
        Node::Value(v) => {
            resp.sensitivity_field(prefix, sensitivity, v);
        }
        Node::Failed(_) | Node::Unevaluated => {}
    }
}
