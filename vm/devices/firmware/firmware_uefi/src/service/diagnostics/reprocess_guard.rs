// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reprocessing guard to prevent guest from spamming diagnostics processing

use inspect::Inspect;

/// Controls whether diagnostics processing is allowed.
///
/// This guard prevents guests from repeatedly requesting diagnostics processing,
/// which could be used to spam logs or waste resources.
#[derive(Debug, Default, Inspect)]
pub struct ReprocessGuard {
    /// Whether guest-initiated processing has occurred
    processed: bool,
}

/// The result of checking whether processing is allowed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingPermission {
    /// Processing is allowed and this is the first time (guest-initiated)
    AllowedFirstTime,
    /// Processing is allowed as a retry (host-initiated or debugging)
    AllowedRetry,
    /// Processing was already done and should be skipped
    Denied,
}

impl ReprocessGuard {
    /// Create a new reprocess guard
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if processing should be allowed
    ///
    /// # Arguments
    /// * `allow_reprocess` - If true, bypass the guard (for host-initiated processing)
    ///
    /// # Returns
    /// Permission indicating whether and how processing should proceed
    pub fn check_permission(&self, allow_reprocess: bool) -> ProcessingPermission {
        if allow_reprocess {
            // Host explicitly allows reprocessing (e.g., inspect command)
            ProcessingPermission::AllowedRetry
        } else if !self.processed {
            // First time processing
            ProcessingPermission::AllowedFirstTime
        } else {
            // Already processed, guest trying to spam
            ProcessingPermission::Denied
        }
    }

    /// Mark that processing has completed successfully.
    ///
    /// Should only be called after processing succeeds to ensure
    /// failed attempts can be retried.
    pub fn mark_processed(&mut self) {
        self.processed = true;
    }

    /// Reset the guard (e.g., on VM reset)
    pub fn reset(&mut self) {
        self.processed = false;
    }

    /// Check if processing has occurred before
    pub fn has_processed(&self) -> bool {
        self.processed
    }
}

impl ProcessingPermission {
    /// Check if processing is allowed
    pub fn is_allowed(self) -> bool {
        matches!(
            self,
            ProcessingPermission::AllowedFirstTime | ProcessingPermission::AllowedRetry
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_time_processing() {
        let guard = ReprocessGuard::new();
        let permission = guard.check_permission(false);
        assert_eq!(permission, ProcessingPermission::AllowedFirstTime);
        assert!(permission.is_allowed());
    }

    #[test]
    fn test_deny_reprocessing() {
        let mut guard = ReprocessGuard::new();
        guard.mark_processed();
        let permission = guard.check_permission(false);
        assert_eq!(permission, ProcessingPermission::Denied);
        assert!(!permission.is_allowed());
    }

    #[test]
    fn test_allow_explicit_reprocessing() {
        let mut guard = ReprocessGuard::new();
        guard.mark_processed();
        let permission = guard.check_permission(true);
        assert_eq!(permission, ProcessingPermission::AllowedRetry);
        assert!(permission.is_allowed());
    }

    #[test]
    fn test_mark_processed_only_after_success() {
        let mut guard = ReprocessGuard::new();
        
        // First attempt - allowed
        assert!(guard.check_permission(false).is_allowed());
        
        // Don't mark as processed yet (simulating failure)
        // Second attempt - still allowed since we didn't mark
        assert!(guard.check_permission(false).is_allowed());
        
        // Now mark as processed (success)
        guard.mark_processed();
        
        // Third attempt - denied
        assert!(!guard.check_permission(false).is_allowed());
    }

    #[test]
    fn test_reset() {
        let mut guard = ReprocessGuard::new();
        guard.mark_processed();
        assert!(!guard.check_permission(false).is_allowed());
        
        guard.reset();
        assert!(guard.check_permission(false).is_allowed());
    }
}
