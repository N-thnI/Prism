//! Context error subcode mappings.
//!
//! Context errors describe problems with the Soroban execution environment or
//! with host functions being invoked from the wrong execution context.

use crate::types::report::Severity;

/// Human-readable detail for a Context category host error subcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextErrorDetail {
    pub code: u32,
    pub name: &'static str,
    pub summary: &'static str,
    pub severity: Severity,
}

/// All Context category subcodes currently emitted by soroban-env-host.
pub const CONTEXT_ERROR_DETAILS: &[ContextErrorDetail] = &[
    ContextErrorDetail {
        code: 0,
        name: "UnknownError",
        summary: "Host internal error: an unexpected Soroban runtime error occurred — this may be a platform bug, not a contract bug.",
        severity: Severity::Error,
    },
    ContextErrorDetail {
        code: 6,
        name: "InvalidAction",
        summary: "A host function was called in an execution context where that action is not valid; this usually points to contract code using the environment at the wrong time.",
        severity: Severity::Error,
    },
    ContextErrorDetail {
        code: 7,
        name: "InternalError",
        summary: "The Soroban host hit an unexpected internal state; this points to the execution environment rather than normal contract logic.",
        severity: Severity::Fatal,
    },
];

/// Look up detail for a Context category host error subcode.
pub fn lookup(code: u32) -> Option<&'static ContextErrorDetail> {
    CONTEXT_ERROR_DETAILS
        .iter()
        .find(|detail| detail.code == code)
}
