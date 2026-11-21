// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The path-scoped access policy of a microVM share.
//!
//! A share's policy consists of three sets of canonical share-relative paths:
//!
//! - A *denied* path hides itself and everything below it from the guest.
//! - An *allowed* path exposes itself and everything below it again, inside a
//!   denied path.
//! - When a read-write share has *writable* paths, they are the only parts of
//!   the share that the guest can modify, and the rest of the share is
//!   read-only.
//!
//! The nearest denied or allowed path that contains a path decides whether the
//! guest can see it. The nearest one that contains a denied path must be an
//! allowed path, if any, and the nearest one that contains an allowed path
//! must be a denied path, so no path in the policy is redundant.
//!
//! A hidden directory that leads to an allowed path is *traverse-only*: the
//! guest can look it up and list the entries that lead to allowed paths, but
//! it can see nothing else in it, and it can modify neither the directory nor
//! its entries.
//!
//! The share's root may itself be a denied path, the empty path, when allowed
//! paths expose parts of it again. The root is then traverse-only, so the
//! guest sees only the allowed paths, and the ways to them.
//!
//! A Windows host also resolves a name in another case to the entry of that
//! name, unless its directory is case sensitive. There, a path that names a
//! denied or allowed path in another case is as restricted as that path too,
//! but such a name never exposes or makes writable more than its own name.

use super::profile::MicroVmProfileError;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

/// How the guest can reach a share-relative path, from the least to the most
/// restricted.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum PathVisibility {
    /// The guest can reach the path, subject to the write policy.
    Visible,
    /// The path is hidden but leads to an allowed path, so the guest can look
    /// it up as a directory and list the entries that lead to allowed paths.
    TraverseOnly,
    /// The guest cannot reach the path.
    Hidden,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rule {
    Deny,
    Allow,
}

/// How a host matches a name to the entries of a directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Names {
    /// A name matches only an entry of exactly that name.
    Exact,
    /// A name also matches an entry whose name differs only in case.
    IgnoreCase,
}

/// How the host matches names.
const HOST_NAMES: Names = if cfg!(windows) {
    Names::IgnoreCase
} else {
    Names::Exact
};

/// The validated access policy of a microVM share.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SubtreePolicy {
    denied: Vec<PathBuf>,
    allowed: Vec<PathBuf>,
    writable: Vec<PathBuf>,
}

impl SubtreePolicy {
    /// Validates how the denied, allowed, and writable paths of a share relate
    /// to one another and to its access mode. Each list must already contain
    /// unique, canonical share-relative paths.
    pub(crate) fn new(
        denied: Vec<PathBuf>,
        allowed: Vec<PathBuf>,
        writable: Vec<PathBuf>,
        read_only: bool,
    ) -> Result<Self, MicroVmProfileError> {
        let policy = Self {
            denied,
            allowed,
            writable,
        };
        for denied in &policy.denied {
            // Hiding the root makes sense only to expose allowed paths in it.
            let hidden_root = denied.as_os_str().is_empty() && policy.allowed.is_empty();
            if hidden_root || policy.enclosing_rule(denied) == Some(Rule::Deny) {
                return Err(MicroVmProfileError::InvalidDeniedPaths);
            }
        }
        for allowed in &policy.allowed {
            if policy.denied.contains(allowed) || policy.enclosing_rule(allowed) != Some(Rule::Deny)
            {
                return Err(MicroVmProfileError::InvalidAllowedPaths);
            }
        }
        if read_only && !policy.writable.is_empty() {
            return Err(MicroVmProfileError::InvalidWritablePaths);
        }
        for (index, writable) in policy.writable.iter().enumerate() {
            let overlaps = policy
                .writable
                .iter()
                .enumerate()
                .any(|(other_index, other)| other_index != index && writable.starts_with(other));
            if writable.as_os_str().is_empty()
                || overlaps
                || policy.visibility_matching(writable, Names::Exact) != PathVisibility::Visible
            {
                return Err(MicroVmProfileError::InvalidWritablePaths);
            }
        }
        Ok(policy)
    }

    /// Returns the denied paths.
    pub(crate) fn denied_paths(&self) -> &[PathBuf] {
        &self.denied
    }

