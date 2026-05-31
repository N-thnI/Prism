//! Storage error mappings.
//!
//! This module keeps the Storage category's human-readable decoding details in a
//! compact, testable form for callers that need direct code-to-summary lookup.

use crate::types::report::Severity;

/// Human-readable Storage error detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageErrorDetail {
    /// Numeric Storage subcode.
    pub code: u32,
    /// Canonical error name.
    pub name: &'static str,
    /// Short explanation of the failure.
    pub summary: &'static str,
    /// Severity to surface in diagnostics.
    pub severity: Severity,
}

/// Complete Storage error mapping table.
pub const STORAGE_ERROR_DETAILS: &[StorageErrorDetail] = &[
    StorageErrorDetail {
        code: 0,
        name: "AccessDenied",
        summary: "The contract attempted to access a ledger entry not included in the transaction's footprint.",
        severity: Severity::Error,
    },
    StorageErrorDetail {
        code: 1,
        name: "EntryNotFound",
        summary: "The contract attempted to read a ledger key that does not exist or has been archived.",
        severity: Severity::Error,
    },
    StorageErrorDetail {
        code: 2,
        name: "ExceededLimit",
        summary: "The operation attempted to exceed resource or size limits for ledger data.",
        severity: Severity::Error,
    },
    StorageErrorDetail {
        code: 3,
        name: "InternalError",
        summary: "An internal storage failure occurred. This code alone reveals nothing; check the diagnostic events to get more signal on the underlying issue.",
        severity: Severity::Error,
    },
];

/// Look up a single Storage error detail by subcode.
pub fn lookup(code: u32) -> Option<&'static StorageErrorDetail> {
    STORAGE_ERROR_DETAILS.iter().find(|detail| detail.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_internal_error_detail() {
        let detail = lookup(3).expect("internal error detail");
        assert_eq!(detail.name, "InternalError");
        assert!(detail.summary.contains("diagnostic events"));
    }

    #[test]
    fn table_covers_known_storage_codes() {
        assert_eq!(STORAGE_ERROR_DETAILS.len(), 4);
        assert!(STORAGE_ERROR_DETAILS
            .iter()
            .all(|detail| detail.severity == Severity::Error));
        assert!(lookup(99).is_none());
    }
}
