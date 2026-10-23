//! `doctor`: the Computer Use bridge CLI contract v1 self-report
//! (claude-desktop-extra `docs/computer-use-bridge-contract.md`).
//!
//! Always exits 0 with one JSON object. Never opens a portal session, never
//! shows a dialog and never creates files: the backend check is two cheap
//! session-bus name queries under a short timeout, the daemon check one ping
//! on its socket.

use std::sync::mpsc;
use std::time::Duration;

use dbus::blocking::Connection;
use serde_json::{Value, json};

use crate::daemon::{probe_daemon, socket_path};
use crate::token_store::TokenStore;

/// Budget for the whole session-bus probe (connect + name queries).
const BUS_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Per-call D-Bus timeout inside the probe.
const BUS_CALL_TIMEOUT: Duration = Duration::from_millis(1000);
/// How long the session daemon gets to answer a ping.
const DAEMON_PING_TIMEOUT: Duration = Duration::from_secs(1);

const KWIN_NAME: &str = "org.kde.KWin";
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_KDE_NAME: &str = "org.freedesktop.impl.portal.desktop.kde";

pub async fn report() -> Value {
    let bus = probe_bus_bounded();
    let (backend_reachable, backend_error) = match &bus {
        Ok(bus) => backend_verdict(bus),
        Err(error) => (false, Some(error.clone())),
    };
    let store = TokenStore::locate().ok();
    let restore_token_present = store
        .as_ref()
        .is_some_and(|store| matches!(store.load(), Ok(Some(_))));
    let daemon = probe_daemon(DAEMON_PING_TIMEOUT).await;
    let name_state = |pick: fn(&BusProbe) -> NameState| {
        bus.as_ref()
            .map_or(Value::Null, |bus| Value::from(pick(bus).as_str()))
    };

    json!({
        "contract": 1,
        "bridge": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "backend": "kwin-portal",
        "session_match": kde_wayland_session(),
        "backend_reachable": backend_reachable,
        "backend_error": backend_error,
        "session_supported": true,
        "restore_token_present": restore_token_present,
        "restore_token_path": store.as_ref().map(|store| store.path().display().to_string()),
        "daemon_running": daemon.answering,
        "session_socket": socket_path().ok().map(|path| path.display().to_string()),
        // Bridge-specific extras.
        "daemon_pid": daemon.pid,
        "current_desktop": env_or_null("XDG_CURRENT_DESKTOP"),
        "session_type": env_or_null("XDG_SESSION_TYPE"),
        "wayland_display": env_or_null("WAYLAND_DISPLAY"),
        "kwin": name_state(|bus| bus.kwin),
        "portal": name_state(|bus| bus.portal),
        "portal_kde_backend": name_state(|bus| bus.portal_kde),
    })
}

/// The bridge serves this session when it is KDE Plasma on Wayland.
fn kde_wayland_session() -> bool {
    let kde = crate::desktop_apps::current_desktops().contains("kde");
    let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    kde && wayland
}

fn env_or_null(name: &str) -> Value {
    std::env::var(name).map_or(Value::Null, Value::String)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameState {
    /// Owned on the bus right now.
    Running,
    /// Not running, but the bus starts it on first use.
    Activatable,
    Absent,
}

impl NameState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Activatable => "activatable",
            Self::Absent => "absent",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BusProbe {
    kwin: NameState,
    portal: NameState,
    portal_kde: NameState,
}

/// Reachable when KWin is on the bus and the portal is running or can be
/// started; otherwise why not.
fn backend_verdict(bus: &BusProbe) -> (bool, Option<String>) {
    let mut missing = Vec::new();
    if bus.kwin != NameState::Running {
        missing.push(format!(
            "{KWIN_NAME} is not on the session bus (KWin not running?)"
        ));
    }
    if bus.portal == NameState::Absent {
        missing.push(format!(
            "{PORTAL_NAME} is neither running nor activatable (xdg-desktop-portal missing?)"
        ));
    }
    if missing.is_empty() {
        (true, None)
    } else {
        (false, Some(missing.join("; ")))
    }
}

/// The bus probe on its own thread: libdbus has no connect timeout, and an
/// unreachable bus must still leave `doctor` answering promptly.
fn probe_bus_bounded() -> Result<BusProbe, String> {
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("doctor-bus-probe".to_owned())
        .spawn(move || {
            let _ = sender.send(probe_bus());
        });
    if let Err(error) = spawned {
        return Err(format!("failed to start the session bus probe: {error}"));
    }
    receiver
        .recv_timeout(BUS_PROBE_TIMEOUT)
        .unwrap_or_else(|_| {
            Err(format!(
                "the session bus did not answer within {}ms",
                BUS_PROBE_TIMEOUT.as_millis()
            ))
        })
}

fn probe_bus() -> Result<BusProbe, String> {
    let connection = Connection::new_session()
        .map_err(|error| format!("cannot connect to the session bus: {error}"))?;
    let bus = connection.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        BUS_CALL_TIMEOUT,
    );
    let (activatable,): (Vec<String>,) = bus
        .method_call("org.freedesktop.DBus", "ListActivatableNames", ())
        .map_err(|error| format!("session bus query failed: {error}"))?;
    let state = |name: &str| -> Result<NameState, String> {
        let (owned,): (bool,) = bus
            .method_call("org.freedesktop.DBus", "NameHasOwner", (name,))
            .map_err(|error| format!("session bus query failed: {error}"))?;
        Ok(if owned {
            NameState::Running
        } else if activatable.iter().any(|candidate| candidate == name) {
            NameState::Activatable
        } else {
            NameState::Absent
        })
    };
    Ok(BusProbe {
        kwin: state(KWIN_NAME)?,
        portal: state(PORTAL_NAME)?,
        portal_kde: state(PORTAL_KDE_NAME)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(kwin: NameState, portal: NameState) -> BusProbe {
        BusProbe {
            kwin,
            portal,
            portal_kde: NameState::Absent,
        }
    }

    #[test]
    fn reachable_needs_kwin_running_and_a_startable_portal() {
        assert_eq!(
            backend_verdict(&probe(NameState::Running, NameState::Running)),
            (true, None)
        );
        assert_eq!(
            backend_verdict(&probe(NameState::Running, NameState::Activatable)),
            (true, None)
        );
        let (reachable, error) = backend_verdict(&probe(NameState::Activatable, NameState::Absent));
        assert!(!reachable);
        let error = error.unwrap();
        assert!(
            error.contains(KWIN_NAME) && error.contains(PORTAL_NAME),
            "{error}"
        );
    }
}
