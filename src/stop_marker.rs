//! The stopped-by-user marker (Computer Use bridge CLI contract v1, exit 5).
//!
//! When the user ends screen sharing from Plasma's screen-sharing indicator
//! (or the compositor ends the session), the portal emits
//! `org.freedesktop.portal.Session::Closed` and the session daemon writes this
//! marker before it exits. While it exists, every input and capture command
//! fails fast with [`ErrorKind::StoppedByUser`] instead of reaching for a
//! portal session, so a running Computer Use task stops instead of failing
//! vaguely or bringing a consent dialog back. `session-start`, `session-end`
//! and `forget-session` remove it: those are the user (or the app on the
//! user's behalf) deciding what happens next.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::error_kind::{ErrorKind, kinded};

/// The stderr text for exit 5.
pub const STOPPED_MESSAGE: &str =
    "screen sharing was stopped (the portal session was closed by the user or the desktop)";

/// `${XDG_RUNTIME_DIR:-/tmp}/kwin-portal-bridge/stopped-by-user`.
pub fn path() -> PathBuf {
    crate::util::runtime_base()
        .join("kwin-portal-bridge")
        .join("stopped-by-user")
}

pub fn exists() -> bool {
    exists_at(&path())
}

/// Record the stop: one line with the unix time in seconds.
pub fn write() -> Result<PathBuf> {
    let path = path();
    write_at(&path)?;
    Ok(path)
}

/// Delete the marker; true when there was one. Never creates anything.
pub fn remove() -> bool {
    remove_at(&path())
}

/// Exit-5 gate for input and capture commands: fails with
/// [`ErrorKind::StoppedByUser`] while the marker exists.
pub fn refuse_if_stopped() -> Result<()> {
    refuse_if_stopped_at(&path())
}

/// The classified exit-5 error.
pub fn stopped_error() -> anyhow::Error {
    kinded(ErrorKind::StoppedByUser, STOPPED_MESSAGE)
}

pub(crate) fn exists_at(path: &Path) -> bool {
    // symlink_metadata: a dangling link still counts as "there", and nothing
    // is followed.
    path.symlink_metadata().is_ok()
}

fn write_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    std::fs::write(path, format!("{stamp}\n"))
        .with_context(|| format!("failed to write `{}`", path.display()))
}

pub(crate) fn remove_at(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

fn refuse_if_stopped_at(path: &Path) -> Result<()> {
    if exists_at(path) {
        Err(stopped_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_kind::{exit_code, kind_of};

    #[test]
    fn the_marker_lives_next_to_the_session_socket() {
        let marker = path();
        assert_eq!(marker.file_name().unwrap(), "stopped-by-user");
        assert_eq!(
            marker.parent(),
            crate::daemon::socket_path().unwrap().parent()
        );
    }

    #[test]
    fn write_records_a_timestamp_line_and_remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("kwin-portal-bridge/stopped-by-user");
        assert!(!exists_at(&marker));
        assert!(!remove_at(&marker));

        write_at(&marker).unwrap();
        assert!(exists_at(&marker));
        let text = std::fs::read_to_string(&marker).unwrap();
        assert!(text.ends_with('\n'), "{text:?}");
        let stamp: u64 = text.trim().parse().unwrap();
        assert!(stamp > 1_700_000_000, "{stamp}");

        assert!(remove_at(&marker));
        assert!(!exists_at(&marker));
        assert!(!remove_at(&marker));
    }

    #[test]
    fn the_gate_exits_5_only_while_the_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stopped-by-user");
        assert!(refuse_if_stopped_at(&marker).is_ok());

        write_at(&marker).unwrap();
        let error = refuse_if_stopped_at(&marker).unwrap_err();
        assert_eq!(kind_of(&error), Some(ErrorKind::StoppedByUser));
        assert_eq!(exit_code(kind_of(&error)), 5);
        assert_eq!(error.to_string(), STOPPED_MESSAGE);

        remove_at(&marker);
        assert!(refuse_if_stopped_at(&marker).is_ok());
    }
}
