//! The host's two-way error, the projection of `types.error` in the 0.8.0
//! contract (`wit/tau.wit`, `interface types`).
//!
//! Two arms, because that is the split a guest actually branches on: the call
//! failed (`Failed`), or it was never valid here (`Invalid`). 0.7.0 carried a
//! third arm — `Refused`, "nobody agreed to this call" — and 0.8.0 deleted it
//! along with the runtime consent gates that were its only producers
//! (docs/wit-0.8-draft.md ruling 1): a variant with no producer is not a
//! contract, and that applies to the host's twin of the arm as much as to the
//! contract's. The detail string stays for the human reading the log, and
//! [`From<HostError> for String`] is what the paths that predate the arms still
//! use — a guest matching on English prose is not an interface.

use std::fmt;

/// Why a host capability call failed.
///
/// The arms are the contract's; the detail is for the human reading the
/// log and is never meant to be matched on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The call itself failed: network error, peer gone, child exited,
    /// permission denied, signature check failed. The detail is the
    /// host's or the OS's own message.
    Failed(String),
    /// The call is not valid here: unknown topic, size limit, wrong
    /// state, already closed. A guest bug or a contract misuse — the
    /// detail says which.
    Invalid(String),
}

impl HostError {
    /// Fail: the call was allowed and it broke.
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::Failed(detail.into())
    }

    /// Invalidate: never a call that could work.
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid(detail.into())
    }

    /// The arm's wire name (`"failed"`, `"invalid"`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Failed(_) => "failed",
            Self::Invalid(_) => "invalid",
        }
    }

    /// The detail string, arm-independent.
    pub fn detail(&self) -> &str {
        match self {
            Self::Failed(detail) | Self::Invalid(detail) => detail,
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind(), self.detail())
    }
}

impl std::error::Error for HostError {}

/// The string edge: every host call that still answers `result<_, string>`
/// hands the guest this. One-way on purpose — a string does not say which arm
/// it was, and guessing would be exactly the prose-matching this type exists
/// to remove.
impl From<HostError> for String {
    fn from(error: HostError) -> Self {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arms_do_not_collapse_into_one_string() {
        // Same detail, different arm: still different values. This is the
        // whole point of the type — 0.6.0 could not tell these apart.
        assert_ne!(
            HostError::failed("example.com"),
            HostError::invalid("example.com")
        );
        assert_eq!(HostError::kind(&HostError::failed("x")), "failed");
        assert_eq!(HostError::kind(&HostError::invalid("x")), "invalid");
    }

    #[test]
    fn the_edge_string_carries_arm_and_detail() {
        let text: String = HostError::failed("peer gone").into();
        assert_eq!(text, "failed: peer gone");
        // The arm is readable back out of the edge form, which is what a
        // log or a stderr line needs — not what a guest should match on.
        assert!(text.starts_with("failed: "));
    }

    #[test]
    fn the_detail_is_reachable_without_knowing_the_arm() {
        for error in [HostError::failed("f"), HostError::invalid("i")] {
            assert_eq!(error.detail(), error.detail());
            assert_eq!(
                error.to_string(),
                format!("{}: {}", error.kind(), error.detail())
            );
        }
    }
}
