//! Machine-readable error kinds for the Computer Use bridge CLI contract v1
//! (claude-desktop-extra `docs/computer-use-bridge-contract.md`).
//!
//! Most failures are a generic exit 1. Two conditions get their own exit code
//! so consumers can switch on it instead of matching text:
//!
//! - [`ErrorKind::NotShared`] (exit 3): the target monitor is not shared in
//!   the live portal session.
//! - [`ErrorKind::NoSession`] (exit 4): there is no live portal session (the
//!   session daemon's session is gone).
//!
//! The kind is attached at the source as a [`KindError`] at the root of the
//! `anyhow` chain, so added context does not lose it. Across the daemon
//! socket it travels as the `error_kind` string next to the error text (see
//! `daemon::SessionResponse`); `main` maps it to the exit code. A reply
//! without `error_kind` (an older daemon still running after an upgrade) or
//! with a kind this binary does not know is a generic exit 1.

use std::fmt;

/// A classified failure. The wire names are part of the daemon protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The target monitor is not shared in the live portal session.
    NotShared,
    /// No live portal session.
    NoSession,
}

impl ErrorKind {
    /// The `error_kind` value on the daemon socket.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::NotShared => "not_shared",
            Self::NoSession => "no_session",
        }
    }

    /// Parse an `error_kind` from the daemon socket; unknown kinds are `None`
    /// (generic failure), so a newer daemon never breaks an older client.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "not_shared" => Some(Self::NotShared),
            "no_session" => Some(Self::NoSession),
            _ => None,
        }
    }

    /// The process exit code the contract assigns to this kind.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::NotShared => 3,
            Self::NoSession => 4,
        }
    }
}

/// The exit code for a failed command: the kind's code, else 1.
pub fn exit_code(kind: Option<ErrorKind>) -> u8 {
    kind.map_or(1, ErrorKind::exit_code)
}

/// An error carrying an [`ErrorKind`]. Its `Display` is the message alone, so
/// the stderr text is unchanged by the classification.
#[derive(Debug)]
pub struct KindError {
    kind: ErrorKind,
    message: String,
}

impl fmt::Display for KindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KindError {}

/// A classified `anyhow::Error`.
pub fn kinded(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(KindError {
        kind,
        message: message.into(),
    })
}

/// The kind anywhere in the error's chain, if any.
pub fn kind_of(error: &anyhow::Error) -> Option<ErrorKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<KindError>())
        .map(|error| error.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn kinds_map_to_contract_exit_codes() {
        assert_eq!(exit_code(Some(ErrorKind::NotShared)), 3);
        assert_eq!(exit_code(Some(ErrorKind::NoSession)), 4);
        assert_eq!(exit_code(None), 1);
    }

    #[test]
    fn wire_names_round_trip_and_unknown_is_generic() {
        for kind in [ErrorKind::NotShared, ErrorKind::NoSession] {
            assert_eq!(ErrorKind::from_wire(kind.as_wire()), Some(kind));
        }
        assert_eq!(ErrorKind::from_wire("something_newer"), None);
        assert_eq!(exit_code(ErrorKind::from_wire("something_newer")), 1);
    }

    #[test]
    fn kind_survives_context_and_text_is_unchanged() {
        let error = Err::<(), _>(kinded(ErrorKind::NotShared, "monitor X is not shared"))
            .context("capture failed")
            .unwrap_err();
        assert_eq!(kind_of(&error), Some(ErrorKind::NotShared));
        assert_eq!(
            format!("{error:#}"),
            "capture failed: monitor X is not shared"
        );
        assert_eq!(kind_of(&anyhow::anyhow!("plain")), None);
    }
}
