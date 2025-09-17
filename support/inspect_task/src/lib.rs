// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Logic for inspecting the task list.

#![forbid(unsafe_code)]

use inspect::Inspect;
use pal_async::task::TaskData;
use pal_async::task::TaskList;

struct Wrap(Vec<TaskData>);

impl Inspect for Wrap {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        for task in &self.0 {
            resp.child(&task.id().to_string(), |req| {
                req.respond()
                    .field("name", task.name())
                    .field("executor", task.executor())
                    .display("state", &task.state())
                    .field(
                        "location",
                        format!("{}:{}", task.location().file(), task.location().line()),
                    );
            });
        }
    }
}

/// Takes a snapshot of the active tasks and returns them in an inspectable
/// format.
pub fn inspect_task_list() -> impl Inspect {
    Wrap(TaskList::global().tasks())
}
