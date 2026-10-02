// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Suppression of known noisy EFI diagnostics messages.

use crate::service::diagnostics::LogLevel;
use crate::service::diagnostics::log::Log;
use std::collections::BTreeMap;

enum SuppressionMatch {
    Contains,
    Exact,
    SingleLinePrefix,
    UnsupportedImage,
}

// Temporary noise suppression pending firmware investigation and fixes.
// Keep new matches narrow so other statuses and failures remain visible.
// TODO: Fix UEFI to resolve these errors/warnings
const SUPPRESS_LOGS: &[(&str, SuppressionMatch)] = &[
    (
        "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
        SuppressionMatch::Contains,
    ),
    (
        "that are linking different HashInstanceLib instances!",
        SuppressionMatch::Contains,
    ),
    (
        "ConvertPages: failed to find range",
        SuppressionMatch::Contains,
    ),
    (
        "ConvertPages: Incompatible memory types",
        SuppressionMatch::Contains,
    ),
    ("ConvertPages: range", SuppressionMatch::Contains),
    (
        "InstallPermanentMemoryBuffer: - New Info=",
        SuppressionMatch::SingleLinePrefix,
    ),
    (
        "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 0",
        SuppressionMatch::Exact,
    ),
    (
        "FPDT: WARNING: SEC Performance Data Hob not found, ResetEnd will be set to 0!",
        SuppressionMatch::Exact,
    ),
    (
        "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Not Found",
        SuppressionMatch::Exact,
    ),
    (
        "Error: Image at <address> start failed: Unsupported",
        SuppressionMatch::UnsupportedImage,
    ),
    (
        "MnpStart: MnpStartSnp failed, Already started.",
        SuppressionMatch::Exact,
    ),
    (
        "WARN [DE]: Failed to locate on-screen keyboard protocol (Not Found).",
        SuppressionMatch::Exact,
    ),
    (
        "SecurityLock::LockType: SOFTWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Lock Capsule Interface",
        SuppressionMatch::Exact,
    ),
    ("[Bds] Unable to boot!", SuppressionMatch::Exact),
    ("Boot order is empty", SuppressionMatch::Exact),
    (
        "VmbusRootIsChannelAllowed: Channel not allowed during boot",
        SuppressionMatch::SingleLinePrefix,
    ),
];

fn suppression_pattern(message: &str) -> Option<&'static str> {
    for &(pattern, ref match_kind) in SUPPRESS_LOGS {
        let matches = match match_kind {
            SuppressionMatch::Contains => message.contains(pattern),
            SuppressionMatch::Exact => message == pattern,
            SuppressionMatch::SingleLinePrefix => {
                message.starts_with(pattern) && !message.contains(['\r', '\n'])
            }
            SuppressionMatch::UnsupportedImage => message
                .strip_prefix("Error: Image at ")
                .and_then(|rest| rest.strip_suffix(" start failed: Unsupported"))
                .is_some_and(|address| {
                    !address.is_empty()
                        && address.len() <= 16
                        && address.bytes().all(|c| c.is_ascii_hexdigit())
                }),
        };
        if matches {
            return Some(pattern);
        }
    }
    None
}

pub(super) struct LogSuppressor {
    suppressed_logs: BTreeMap<&'static str, u32>,
}

impl LogSuppressor {
    pub(super) fn new() -> Self {
        Self {
            suppressed_logs: BTreeMap::new(),
        }
    }

    fn should_suppress(&mut self, log: &Log) -> bool {
        if let Some(pattern) = suppression_pattern(log.message_trimmed()) {
            *self.suppressed_logs.entry(pattern).or_insert(0) += 1;
            return true;
        }
        false
    }

    pub(super) fn log_summary(&self) {
        for (substring, count) in &self.suppressed_logs {
            tracelimit::warn_ratelimited!(substring, count, "suppressed logs");
        }
    }

