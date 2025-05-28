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

/// Creates a node with the specified tag, attributes, and content.
function node(tag, attrs = {}, ...content) {
    const element = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs)) {
        if (key === 'class') {
            element.className = value;
        } else if (typeof value === 'object' && value.constructor === Object) {
            Object.assign(element[key], value);
        } else {
            element.setAttribute(key, value);
        }
    }
    element.append(...content);
    return element;
}