    /// Returns the allowed paths.
    pub(crate) fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed
    }

    /// Returns the writable paths; none means that a read-write share is
    /// writable everywhere that the guest can see.
    pub(crate) fn writable_paths(&self) -> &[PathBuf] {
        &self.writable
    }

    /// Returns the rule of the nearest denied or allowed path that contains
    /// `path`, other than `path` itself.
    fn enclosing_rule(&self, path: &Path) -> Option<Rule> {
        self.nearest_rule(path, false, Names::Exact)
    }

    /// Returns the rule of the nearest denied or allowed path that contains
    /// `path`, including `path` itself when `inclusive`, matching names as
    /// `names` does.
    fn nearest_rule(&self, path: &Path, inclusive: bool, names: Names) -> Option<Rule> {
        self.denied
            .iter()
            .map(|rule| (rule, Rule::Deny))
            .chain(self.allowed.iter().map(|rule| (rule, Rule::Allow)))
            .filter(|(rule, _)| {
                starts_with(path, rule, names) && (inclusive || !same_path(rule, path, names))
            })
            // Only names that differ in case can make a denied and an allowed
            // path equally near, and then the denied path decides.
            .max_by_key(|(rule, kind)| (rule.components().count(), *kind == Rule::Deny))
            .map(|(_, kind)| kind)
    }

    /// Returns how the guest can reach `path` when names match as `names`
    /// does.
    fn visibility_matching(&self, path: &Path, names: Names) -> PathVisibility {
        match self.nearest_rule(path, true, names) {
            None | Some(Rule::Allow) => PathVisibility::Visible,
            Some(Rule::Deny)
                if self.allowed.iter().any(|allowed| {
                    starts_with(allowed, path, names) && !same_path(allowed, path, names)
                }) =>
            {
                PathVisibility::TraverseOnly
            }
            Some(Rule::Deny) => PathVisibility::Hidden,
        }
    }

    /// Returns how the guest can reach `path`.
    pub(crate) fn visibility(&self, path: &Path) -> PathVisibility {
        self.visibility_on(path, HOST_NAMES)
    }

    /// Returns how the guest can reach `path` on a host that matches names as
    /// `host` does: as the policy's paths decide, and when the host ignores
    /// case, as the policy's paths that `path` names in another case decide,
    /// if they restrict it more.
    fn visibility_on(&self, path: &Path, host: Names) -> PathVisibility {
        let exact = self.visibility_matching(path, Names::Exact);
        match host {
            Names::IgnoreCase if exact != PathVisibility::Hidden => {
                exact.max(self.visibility_matching(path, Names::IgnoreCase))
            }
            Names::IgnoreCase | Names::Exact => exact,
        }
    }

    /// Returns whether the guest may modify `path` in a read-write share.
    pub(crate) fn is_writable(&self, path: &Path) -> bool {
        self.is_writable_on(path, HOST_NAMES)
    }

    /// Returns whether the guest may modify `path` in a read-write share on a
    /// host that matches names as `host` does. Only the exact names of a
    /// writable path make it writable.
    fn is_writable_on(&self, path: &Path, host: Names) -> bool {
        self.visibility_on(path, host) == PathVisibility::Visible
            && (self.writable.is_empty()
                || self
                    .writable
                    .iter()
                    .any(|writable| path.starts_with(writable)))
    }

    /// Returns whether the guest may modify `path` and every path below it in
    /// a read-write share, as a rename of `path` does. Below a writable path,
    /// only a denied path hides or protects anything: an allowed path there
    /// lies inside a denied path there too, and the writable path that
    /// contains `path`, if any, contains everything below it.
    pub(crate) fn is_subtree_writable(&self, path: &Path) -> bool {
        self.is_subtree_writable_on(path, HOST_NAMES)
    }

    /// Returns whether the guest may modify `path` and every path below it in
    /// a read-write share on a host that matches names as `host` does: there,
    /// a denied path below `path` in another case protects `path` too.
    fn is_subtree_writable_on(&self, path: &Path, host: Names) -> bool {
        self.is_writable_on(path, host)
            && !self
                .denied
                .iter()
                .any(|denied| starts_with(denied, path, host))
    }

    /// Returns whether some path that the guest can reach is not writable in a
    /// read-write share, so that each modification must check its paths.
    pub(crate) fn restricts_writes(&self) -> bool {
        !self.allowed.is_empty() || !self.writable.is_empty()
    }

    /// Returns the hidden paths whose host objects are pinned when the share is
    /// attached, each with the only path at which the guest may reach the
    /// object: none for a denied path that leads to no allowed path, and its
    /// own path for a traverse-only directory. Pinning the objects keeps them
    /// hidden when the host makes them reachable through another path, such
    /// as a bind mount, a hard link, or a renamed ancestor.
    pub(crate) fn pinned_paths(&self) -> Vec<(PathBuf, Option<PathBuf>)> {
        let mut pinned = Vec::new();
        for denied in &self.denied {
            let reachable_at = match self.visibility_matching(denied, Names::Exact) {
                PathVisibility::Hidden => None,
                PathVisibility::Visible | PathVisibility::TraverseOnly => Some(denied.clone()),
            };
            pinned.push((denied.clone(), reachable_at));
        }
        for allowed in &self.allowed {
            let mut prefix = PathBuf::new();
            for component in allowed.components() {
                prefix.push(component);
                if prefix != *allowed
                    && self.visibility_matching(&prefix, Names::Exact)
                        == PathVisibility::TraverseOnly
                    && !pinned.iter().any(|(path, _)| *path == prefix)
                {
                    pinned.push((prefix.clone(), Some(prefix.clone())));
                }
            }
        }
        pinned
    }
}