    pub(super) fn should_emit(&mut self, log: &Log, log_level: LogLevel) -> bool {
        log_level.should_log(log.debug_level) && !self.should_suppress(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use uefi_specs::hyperv::advanced_logger::DXE_PHASE;
    use uefi_specs::hyperv::advanced_logger::RUNTIME_PHASE;
    use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
    use uefi_specs::hyperv::debug_level::DEBUG_INFO;
    use uefi_specs::hyperv::debug_level::DEBUG_WARN;

    const EXACT_MESSAGES: [&str; 8] = [
        "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 0",
        "FPDT: WARNING: SEC Performance Data Hob not found, ResetEnd will be set to 0!",
        "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Not Found",
        "MnpStart: MnpStartSnp failed, Already started.",
        "WARN [DE]: Failed to locate on-screen keyboard protocol (Not Found).",
        "SecurityLock::LockType: SOFTWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Lock Capsule Interface",
        "[Bds] Unable to boot!",
        "Boot order is empty",
    ];

    fn log(message: &str) -> Log {
        Log {
            debug_level: DEBUG_ERROR,
            time_stamp: 0,
            phase: DXE_PHASE,
            message: message.to_owned(),
        }
    }

    #[test]
    fn approved_messages_are_suppressed_with_or_without_line_endings() {
        let mut suppressor = LogSuppressor::new();
        for message in EXACT_MESSAGES.into_iter().chain([
                "InstallPermanentMemoryBuffer: - New Info=44FE000, Buffer Offset=50, Current Offset=1000, Size=4194224, Discarded=5511",
                "InstallPermanentMemoryBuffer: - New Info=123ABCD, Buffer Offset=50, Current Offset=1000, Size=4194224, Discarded=0",
                "Error: Image at 0003FC79000 start failed: Unsupported",
                "Error: Image at 0003FBD0000 start failed: Unsupported",
                "Error: Image at abcdef0123456789 start failed: Unsupported",
            ]) {
                for ending in ["", "\n", "\r\n"] {
                    assert!(
                        suppressor.should_suppress(&log(&format!("{message}{ending}"))),
                        "{message:?} with ending {ending:?}"
                    );
                }
            }
        assert_eq!(suppressor.suppressed_logs.len(), 10);
    }

    #[test]
    fn approved_vmbus_channel_warnings_are_suppressed() {
        let mut suppressor = LogSuppressor::new();
        for guid in [
            "0E0B6031-5213-4934-818B-38D90CED39DB",
            "276AACF4-AC15-426C-98DD-7521AD3F01FE",
            "3375BAF4-9E15-4B30-B765-67ACB10D607B",
            "35FA2E29-EA23-4236-96AE-3A6EBACBA440",
            "525074DC-8985-46E2-8057-A307DC18A502",
            "57164F39-9115-4E78-AB55-382F3BD5422D",
            "9527E630-D0AE-497B-ADCE-E80AB0175CAF",
            "A9A0F4E7-5A45-4D96-B827-8A841E8C03E6",
            "CFA8B69E-5B4A-4CC0-B98B-8BA1A1F3F95A",
            "F8E65716-3CB3-4A06-9A60-1889C5CCCAB5",
        ] {
            for ending in ["", "\n", "\r\n"] {
                let mut log = log(&format!(
                    "VmbusRootIsChannelAllowed: Channel not allowed during boot ({guid}).{ending}"
                ));
                log.debug_level = DEBUG_WARN;
                assert!(!suppressor.should_emit(&log, LogLevel::make_default()));
            }
        }
        assert_eq!(
            suppressor.suppressed_logs["VmbusRootIsChannelAllowed: Channel not allowed during boot"],
            30
        );
        assert_eq!(suppressor.suppressed_logs.len(), 1);
    }

    #[test]
    fn vmbus_channel_warnings_are_suppressed_regardless_of_suffix() {
        let mut suppressor = LogSuppressor::new();
        for suffix in ["", " (not-a-guid).", " ().", ": additional details"] {
            let message =
                format!("VmbusRootIsChannelAllowed: Channel not allowed during boot{suffix}");
            assert!(suppressor.should_suppress(&log(&message)), "{message}");
        }
        assert_eq!(
            suppressor.suppressed_logs["VmbusRootIsChannelAllowed: Channel not allowed during boot"],
            4
        );
    }

    #[test]
    fn approved_boot_and_capsule_messages_are_suppressed_at_observed_levels_and_phases() {
        let mut suppressor = LogSuppressor::new();
        for (message, debug_level, phase) in [
            (
                "SecurityLock::LockType: SOFTWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Lock Capsule Interface",
                DEBUG_ERROR,
                RUNTIME_PHASE,
            ),
            ("[Bds] Unable to boot!", DEBUG_ERROR, DXE_PHASE),
            ("Boot order is empty", DEBUG_WARN, DXE_PHASE),
        ] {
            let mut log = log(message);
            log.debug_level = debug_level;
            log.phase = phase;
            assert!(!suppressor.should_emit(&log, LogLevel::make_default()));
            assert_eq!(suppressor.suppressed_logs[message], 1);
        }
    }

    #[test]
    fn similar_messages_and_other_statuses_remain_visible() {
        let mut suppressor = LogSuppressor::new();
        for message in [
            "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 1",
            "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 01",
            "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Device Error",
            "MnpStart: MnpStartSnp failed, Device Error.",
            "WARN [DE]: Failed to locate on-screen keyboard protocol (Device Error).",
            "InstallPermanentMemoryBuffer: allocation failed",
            "InstallPermanentMemoryBuffer: - New Info=123\nUnexpected failure",
            "Error: Image at 0003FC79000 start failed: Security Violation",
            "Error: Image at 0003FC79000 start failed: Unsupported operation",
            "Error: Image at  start failed: Unsupported",
            "Error: Image at not-an-address start failed: Unsupported",
            "Error: Image at 12345678901234567 start failed: Unsupported",
            "Error: Image at 123\n456 start failed: Unsupported",
            "Error: Image at 0003FC79000 start failed: Unsupported\nUnexpected failure",
            "SecurityLock::LockType: HARDWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Lock Capsule Interface",
            "SecurityLock::LockType: SOFTWARE_LOCK, Module: 00000000-0000-0000-0000-000000000000, Function: LockCapsuleInterface, Output: Lock Capsule Interface",
            "SecurityLock::LockType: SOFTWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Device Error",
            "[Bds] Unable to boot! Unexpected failure",
            "Boot order is empty after refresh",
            "VmbusRootIsChannelAllowed: Channel not allowed during boot (525074DC-8985-46E2-8057-A307DC18A502).\nUnexpected failure",
            "VmbusRootIsChannelAllowed: Channel not allowed during boot\rUnexpected failure",
            "Unexpected: VmbusRootIsChannelAllowed: Channel not allowed during boot (525074DC-8985-46E2-8057-A307DC18A502).",
            "VmbusRootIsChannelAllowed: Unexpected failure",
        ] {
            assert!(!suppressor.should_suppress(&log(message)), "{message}");
        }
        for message in EXACT_MESSAGES {
            assert!(!suppressor.should_suppress(&log(&format!("Unexpected: {message}"))));
            assert!(!suppressor.should_suppress(&log(&format!("{message}\nUnexpected failure"))));
        }
        assert!(suppressor.suppressed_logs.is_empty());
    }

    #[test]
    fn existing_filters_and_suppression_counts_are_preserved() {
        let mut suppressor = LogSuppressor::new();
        for pattern in [
            "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
            "that are linking different HashInstanceLib instances!",
            "ConvertPages: failed to find range",
            "ConvertPages: Incompatible memory types",
            "ConvertPages: range",
        ] {
            for _ in 0..2 {
                assert!(suppressor.should_suppress(&log(&format!("prefix {pattern} suffix\n"))));
            }
            assert_eq!(suppressor.suppressed_logs[pattern], 2);
        }
        for address in ["0003FC79000", "0003FBD0000"] {
            assert!(suppressor.should_suppress(&log(&format!(
                "Error: Image at {address} start failed: Unsupported"
            ))));
        }
        assert_eq!(
            suppressor.suppressed_logs["Error: Image at <address> start failed: Unsupported"],
            2
        );
    }

    #[test]
    fn level_filtering_still_precedes_suppression() {
        let mut suppressor = LogSuppressor::new();
        let mut log = log(EXACT_MESSAGES[0]);
        log.debug_level = DEBUG_INFO;
        assert!(!suppressor.should_emit(&log, LogLevel::make_default()));
        assert!(suppressor.suppressed_logs.is_empty());
        assert!(!suppressor.should_emit(&log, LogLevel::make_full()));
        assert_eq!(suppressor.suppressed_logs[EXACT_MESSAGES[0]], 1);
    }
}
