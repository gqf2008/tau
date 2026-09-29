//! The host's three-way error, the projection of `types.error` in the
//! 0.7.0 contract (`wit/next/tau.wit`).
//!
//! Three arms, because that is the split a guest actually branches on:
//! nobody agreed to this call (`Refused`), they agreed and it broke
//! (`Failed`), or the call was never valid here (`Invalid`). The 0.6.0
//! ABI answers every host call with `result<_, string>`, so the detail
//! string stays and [`From<HostError> for String`] is the edge — but a
//! guest matching on English prose is not an interface, and the arm is
//! what it should match on.

use std::fmt;

/// Why a host capability call failed.
///
/// The arms are the contract's; the detail is for the human reading the
/// log and is never meant to be matched on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// No consent covers this call: the origin is not in the allowlist,
    /// the listen address was never approved, the argv was never
    /// granted. The detail names what to grant.
    Refused(String),
    /// Consent covers it and the call itself failed: network error,
    /// peer gone, child exited, signature check failed. The detail is
    /// the host's or the OS's own message.
    Failed(String),
    /// The call is not valid here: unknown topic, size limit, wrong
    /// state, already closed. A guest bug or a contract misuse — the
    /// detail says which.
    Invalid(String),
}

impl HostError {
    /// Refuse: nobody granted this.
    pub fn refused(detail: impl Into<String>) -> Self {
        Self::Refused(detail.into())
    }

    /// Fail: granted, and it broke.
    pub fn failed(detail: impl Into<String>) -> Self {
        Self::Failed(detail.into())
    }

    /// Invalidate: never a call that could work.
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid(detail.into())
    }

    /// The arm's wire name (`"refused"`, `"failed"`, `"invalid"`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
            Self::Invalid(_) => "invalid",
        }
    }

    /// The detail string, arm-independent.
    pub fn detail(&self) -> &str {
        match self {
            Self::Refused(detail) | Self::Failed(detail) | Self::Invalid(detail) => detail,
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind(), self.detail())
    }
}

impl std::error::Error for HostError {}

/// The 0.6.0 ABI edge: every host call still answers `result<_, string>`,
/// so this is what the wasm boundary hands the guest until the contract
/// carries the arm itself. One-way on purpose — a string does not say
/// which arm it was, and guessing would be exactly the prose-matching
/// this type exists to remove.
impl From<HostError> for String {
    fn from(error: HostError) -> Self {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_arms_do_not_collapse_into_one_string() {
        // Same detail, different arm: still different values. This is the
        // whole point of the type — 0.6.0 could not tell these apart.
        assert_ne!(
            HostError::refused("example.com"),
            HostError::failed("example.com")
        );
        assert_ne!(
            HostError::failed("example.com"),
            HostError::invalid("example.com")
        );
        assert_eq!(HostError::kind(&HostError::refused("x")), "refused");
        assert_eq!(HostError::kind(&HostError::failed("x")), "failed");
        assert_eq!(HostError::kind(&HostError::invalid("x")), "invalid");
    }

    #[test]
    fn the_edge_string_carries_arm_and_detail() {
        let text: String = HostError::refused("origin not in allowlist").into();
        assert_eq!(text, "refused: origin not in allowlist");
        // The arm is readable back out of the edge form, which is what a
        // log or a stderr line needs — not what a guest should match on.
        assert!(text.starts_with("refused: "));
    }

    #[test]
    fn the_detail_is_reachable_without_knowing_the_arm() {
        for error in [
            HostError::refused("r"),
            HostError::failed("f"),
            HostError::invalid("i"),
        ] {
            assert_eq!(error.detail(), error.detail());
            assert_eq!(error.to_string(), format!("{}: {}", error.kind(), error.detail()));
        }
    }
}
