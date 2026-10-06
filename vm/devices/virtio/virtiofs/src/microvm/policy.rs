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

use super::profile::MicroVmProfileError;
use std::path::Path;
use std::path::PathBuf;

/// How the guest can reach a share-relative path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
            if denied.as_os_str().is_empty() || policy.enclosing_rule(denied) == Some(Rule::Deny) {
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
                || policy.visibility(writable) != PathVisibility::Visible
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
        self.nearest_rule(path, false)
    }

    /// Returns the rule of the nearest denied or allowed path that contains
    /// `path`, including `path` itself when `inclusive`.
    fn nearest_rule(&self, path: &Path, inclusive: bool) -> Option<Rule> {
        self.denied
            .iter()
            .map(|rule| (rule, Rule::Deny))
            .chain(self.allowed.iter().map(|rule| (rule, Rule::Allow)))
            .filter(|(rule, _)| path.starts_with(rule) && (inclusive || rule.as_path() != path))
            .max_by_key(|(rule, _)| rule.components().count())
            .map(|(_, kind)| kind)
    }

    /// Returns how the guest can reach `path`.
    pub(crate) fn visibility(&self, path: &Path) -> PathVisibility {
        match self.nearest_rule(path, true) {
            None | Some(Rule::Allow) => PathVisibility::Visible,
            Some(Rule::Deny)
                if self
                    .allowed
                    .iter()
                    .any(|allowed| allowed.as_path() != path && allowed.starts_with(path)) =>
            {
                PathVisibility::TraverseOnly
            }
            Some(Rule::Deny) => PathVisibility::Hidden,
        }
    }

    /// Returns whether the guest may modify `path` in a read-write share.
    pub(crate) fn is_writable(&self, path: &Path) -> bool {
        self.visibility(path) == PathVisibility::Visible
            && (self.writable.is_empty()
                || self
                    .writable
                    .iter()
                    .any(|writable| path.starts_with(writable)))
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
            let reachable_at = match self.visibility(denied) {
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
                    && self.visibility(&prefix) == PathVisibility::TraverseOnly
                    && !pinned.iter().any(|(path, _)| *path == prefix)
                {
                    pinned.push((prefix.clone(), Some(prefix.clone())));
                }
            }
        }
        pinned
    }
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
        // The share root itself is not a policy path.
        assert!(SubtreePolicy::new(vec![PathBuf::new()], Vec::new(), Vec::new(), false).is_err());
        assert!(SubtreePolicy::new(Vec::new(), Vec::new(), vec![PathBuf::new()], false).is_err());
    }
}
