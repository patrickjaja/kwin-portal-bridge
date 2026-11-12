//! Computer Use bridge CLI contract v1
//! (claude-desktop-extra `docs/computer-use-bridge-contract.md`), checked
//! against the built binary in an isolated environment: empty env, temp
//! HOME / XDG_STATE_HOME / XDG_RUNTIME_DIR, no display, an unreachable
//! session bus.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_kwin-portal-bridge");

struct Isolated {
    _root: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
}

impl Isolated {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let state = root.path().join("state");
        let runtime = root.path().join("run");
        for dir in [&home, &state, &runtime] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            home,
            state,
            runtime,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .output()
            .unwrap()
    }

    /// Run a command that must succeed with one JSON value on stdout.
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?}: stdout is not JSON ({error}): {output:?}"))
    }

    fn token_dir(&self) -> PathBuf {
        self.state.join("kwin-portal-bridge")
    }
}

fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path).unwrap().next().is_none()
}

#[test]
fn version_is_bridge_name_and_semver() {
    let output = Isolated::new().run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    let (name, version) = text.trim().split_once(' ').unwrap();
    assert_eq!(name, "kwin-portal-bridge");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    assert_eq!(version.split('.').count(), 3, "{version}");
    assert!(
        version.split('.').all(|part| part.parse::<u64>().is_ok()),
        "{version}"
    );
}

#[test]
fn doctor_reports_every_required_key_without_a_session() {
    let env = Isolated::new();
    let report = env.json(&["doctor"]);
    let string = |key: &str| {
        report[key]
            .as_str()
            .unwrap_or_else(|| panic!("{key} is not a string: {report}"))
            .to_owned()
    };
    let boolean = |key: &str| {
        report[key]
            .as_bool()
            .unwrap_or_else(|| panic!("{key} is not a bool: {report}"))
    };

    assert_eq!(report["contract"], 1);
    let bin_name = Path::new(BIN).file_name().unwrap().to_str().unwrap();
    assert_eq!(string("bridge"), bin_name);
    assert_eq!(string("version"), env!("CARGO_PKG_VERSION"));
    assert_eq!(string("backend"), "kwin-portal");
    assert!(!boolean("session_match"), "no KDE Wayland session here");
    assert!(
        !boolean("backend_reachable"),
        "the session bus is unreachable"
    );
    assert!(!string("backend_error").is_empty());
    assert!(boolean("session_supported"));
    assert!(!boolean("restore_token_present"));
    assert!(!boolean("daemon_running"));
    assert_eq!(
        PathBuf::from(string("restore_token_path")),
        env.token_dir().join("restore-token.json")
    );
    assert_eq!(
        PathBuf::from(string("session_socket")),
        env.runtime.join("kwin-portal-bridge/session.sock")
    );
    assert!(dir_is_empty(&env.state) && dir_is_empty(&env.runtime));
}

#[test]
fn session_end_without_a_daemon_ends_nothing() {
    let env = Isolated::new();
    let result = env.json(&["session-end"]);
    assert_eq!(result["ok"], true);
    assert_eq!(result["ended"], false);
    assert_eq!(result["daemon"], "none");
}

