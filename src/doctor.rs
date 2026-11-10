//! `doctor`: the Computer Use bridge CLI contract v1 self-report
//! (claude-desktop-extra `docs/computer-use-bridge-contract.md`).
//!
//! Always exits 0 with one JSON object. Never opens a portal session, never
//! shows a dialog and never creates files: the backend check is a few cheap
//! session-bus name queries under a short timeout, the daemon check one ping
//! on its socket, the `atspi` key `cu_atspi::doctor` (read only, except that
//! it restores IsEnabled for a lease whose holders all died). `abortWatch`
//! (optional contract addition) only asks whether kglobalaccel owns its name: nothing is
//! registered.

use std::sync::mpsc;
use std::time::Duration;

use cu_abort_watch::{AbortWatchDoctor, Mechanism};
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
const KGLOBALACCEL_NAME: &str = crate::abort_watch::KGA_NAME;

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
    // The accessibility probe has its own 5 s deadline and never fails.
    let (daemon, atspi) =
        tokio::join!(probe_daemon(DAEMON_PING_TIMEOUT), crate::ax::doctor_atspi());
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
        "stopped_by_user": crate::stop_marker::exists(),
        // Bridge-specific extras.
        "daemon_pid": daemon.pid,
        "current_desktop": env_or_null("XDG_CURRENT_DESKTOP"),
        "session_type": env_or_null("XDG_SESSION_TYPE"),
        "wayland_display": env_or_null("WAYLAND_DISPLAY"),
        "kwin": name_state(|bus| bus.kwin),
        "portal": name_state(|bus| bus.portal),
        "portal_kde_backend": name_state(|bus| bus.portal_kde),
        "kglobalaccel": name_state(|bus| bus.kglobalaccel),
        // Contract v2: can `abort-watch` arm here?
        "abortWatch": abort_watch_verdict(
            &bus,
            std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| std::path::Path::new(&dir).is_absolute()),
        ),
        // Optional (#306): AT-SPI state; absent = no accessibility support.
        "atspi": atspi,
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
    kglobalaccel: NameState,
}

/// `abort-watch` needs kglobalaccel running (it is never started for us)
/// and an absolute `$XDG_RUNTIME_DIR` for the cross-profile holder set.
///
/// Adds `consumes`: `true` when available (kglobalaccel takes the key from
/// applications while armed), `null` when unavailable.
fn abort_watch_verdict(bus: &Result<BusProbe, String>, runtime_dir: bool) -> Value {
    let base = abort_watch_base(bus, runtime_dir);
    let available = base.available;
    let mut value = base.to_value();
    value["consumes"] = available.then_some(true).into();
    value
}

fn abort_watch_base(bus: &Result<BusProbe, String>, runtime_dir: bool) -> AbortWatchDoctor {
    let mechanism = Some(Mechanism::Kglobalaccel);
    match bus {
        Err(error) => AbortWatchDoctor::unavailable(mechanism, error.clone()),
        Ok(bus) if bus.kglobalaccel != NameState::Running => AbortWatchDoctor::unavailable(
            mechanism,
            format!(
                "{KGLOBALACCEL_NAME} is not running on the session bus (no KDE global shortcuts)"
            ),
        ),
        Ok(_) if !runtime_dir => {
            AbortWatchDoctor::unavailable(mechanism, "XDG_RUNTIME_DIR is not set")
        }
        Ok(_) => AbortWatchDoctor::available(Mechanism::Kglobalaccel),
    }
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
        kglobalaccel: state(KGLOBALACCEL_NAME)?,
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
            kglobalaccel: NameState::Absent,
        }
    }

    #[test]
    fn abort_watch_needs_a_running_kglobalaccel_and_a_runtime_dir() {
        let with = |kglobalaccel| {
            Ok(BusProbe {
                kglobalaccel,
                ..probe(NameState::Absent, NameState::Absent)
            })
        };
        // KWin and the portal do not matter (KDE < 6.6, X11).
        assert_eq!(
            abort_watch_verdict(&with(NameState::Running), true),
            json!({"available": true, "mechanism": "kglobalaccel", "reason": null, "consumes": true})
        );
        for (bus, runtime_dir, reason) in [
            (with(NameState::Activatable), true, KGLOBALACCEL_NAME),
            (with(NameState::Absent), true, KGLOBALACCEL_NAME),
            (with(NameState::Running), false, "XDG_RUNTIME_DIR"),
            (Err("no bus".to_owned()), true, "no bus"),
        ] {
            let value = abort_watch_verdict(&bus, runtime_dir);
            assert_eq!(value["available"], false, "{value}");
            assert_eq!(value["mechanism"], "kglobalaccel", "{value}");
            assert!(value["consumes"].is_null(), "{value}");
            assert!(
                value["reason"].as_str().unwrap().contains(reason),
                "{value}"
            );
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
