// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use futures::FutureExt;
use inspect::DeferredUpdate;
use inspect::InspectMut;
use inspect::InspectionBuilder;
use inspect::Node;
use inspect::Request;
use inspect::Response;
use inspect::SensitivityLevel;
use inspect::ValueKind;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

struct InspectNode<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    depth: usize,
    depth_limit: usize,
}

impl InspectMut for InspectNode<'_, '_> {
    fn inspect_mut(&mut self, req: Request<'_>) {
        fuzz_eprintln!("depth {}", self.depth);
        // Depth limit so that we don't recurse infinitely
        if self.depth >= self.depth_limit {
            return;
        }
        let _ = self.inspect_inner(req);
        fuzz_eprintln!("done {}", self.depth);
    }
}

impl InspectNode<'_, '_> {
    fn inspect_inner(&mut self, req: Request<'_>) -> Result<(), arbitrary::Error> {
        match self.u.int_in_range(0..=5)? {
            0 => {
                fuzz_eprintln!("value");
                req.value(self.u.arbitrary::<ValueKind>()?)
            }
            1 => {
                fuzz_eprintln!("ignore");
                req.ignore()
            }
            2 => {
                fuzz_eprintln!("respond");
                self.respond(&mut req.respond())?
            }
            3 => {
                fuzz_eprintln!("defer");
                let defer = req.defer();
                match self.u.int_in_range(0..=6)? {
                    0 => {
                        fuzz_eprintln!("value");
                        defer.value(self.u.arbitrary::<ValueKind>()?)
                    }
                    1 => {
                        fuzz_eprintln!("ignore");
                        defer.ignore()
                    }
                    2 => {
                        fuzz_eprintln!("inspect");
                        defer.inspect(InspectNode {
                            u: self.u,
                            depth: self.depth + 1,
                            depth_limit: self.depth_limit,
                        })
                    }
                    3 => {
                        fuzz_eprintln!("respond");
                        defer.respond(|r| {
                            let _ = self.respond(r);
                        })
                    }
                    4 => {
                        fuzz_eprintln!("complete_external");
                        let _ = defer.external_request();
                        defer.complete_external(self.u.arbitrary()?, self.u.arbitrary()?)
                    }
                    5 => {
                        fuzz_eprintln!("update");
                        if let Ok(upd) = defer.update() {
                            do_defer_update(upd);
                        }
                    }
                    6 => {
                        fuzz_eprintln!("nothing");
                        // Do nothing
                    }
                    _ => unreachable!(),
                }
            }
            4 => {
                fuzz_eprintln!("update");
                if let Ok(upd) = req.update() {
                    if self.u.arbitrary()? {
                        fuzz_eprintln!("defer");
                        do_defer_update(upd.defer())
                    } else {
                        let parse_attempt: Result<i64, _> = upd.new_value().parse();
                        match parse_attempt {
                            Ok(new_val) => {
                                fuzz_eprintln!("succeed");
                                upd.succeed(new_val);
                            }
                            Err(e) => {
                                fuzz_eprintln!("fail");
                                upd.fail(e)
                            }
                        }
                    }
                }
            }
            5 => {
                fuzz_eprintln!("nothing");
                // Do nothing
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn respond(&mut self, mut resp: &mut Response<'_>) -> Result<(), arbitrary::Error> {
        for _ in 0..self.u.int_in_range(0..=4)? {
            resp = match self.u.int_in_range(0..=11)? {
                0 => {
                    fuzz_eprintln!("nothing");
                    resp
                }
                1 => {
                    fuzz_eprintln!("hex");
                    resp.hex(self.u.arbitrary()?, ValueKind::arbitrary(self.u)?)
                }
                2 => {
                    fuzz_eprintln!("counter");
                    resp.counter(self.u.arbitrary()?, ValueKind::arbitrary(self.u)?)
                }
                3 => {
                    fuzz_eprintln!("sensitivity_counter");
                    resp.sensitivity_counter(
                        self.u.arbitrary()?,
                        self.u.arbitrary()?,
                        ValueKind::arbitrary(self.u)?,
                    )
                }
                4 => {
                    fuzz_eprintln!("binary");
                    resp.binary(self.u.arbitrary()?, ValueKind::arbitrary(self.u)?)
                }
                5 => {
                    fuzz_eprintln!("display");
                    resp.display(self.u.arbitrary()?, &String::arbitrary(self.u)?)
                }
                6 => {
                    fuzz_eprintln!("display_debug");
                    resp.display_debug(self.u.arbitrary()?, &String::arbitrary(self.u)?)
                }
                7 => {
                    fuzz_eprintln!("field_mut");
                    resp.field_mut(
                        self.u.arbitrary()?,
                        &mut InspectNode {
                            u: self.u,
                            depth: self.depth + 1,
                            depth_limit: self.depth_limit,
                        },
                    )
                }
                8 => {
                    fuzz_eprintln!("sensitivity_field_mut");
                    resp.sensitivity_field_mut(
                        self.u.arbitrary()?,
                        self.u.arbitrary()?,
                        &mut InspectNode {
                            u: self.u,
                            depth: self.depth + 1,
                            depth_limit: self.depth_limit,
                        },
                    )
                }
                9 => {
                    fuzz_eprintln!("child");
                    resp.child(self.u.arbitrary()?, |r| {
                        let _ = InspectNode {
                            u: self.u,
                            depth: self.depth + 1,
                            depth_limit: self.depth_limit,
                        }
                        .inspect_inner(r);
                    })
                }
                10 => {
                    fuzz_eprintln!("sensitivity_child");
                    resp.sensitivity_child(self.u.arbitrary()?, self.u.arbitrary()?, |r| {
                        let _ = InspectNode {
                            u: self.u,
                            depth: self.depth + 1,
                            depth_limit: self.depth_limit,
                        }
                        .inspect_inner(r);
                    })
                }
                11 => {
                    fuzz_eprintln!("merge");
                    resp.merge(&mut InspectNode {
                        u: self.u,
                        depth: self.depth + 1,
                        depth_limit: self.depth_limit,
                    })
                }
                _ => unreachable!(),
            };
        }
        Ok(())
    }
}

fn do_defer_update(upd: DeferredUpdate) {
    // i64 chosen for simplicity while still being able to fail sometimes.
    let parse_attempt: Result<i64, _> = upd.new_value().parse();
    match parse_attempt {
        Ok(new_val) => {
            fuzz_eprintln!("succeed");
            upd.succeed(new_val);
        }
        Err(e) => {
            fuzz_eprintln!("fail");
            upd.fail(e)
        }
    }
}

fn do_fuzz(mut u: Unstructured<'_>) -> Result<(), arbitrary::Error> {
    let path: String = u.arbitrary()?;
    let depth = u.arbitrary()?;
    let sensitivity = u.arbitrary()?;
    let depth_limit = u.int_in_range(0..=10)?;

    let inspection_builder = InspectionBuilder::new(&path)
        .depth(depth)
        .sensitivity(sensitivity);

    // TODO: update
    if u.arbitrary()? {
        let mut starting_node = InspectNode {
            u: &mut u,
            depth: 0,
            depth_limit,
        };
        let mut inspection = inspection_builder.inspect(&mut starting_node);
        inspection.resolve().now_or_never().unwrap();
        let results = inspection.results();
        // Can't validate depth, as complete_external can return arbitrarily deep nodes.
        // That's ok, it's a perf hint, not a disclosure concern.
        validate_results(&results, sensitivity);
    } else {
        let new_value = u.arbitrary()?;
        let mut starting_node = InspectNode {
            u: &mut u,
            depth: 0,
            depth_limit,
        };
        let update = inspection_builder.update(new_value, &mut starting_node);
        let _result = update.now_or_never().unwrap();
        // Nothing we can validate here.
        // The returned 'success' value may not be the same as the one we passed in thanks to complete_external.
    }

    Ok(())
}

fn validate_results(results: &Node, req_sens: Option<SensitivityLevel>) {
    match &results {
        Node::Unevaluated => {}
        Node::Failed(_) => {}
        Node::Value(_) => {}
        Node::Dir(entries) => {
            for e in entries {
                if let Some(sens) = req_sens {
                    assert!(e.sensitivity <= sens);
                }
                validate_results(&e.node, req_sens);
            }
        }
    }
}

fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    let _ = do_fuzz(Unstructured::new(input));
});