/// Returns whether the names of `path` begin with those of `prefix`, matched
/// as `names` does.
fn starts_with(path: &Path, prefix: &Path, names: Names) -> bool {
    let mut components = path.components();
    prefix.components().all(|prefix| {
        components
            .next()
            .is_some_and(|component| same_name(component.as_os_str(), prefix.as_os_str(), names))
    })
}

/// Returns whether `first` and `second` have the same names, matched as
/// `names` does.
fn same_path(first: &Path, second: &Path, names: Names) -> bool {
    first.components().count() == second.components().count() && starts_with(first, second, names)
}

/// Returns whether two names match as `names` does.
///
/// Windows file systems match names in another case by upper-casing each
/// UTF-16 unit with a table of Unicode's simple case mappings. Unicode's full
/// mappings, which `str::to_uppercase` applies, make every such pair of names
/// match, and a few more, such as `ß` and `SS`, which only restricts a path
/// further (see [`SubtreePolicy::visibility_on`]).
fn same_name(first: &OsStr, second: &OsStr, names: Names) -> bool {
    first == second
        || (names == Names::IgnoreCase
            && match (first.to_str(), second.to_str()) {
                (Some(first), Some(second)) => first.to_uppercase() == second.to_uppercase(),
                _ => false,
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> PathBuf {
        value
            .split('/')
            .filter(|component| !component.is_empty())
            .collect()
    }

    fn paths(values: &[&str]) -> Vec<PathBuf> {
        values.iter().map(|value| path(value)).collect()
    }

    fn policy(denied: &[&str], allowed: &[&str], writable: &[&str]) -> SubtreePolicy {
        SubtreePolicy::new(paths(denied), paths(allowed), paths(writable), false).unwrap()
    }

    fn invalid(
        denied: &[&str],
        allowed: &[&str],
        writable: &[&str],
        read_only: bool,
    ) -> MicroVmProfileError {
        SubtreePolicy::new(paths(denied), paths(allowed), paths(writable), read_only).unwrap_err()
    }

    fn visibility(policy: &SubtreePolicy, value: &str) -> PathVisibility {
        policy.visibility(&path(value))
    }

    fn writable(policy: &SubtreePolicy, value: &str) -> bool {
        policy.is_writable(&path(value))
    }

    fn subtree_writable(policy: &SubtreePolicy, value: &str) -> bool {
        policy.is_subtree_writable(&path(value))
    }

    #[test]
    fn allowed_paths_expose_subtrees_of_denied_paths() {
        let policy = policy(&["logs"], &["logs/mcp/payloads"], &[]);
        assert_eq!(visibility(&policy, "workspace"), PathVisibility::Visible);
        assert_eq!(visibility(&policy, "logs"), PathVisibility::TraverseOnly);
        assert_eq!(
            visibility(&policy, "logs/mcp"),
            PathVisibility::TraverseOnly
        );
        assert_eq!(
            visibility(&policy, "logs/mcp/gateway"),
            PathVisibility::Hidden
        );
        assert_eq!(visibility(&policy, "logs/secret"), PathVisibility::Hidden);
        assert_eq!(
            visibility(&policy, "logs/mcp/payloads"),
            PathVisibility::Visible
        );
        assert_eq!(
            visibility(&policy, "logs/mcp/payloads/session/1"),
            PathVisibility::Visible
        );
        // A name that only shares a prefix with an allowed path stays hidden.
        assert_eq!(visibility(&policy, "logs/mc"), PathVisibility::Hidden);
        assert_eq!(
            visibility(&policy, "logs/mcp/payloads-old"),
            PathVisibility::Hidden
        );
        assert!(policy.restricts_writes());
    }

    #[test]
    fn denied_paths_nest_inside_allowed_paths() {
        let policy = policy(&["tmp", "tmp/sandbox/firewall/logs"], &["tmp/sandbox"], &[]);
        assert_eq!(visibility(&policy, "tmp/sandbox"), PathVisibility::Visible);
        assert_eq!(
            visibility(&policy, "tmp/sandbox/firewall"),
            PathVisibility::Visible
        );
        assert_eq!(
            visibility(&policy, "tmp/sandbox/firewall/logs"),
            PathVisibility::Hidden
        );
        assert_eq!(
            visibility(&policy, "tmp/sandbox/firewall/logs/access.log"),
            PathVisibility::Hidden
        );
        assert_eq!(visibility(&policy, "tmp/other"), PathVisibility::Hidden);
        assert_eq!(
            policy.pinned_paths(),
            [
                (path("tmp"), Some(path("tmp"))),
                (path("tmp/sandbox/firewall/logs"), None),
            ]
        );
    }

    #[test]
    fn writable_paths_narrow_writes_to_their_subtrees() {
        let policy = policy(&["out/secrets"], &[], &["build/output.log", "out"]);
        assert!(writable(&policy, "out"));
        assert!(writable(&policy, "out/nested/file"));
        assert!(writable(&policy, "build/output.log"));
        assert!(!writable(&policy, "build"));
        assert!(!writable(&policy, "build/output.log.tmp"));
        assert!(!writable(&policy, "outside"));
        assert!(!writable(&policy, ""));
        assert!(!writable(&policy, "out/secrets"));
        assert!(!writable(&policy, "out/secrets/token"));
        assert!(policy.restricts_writes());
    }

    #[test]
    fn traverse_only_directories_are_never_writable() {
        // Without writable paths, every visible path of a read-write share is
        // writable, but a traverse-only directory never is.
        let policy = policy(&["logs"], &["logs/payloads"], &[]);
        assert!(writable(&policy, ""));
        assert!(writable(&policy, "logs/payloads"));
        assert!(writable(&policy, "logs/payloads/file"));
        assert!(!writable(&policy, "logs"));
        assert!(!writable(&policy, "logs/secret"));
    }

    #[test]
    fn writable_paths_may_contain_allowed_and_denied_paths() {
        let narrowed = policy(&["out/logs"], &["out/logs/payloads"], &["out"]);
        assert!(writable(&narrowed, "out/file"));
        assert!(writable(&narrowed, "out/logs/payloads/file"));
        assert!(!writable(&narrowed, "out/logs"));
        assert!(!writable(&narrowed, "out/logs/secret"));

        let exemption = policy(&["logs"], &["logs/payloads"], &["logs/payloads"]);
        assert!(writable(&exemption, "logs/payloads/file"));
        assert!(!writable(&exemption, "workspace"));
    }

    #[test]
    fn subtrees_that_hold_denied_paths_are_not_writable() {
        let nested = policy(
            &["logs/secret", "tmp", "tmp/sandbox/firewall/logs"],
            &["tmp/sandbox"],
            &[],
        );
        for (value, expected) in [
            ("", false),
            ("logs", false),
            ("logs/secret", false),
            ("logs/secret/inner", false),
            ("logs/other", true),
            // Names that only share a prefix with a denied path's ancestor.
            ("log", true),
            ("logs-old", true),
            ("tmp", false),
            ("tmp/other", false),
            ("tmp/sandbox", false),
            ("tmp/sandbox/firewall", false),
            ("tmp/sandbox/firewall/other", true),
            ("tmp/sandbox/other", true),
        ] {
            assert_eq!(subtree_writable(&nested, value), expected, "{value}");
        }

        let narrowed = policy(&["out/secrets"], &[], &["build", "out"]);
        assert!(!subtree_writable(&narrowed, "out"));
        assert!(subtree_writable(&narrowed, "out/nested"));
        assert!(subtree_writable(&narrowed, "build"));
        assert!(!subtree_writable(&narrowed, "elsewhere"));
    }

    #[test]
    fn deny_only_policies_do_not_restrict_writes() {
        let policy = policy(&["secrets"], &[], &[]);
        assert!(!policy.restricts_writes());
        assert_eq!(policy.pinned_paths(), [(path("secrets"), None)]);
        assert!(!SubtreePolicy::default().restricts_writes());
    }

    #[test]
    fn traverse_only_directories_are_pinned_to_their_paths() {
        let policy = policy(&["logs", "other"], &["logs/mcp/payloads"], &[]);
        assert_eq!(
            policy.pinned_paths(),
            [
                (path("logs"), Some(path("logs"))),
                (path("other"), None),
                (path("logs/mcp"), Some(path("logs/mcp"))),
            ]
        );
    }

    #[test]
    fn a_hidden_root_is_traverse_only() {
        let hidden = policy(&[""], &["config.json", "tools/bin"], &[]);
        assert_eq!(visibility(&hidden, ""), PathVisibility::TraverseOnly);
        assert_eq!(visibility(&hidden, "config.json"), PathVisibility::Visible);
        assert_eq!(visibility(&hidden, "tools"), PathVisibility::TraverseOnly);
        assert_eq!(
            visibility(&hidden, "tools/bin/tool"),
            PathVisibility::Visible
        );
        assert_eq!(visibility(&hidden, "secret"), PathVisibility::Hidden);
        assert_eq!(
            visibility(&hidden, "config.json.bak"),
            PathVisibility::Hidden
        );
        assert_eq!(visibility(&hidden, "tools/other"), PathVisibility::Hidden);
        assert!(!writable(&hidden, ""));
        assert!(!writable(&hidden, "tools"));
        assert!(writable(&hidden, "config.json"));
        assert!(hidden.restricts_writes());
        // The root is pinned to itself.
        assert_eq!(
            hidden.pinned_paths(),
            [
                (path(""), Some(path(""))),
                (path("tools"), Some(path("tools"))),
            ]
        );

        let narrowed = policy(&[""], &["read", "write"], &["write"]);
        assert!(!writable(&narrowed, "read"));
        assert!(writable(&narrowed, "write"));
        // A denied path may nest inside an allowed path.
        policy(&["", "tools/bin/secret"], &["tools/bin"], &[]);

        // The root is hidden only to expose allowed paths, which make any
        // other denied path outside them redundant.
        assert!(matches!(
            invalid(&[""], &[], &[], false),
            MicroVmProfileError::InvalidDeniedPaths
        ));
        assert!(matches!(
            invalid(&["", "secret"], &["config.json"], &[], false),
            MicroVmProfileError::InvalidDeniedPaths
        ));
        assert!(matches!(
            invalid(&[""], &["config.json"], &["other"], false),
            MicroVmProfileError::InvalidWritablePaths
        ));
    }

    #[test]
    fn names_in_another_case_only_restrict_paths_on_hosts_that_ignore_case() {
        use PathVisibility::Hidden;
        use PathVisibility::TraverseOnly;
        use PathVisibility::Visible;

        let shared = policy(
            &["logs", "marker", "out/readonly", "Ärger"],
            &["logs/payloads", "out/readonly/pub"],
            &["out"],
        );
        for (value, exact, ignoring_case) in [
            ("marker", Hidden, Hidden),
            ("MARKER", Visible, Hidden),
            ("Marker/inside", Visible, Hidden),
            ("ärger", Visible, Hidden),
            ("LOGS", Visible, TraverseOnly),
            ("LOGS/secret", Visible, Hidden),
            ("LOGS/payloads", Visible, Visible),
            ("out/READONLY", Visible, TraverseOnly),
            ("OUT/readonly/secret", Visible, Hidden),
            // Another case never exposes an allowed path.
            ("logs/PAYLOADS", Hidden, Hidden),
            ("markers", Visible, Visible),
        ] {
            assert_eq!(
                shared.visibility_on(&path(value), Names::Exact),
                exact,
                "{value}"
            );
            assert_eq!(
                shared.visibility_on(&path(value), Names::IgnoreCase),
                ignoring_case,
                "{value}"
            );
        }
        // Another case never makes a path writable either.
        for (value, exact, ignoring_case) in [
            ("out/file", true, true),
            ("out/READONLY", true, false),
            ("OUT/file", false, false),
        ] {
            assert_eq!(
                shared.is_writable_on(&path(value), Names::Exact),
                exact,
                "{value}"
            );
            assert_eq!(
                shared.is_writable_on(&path(value), Names::IgnoreCase),
                ignoring_case,
                "{value}"
            );
        }
        // A denied path below a name in another case protects the name from a
        // rename, as below its own name.
        let below = policy(&["logs/secret"], &[], &[]);
        for (value, exact, ignoring_case) in [
            ("logs", false, false),
            ("LOGS", true, false),
            ("Logs/SECRET", true, false),
            ("LOGS/other", true, true),
            ("other", true, true),
        ] {
            assert_eq!(
                below.is_subtree_writable_on(&path(value), Names::Exact),
                exact,
                "{value}"
            );
            assert_eq!(
                below.is_subtree_writable_on(&path(value), Names::IgnoreCase),
                ignoring_case,
                "{value}"
            );
        }

        // Of a denied and an allowed path that differ only in case, the denied
        // path decides.
        let tied = policy(&["X", "x/a"], &["X/A"], &[]);
        assert_eq!(tied.visibility_on(&path("X/A"), Names::Exact), Visible);
        assert_eq!(tied.visibility_on(&path("X/A"), Names::IgnoreCase), Hidden);
    }

    #[test]
    fn redundant_or_contradictory_policies_are_rejected() {
        // A denied path directly inside another denied path is redundant.
        assert!(matches!(
            invalid(&["a", "a/b"], &[], &[], false),
            MicroVmProfileError::InvalidDeniedPaths
        ));
        // An allowed path must be inside a denied path, and not directly
        // inside another allowed path.
        for (denied, allowed) in [
            (vec![], vec!["a"]),
            (vec!["b"], vec!["a"]),
            (vec!["a"], vec!["a"]),
            (vec!["a"], vec!["a/b", "a/b/c"]),
        ] {
            assert!(matches!(
                invalid(&denied, &allowed, &[], false),
                MicroVmProfileError::InvalidAllowedPaths
            ));
        }
        // Writable paths need a read-write share, must not overlap, and must
        // be visible.
        for (denied, allowed, writable, read_only) in [
            (vec![], vec![], vec!["a"], true),
            (vec![], vec![], vec!["a", "a/b"], false),
            (vec!["a"], vec![], vec!["a/b"], false),
            (vec!["a"], vec![], vec!["a"], false),
            (vec!["a"], vec!["a/b/c"], vec!["a/b"], false),
        ] {
            assert!(matches!(
                invalid(&denied, &allowed, &writable, read_only),
                MicroVmProfileError::InvalidWritablePaths
            ));
        }
        // The share root is a denied path only with allowed paths, and never a
        // writable path.
        assert!(SubtreePolicy::new(vec![PathBuf::new()], Vec::new(), Vec::new(), false).is_err());
        assert!(SubtreePolicy::new(Vec::new(), Vec::new(), vec![PathBuf::new()], false).is_err());
    }
}
