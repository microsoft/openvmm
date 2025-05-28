// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Common functions for the Petri Log Viewer.

/// Replace `__` with `::` in test names, undoing a transformation done by the
/// test runner.
function convertTestName(name) {
    return name.replace(/__/g, "::");
}

/// Base blob storage URL for the test results.
const baseUrl = "https://openvmmghtestresults.blob.core.windows.net/results";