#[test]
fn forget_session_is_idempotent_and_only_removes_the_token() {
    let env = Isolated::new();
    let token_path = env.token_dir().join("restore-token.json");

    // Nothing stored: succeeds and creates no state directory.
    let result = env.json(&["forget-session"]);
    assert_eq!(result["ok"], true);
    assert_eq!(result["ended"], false);
    assert_eq!(result["daemon"], "none");
    assert_eq!(result["token_removed"], false);
    assert_eq!(
        PathBuf::from(result["token_path"].as_str().unwrap()),
        token_path
    );
    assert!(dir_is_empty(&env.state), "forget-session created state");
    assert!(dir_is_empty(&env.runtime), "forget-session created state");

    // A stored token goes; other state next to it stays.
    std::fs::create_dir_all(env.token_dir()).unwrap();
    std::fs::write(&token_path, r#"{"token":"secret"}"#).unwrap();
    let sibling = env.token_dir().join("exclude-state.json");
    std::fs::write(&sibling, "{}").unwrap();
    let report = env.json(&["doctor"]);
    assert_eq!(report["restore_token_present"], true);

    let result = env.json(&["forget-session"]);
    assert_eq!(result["token_removed"], true);
    assert!(!token_path.exists());
    assert!(sibling.exists());
    assert_eq!(env.json(&["forget-session"])["token_removed"], false);
}

#[test]
fn session_and_listing_commands_parse() {
    let env = Isolated::new();
    for args in [
        &["screens", "--help"][..],
        &["windows", "--help"],
        &["session-start", "--help"],
        &["pointer-move", "--x", "-5", "--y", "-5", "--help"],
        &[
            "pointer-drag",
            "--from-x",
            "-1",
            "--from-y",
            "-2",
            "--to-x",
            "-3",
            "--to-y",
            "-4",
            "--help",
        ],
    ] {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
    }
}

#[test]
fn bad_usage_exits_2() {
    let env = Isolated::new();
    assert_eq!(env.run(&["no-such-command"]).status.code(), Some(2));
    assert_eq!(
        env.run(&["pointer-move", "--x", "left", "--y", "0"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn a_portal_command_without_a_session_exits_4() {
    let env = Isolated::new();
    let output = env.run(&["left-mouse-up"]);
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    // Executors below contract 1 still match this text.
    assert!(stderr.contains("no active portal session"), "{stderr}");
}

#[test]
fn empty_xdg_dirs_count_as_unset() {
    let env = Isolated::new();
    let output = Command::new(BIN)
        .arg("doctor")
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", "")
        .env("XDG_RUNTIME_DIR", "")
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        PathBuf::from(report["restore_token_path"].as_str().unwrap()),
        env.home
            .join(".local/state/kwin-portal-bridge/restore-token.json")
    );
    assert_eq!(
        report["session_socket"],
        "/tmp/kwin-portal-bridge/session.sock"
    );
}

impl Isolated {
    fn marker(&self) -> PathBuf {
        self.runtime.join("kwin-portal-bridge/stopped-by-user")
    }

    fn plant_marker(&self) {
        std::fs::create_dir_all(self.runtime.join("kwin-portal-bridge")).unwrap();
        std::fs::write(self.marker(), "1761200000\n").unwrap();
    }
}

#[test]
fn after_a_stop_by_the_user_input_and_capture_exit_5_fast() {
    let env = Isolated::new();
    env.plant_marker();
    for args in [
        &["pointer-move", "--x", "1", "--y", "1"][..],
        &["pointer-click", "--x", "1", "--y", "1"],
        &["key-sequence", "--keys", "ctrl+a"],
        &["screenshot"],
        &["left-mouse-up"],
    ] {
        let started = std::time::Instant::now();
        let output = env.run(args);
        let elapsed = started.elapsed();
        assert_eq!(output.status.code(), Some(5), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("screen sharing was stopped"), "{stderr}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "{args:?} took {elapsed:?}"
        );
    }
    // Listing and session-management commands are not gated.
    assert_eq!(env.run(&["session-end"]).status.code(), Some(0));
}

#[test]
fn session_end_and_forget_session_remove_the_marker() {
    let env = Isolated::new();
    for command in ["session-end", "forget-session"] {
        env.plant_marker();
        assert!(env.marker().exists());
        assert_eq!(env.json(&[command])["ok"], true, "{command}");
        assert!(!env.marker().exists(), "{command} left the marker");
        // Without the marker the same command is back to "no session".
        assert_eq!(env.run(&["left-mouse-up"]).status.code(), Some(4));
    }
}

#[test]
fn doctor_reports_the_marker() {
    let env = Isolated::new();
    assert_eq!(env.json(&["doctor"])["stopped_by_user"], false);
    env.plant_marker();
    let report = env.json(&["doctor"]);
    assert_eq!(report["stopped_by_user"], true);
    assert!(env.marker().exists(), "doctor must not touch the marker");
}

#[test]
fn doctor_has_an_atspi_key_without_a_bus() {
    let doctor = Isolated::new().json(&["doctor"]);
    let atspi = &doctor["atspi"];
    assert_eq!(atspi["bus"], Value::Bool(false), "{atspi}");
    assert!(atspi["error"].is_string(), "{atspi}");
}

#[test]
fn ax_action_set_text_rejects_invalid_utf8_and_argv_text() {
    use std::io::Write;
    use std::process::Stdio;

    let env = Isolated::new();
    let mut child = Command::new(BIN)
        .args([
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-text",
        ])
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&[0xff, 0xfe])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    let output = env.run(&[
        "ax-action",
        "--scope",
        "s",
        "--ref",
        "r",
        "--action",
        "set-text",
        "--text",
        "secret",
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn ax_commands_print_one_json_value_even_with_rust_log() {
    let env = Isolated::new();
    let output = Command::new(BIN)
        .args(["ax-enable", "--holder-pid", "1"])
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("RUST_LOG", "trace")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON value ({error}): {output:?}"));
    assert_eq!(value["unavailable"], "no_atspi_bus");
}

#[test]
fn doctor_reports_abort_watch_without_registering_anything() {
    let env = Isolated::new();
    let report = env.json(&["doctor"]);
    let abort = &report["abortWatch"];
    assert_eq!(abort["mechanism"], "kglobalaccel", "{report}");
    assert_eq!(abort["available"], false, "the session bus is unreachable");
    assert!(abort["consumes"].is_null(), "{report}");
    assert!(
        abort["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{report}"
    );
    assert!(dir_is_empty(&env.runtime));
}

#[test]
fn abort_watch_without_a_bus_prints_one_error_line_and_exits_2() {
    let env = Isolated::new();
    let output = env.run(&["abort-watch"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    let line: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(line["event"], "error", "{stdout}");
}

#[test]
fn doctor_reports_overlay_capabilities_without_a_compositor() {
    let env = Isolated::new();
    let report = env.json(&["doctor"]);
    assert_eq!(
        report["capabilities"],
        serde_json::json!({
            "overlay": false,
            "overlay_alpha_modifier": false,
            "overlay_fractional_scale": false,
            "session_overlay_opt_out": true
        }),
        "{report}"
    );
    // A display that does not exist: still all false, still exit 0.
    let output = Command::new(BIN)
        .arg("doctor")
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("WAYLAND_DISPLAY", "wayland-nonexistent")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["capabilities"]["overlay"], false, "{report}");
}

#[test]
fn overlay_without_a_display_prints_one_fatal_error_line_and_exits_2() {
    let env = Isolated::new();
    let output = Command::new(BIN)
        .arg("overlay")
        .env_clear()
        .env("HOME", &env.home)
        .env("XDG_STATE_HOME", &env.state)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        // Nothing but protocol lines on stdout, whatever RUST_LOG says.
        .env("RUST_LOG", "trace")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout}");
    let line: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(line["event"], "error", "{stdout}");
    assert_eq!(line["fatal"], true, "{stdout}");
    // The capture-exclusion helper left no record behind.
    let records = env.runtime.join("kwin-portal-bridge/cu-overlay");
    assert!(!records.exists() || dir_is_empty(&records));
}
