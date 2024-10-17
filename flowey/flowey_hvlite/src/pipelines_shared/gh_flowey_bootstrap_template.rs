// Copyright (C) Microsoft Corporation. All rights reserved.

//! See [`get_template`]

/// Get our internal flowey bootstrap template.
///
/// See [`Pipeline::gh_set_flowey_bootstrap_template`]
///
/// [`Pipeline::gh_set_flowey_bootstrap_template`]:
///     flowey::pipeline::prelude::Pipeline::gh_set_flowey_bootstrap_template
pub fn get_template() -> String {
    // to be clear: these replaces are totally custom to this particular
    // bootstrap template. flowey knows nothing of these replacements.
    include_str!("gh_flowey_bootstrap_template.yml").to_string()
}
