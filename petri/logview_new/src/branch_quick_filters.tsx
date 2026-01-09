// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// List of branch quick filters displayed on the Runs page. Matches to the
// branch name exactly. "all" is a special value that shows all branches.
export const run_filters: string[] = [
    "all",
    "main",
    "release/1.7.2511",
    "release/2505",
];

// List of branch quick filters displayed on the Tests and TestDetails pages. Matches to the
// branch name exactly.
export const test_filters: string[] = [
    "main",
    "release/1.7.2511",
    "release/2505",
];
