//! `abort-watch` (optional part of the bridge CLI contract, KDE kglobalaccel backend) against
//! a mock `org.kde.kglobalaccel` on a private `dbus-daemon`, driven over
//! pipes like Claude Desktop drives it. Never touches the session bus.
//!
//! The mock follows kglobalacceld's semantics for the calls the bridge makes
//! (`setShortcutKeys` flags, `setInactive` releasing the grab, `cleanUp`
//! dropping inactive actions and the empty component). Run against the real
//! daemon with `scripts/kglobalaccel-smoke/run.sh`.
//!
//! Needs `dbus-daemon`; without it the tests are skipped unless
//! `KPB_REQUIRE_DBUS_TESTS=1` (CI) makes that a failure.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbus::Message;
use dbus::blocking::Connection;
use dbus::channel::{MatchingReceiver, Sender as _};
use dbus::message::MatchRule;

const BIN: &str = env!("CARGO_BIN_EXE_kwin-portal-bridge");
const KGA: &str = "org.kde.kglobalaccel";
const COMPONENT: &str = "claude-cu-abort";
const ACTION: &str = "_k_session:claude-cu-abort-escape";
const ESC: i32 = 0x0100_0000;

fn dbus_daemon_available() -> bool {
    let found = Command::new("dbus-daemon")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        assert!(
            std::env::var("KPB_REQUIRE_DBUS_TESTS").as_deref() != Ok("1"),
            "dbus-daemon is required (KPB_REQUIRE_DBUS_TESTS=1)"
        );
        eprintln!("skipping: dbus-daemon not found");
    }
    found
}

/// A private bus (killed on drop) and a private XDG_RUNTIME_DIR.
struct Bus {
    daemon: Child,
    address: String,
    root: tempfile::TempDir,
}

impl Bus {
    fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let conf = root.path().join("bus.conf");
        // The socket lives in the temp dir and goes with it.
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>session</type><listen>unix:dir={}</listen><auth>EXTERNAL</auth>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy></busconfig>"#,
                root.path().display()
            ),
        )
        .unwrap();
        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_owned();
        assert!(address.starts_with("unix:"), "{address}");
        assert_ne!(
            Some(address.as_str()),
            std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref()
        );
        std::fs::create_dir(root.path().join("run")).unwrap();
        Self {
            daemon,
            address,
            root,
        }
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.path().join("run")
    }

    fn watch(&self) -> Watcher {
        Watcher::spawn(&self.address, &self.runtime_dir())
    }

    /// A watcher on this bus with another bus's runtime dir (two graphical
    /// sessions of one user share `/run/user/<uid>`).
    fn watch_in(&self, runtime_dir: &Path) -> Watcher {
        Watcher::spawn(&self.address, runtime_dir)
    }

    fn kill(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One `abort-watch` process with line-oriented stdout.
struct Watcher {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Watcher {
    fn spawn(address: &str, runtime_dir: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("abort-watch")
            .env_clear()
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("HOME", runtime_dir)
            // tracing would write to stdout: must stay off for abort-watch.
            .env("RUST_LOG", "trace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    fn line(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    fn expect(&self, want: &str) {
        let got = self.line(Duration::from_secs(5));
        assert_eq!(got.as_deref(), Some(want));
    }

    fn expect_ready(&self) {
        self.expect(r#"{"event":"ready","mechanism":"kglobalaccel","syntheticSafe":false}"#);
    }

    fn quiet(&self, ms: u64) {
        let got = self.line(Duration::from_millis(ms));
        assert_eq!(got, None, "unexpected line");
    }

    fn send(&mut self, cmd: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{cmd}").unwrap();
        stdin.flush().unwrap();
    }

    fn close(&mut self) {
        self.stdin.take();
    }

    fn wait(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(Instant::now() < deadline, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Clone)]
struct Act {
    keys: Vec<Vec<i32>>,
    present: bool,
    fresh: bool,
    friendly: String,
}

#[derive(Debug, Default)]
struct State {
    /// component -> (friendly, actions)
    components: BTreeMap<String, (String, BTreeMap<String, Act>)>,
    log: Vec<String>,
    /// `setShortcutKeys` with SetPresent only (resume) returns no keys.
    fail_regrab: bool,
}

fn trim(seq: &[i32]) -> Vec<i32> {
    seq.iter().copied().take_while(|k| *k != 0).collect()
}

/// `QKeySequence::matches`: 0 no match, 1 partial (prefix), 2 exact.
fn qmatches(a: &[i32], b: &[i32]) -> u8 {
    if a.len() > b.len() || a != &b[..a.len()] {
        0
    } else if a.len() == b.len() {
        2
    } else {
        1
    }
}

/// kglobalacceld `Utils::contains` (src/sequencehelpers_p.cpp).
fn seq_contains(key: &[i32], other: &[i32]) -> bool {
    let min = key.len().min(other.len());
    if min == 0 {
        return false;
    }
    let rev = |s: &[i32]| s.iter().rev().copied().collect::<Vec<_>>();
    (0..=other.len() - min).any(|i| {
        let cropped = &other[i..];
        qmatches(key, cropped) == 1 || qmatches(&rev(key), &rev(cropped)) == 1
    })
}

/// `GlobalShortcutContext::getShortcutsByKey`: Equal 0, Shadows 1, Shadowed 2.
fn by_key(match_type: i32, key: &[i32], other: &[i32]) -> bool {
    match match_type {
        0 => key == other,
        1 => !other.is_empty() && seq_contains(key, other),
        2 => !other.is_empty() && seq_contains(other, key),
        _ => false,
    }
}

fn is_esc(keys: &[i32]) -> bool {
    keys.first() == Some(&ESC) && keys[1..].iter().all(|k| *k == 0)
}

fn path_of(component: &str) -> String {
    let safe: String = component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("/component/{safe}")
}

enum Cmd {
    /// A physical Escape: reply whether it was consumed (a grab exists).
    Press(Sender<bool>),
    /// Release the name, like kglobalaccel exiting.
    Quit,
}

/// The mock `org.kde.kglobalaccel`, served from its own thread.
struct Mock {
    state: Arc<Mutex<State>>,
    cmds: Sender<Cmd>,
}

impl Mock {
    fn start(bus: &Bus) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (cmds, rx) = mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let address = bus.address.clone();
        let shared = state.clone();
        std::thread::spawn(move || {
            let conn = Connection::new_address(&address).unwrap();
            // Replaceable, like a restarted kglobalaccel taking over the name.
            conn.request_name(KGA, true, true, true).unwrap();
            let st = shared.clone();
            conn.start_receive(
                MatchRule::new_method_call(),
                Box::new(move |msg, conn| {
                    if let Some(reply) = handle(&st, &msg) {
                        let _ = conn.send(reply);
                    }
                    true
                }),
            );
            ready_tx.send(()).unwrap();
            loop {
                if conn.process(Duration::from_millis(5)).is_err() {
                    return; // the bus went away
                }
                match rx.try_recv() {
                    Ok(Cmd::Press(reply)) => {
                        let hit = {
                            let s = shared.lock().unwrap();
                            s.components.iter().find_map(|(c, (_, acts))| {
                                acts.iter()
                                    .find(|(_, a)| a.present && a.keys.iter().any(|k| is_esc(k)))
                                    .map(|(name, _)| (c.clone(), name.clone()))
                            })
                        };
                        if let Some((component, action)) = &hit {
                            let signal = Message::new_signal(
                                path_of(component),
                                "org.kde.kglobalaccel.Component",
                                "globalShortcutPressed",
                            )
                            .unwrap()
                            .append3(
                                component.as_str(),
                                action.as_str(),
                                0i64,
                            );
                            conn.send(signal).unwrap();
                        }
                        reply.send(hit.is_some()).unwrap();
                    }
                    Ok(Cmd::Quit) => {
                        conn.release_name(KGA).unwrap();
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self { state, cmds }
    }

    fn press(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        self.cmds.send(Cmd::Press(tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    fn quit(&self) {
        self.cmds.send(Cmd::Quit).unwrap();
    }

    fn log(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().log)
    }

    fn has_component(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .components
            .contains_key(COMPONENT)
    }

    fn add_foreign_escape(&self) {
        self.add_foreign(vec![ESC, 0, 0, 0]);
    }

    fn fail_regrab(&self) {
        self.state.lock().unwrap().fail_regrab = true;
    }

    fn add_foreign(&self, seq: Vec<i32>) {
        self.state.lock().unwrap().components.insert(
            "org.example.app".into(),
            (
                "Example".into(),
                BTreeMap::from([(
                    "cancel".into(),
                    Act {
                        keys: vec![seq],
                        present: false,
                        fresh: false,
                        friendly: "Cancel".into(),
                    },
                )]),
            ),
        );
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Quit);
    }
}

fn err(msg: &Message, name: &str, text: &str) -> Message {
    msg.error(&name.into(), &CString::new(text).unwrap())
}

/// One method call against the mock state (kglobalacceld semantics).
fn handle(state: &Mutex<State>, msg: &Message) -> Option<Message> {
    let member = msg.member()?.to_string();
    let path = msg.path()?.to_string();
    let mut s = state.lock().unwrap();
    s.log.push(member.clone());
    if path == "/kglobalaccel" {
        match member.as_str() {
            "doRegister" => {
                let id: Vec<String> = msg.read1().ok()?;
                let entry = s
                    .components
                    .entry(id[0].clone())
                    .or_insert_with(|| (id[2].clone(), BTreeMap::new()));
                entry.1.entry(id[1].clone()).or_insert(Act {
                    keys: vec![],
                    present: false,
                    fresh: true,
                    friendly: id[3].clone(),
                });
                Some(msg.method_return())
            }
            "setShortcutKeys" => {
                let (id, keys, flags): (Vec<String>, Vec<(Vec<i32>,)>, u32) = msg.read3().ok()?;
                s.log.push(format!("setShortcutKeys flags={flags}"));
                if flags == 2 && s.fail_regrab {
                    return Some(msg.method_return().append1(Vec::<(Vec<i32>,)>::new()));
                }
                let Some(act) = s
                    .components
                    .get_mut(&id[0])
                    .and_then(|c| c.1.get_mut(&id[1]))
                else {
                    return Some(msg.method_return().append1(Vec::<(Vec<i32>,)>::new()));
                };
                let set_present = flags & 2 != 0;
                let autoload = flags & 4 == 0;
                if !autoload || act.fresh {
                    act.keys = keys.into_iter().map(|k| k.0).collect();
                    act.fresh = false;
                }
                if set_present {
                    act.present = true;
                }
                let out: Vec<(Vec<i32>,)> = act.keys.iter().map(|k| (k.clone(),)).collect();
                Some(msg.method_return().append1(out))
            }
            "setInactive" => {
                let id: Vec<String> = msg.read1().ok()?;
                if let Some(act) = s
                    .components
                    .get_mut(&id[0])
                    .and_then(|c| c.1.get_mut(&id[1]))
                {
                    act.present = false;
                }
                Some(msg.method_return())
            }
            "unregister" => {
                let (component, action): (String, String) = msg.read2().ok()?;
                let removed = s
                    .components
                    .get_mut(&component)
                    .is_some_and(|c| c.1.remove(&action).is_some());
                if s.components.get(&component).is_some_and(|c| c.1.is_empty()) {
                    s.components.remove(&component);
                }
                Some(msg.method_return().append1(removed))
            }
            "globalShortcutsByKey" => {
                let ((key,), (match_type,)): ((Vec<i32>,), (i32,)) = msg.read2().ok()?;
                type Info = (
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                    Vec<i32>,
                    Vec<i32>,
                );
                let mut out: Vec<Info> = Vec::new();
                let key = trim(&key);
                {
                    for (c, (cf, acts)) in &s.components {
                        for (name, a) in acts {
                            if a.keys.iter().any(|k| by_key(match_type, &key, &trim(k))) {
                                out.push((
                                    name.clone(),
                                    a.friendly.clone(),
                                    c.clone(),
                                    cf.clone(),
                                    "default".into(),
                                    "Default Context".into(),
                                    vec![ESC],
                                    vec![],
                                ));
                            }
                        }
                    }
                }
                Some(msg.method_return().append1(out))
            }
            "getComponent" => {
                let component: String = msg.read1().ok()?;
                Some(if s.components.contains_key(&component) {
                    msg.method_return()
                        .append1(dbus::Path::new(path_of(&component)).unwrap())
                } else {
                    err(
                        msg,
                        "org.kde.kglobalaccel.NoSuchComponent",
                        "The component doesn't exist.",
                    )
                })
            }
            _ => Some(err(
                msg,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "unknown",
            )),
        }
    } else if let Some(component) = s.components.keys().find(|c| path_of(c) == path).cloned() {
        match member.as_str() {
            "shortcutNames" => {
                let names: Vec<String> = s.components[&component].1.keys().cloned().collect();
                Some(msg.method_return().append1(names))
            }
            "cleanUp" => {
                let acts = &mut s.components.get_mut(&component).unwrap().1;
                let before = acts.len();
                acts.retain(|_, a| a.present);
                let changed = acts.len() != before;
                if acts.is_empty() {
                    s.components.remove(&component);
                }
                Some(msg.method_return().append1(changed))
            }
            _ => Some(err(
                msg,
                "org.freedesktop.DBus.Error.UnknownMethod",
                "unknown",
            )),
        }
    } else {
        Some(err(
            msg,
            "org.freedesktop.DBus.Error.UnknownObject",
            "no such object",
        ))
    }
}

fn count(log: &[String], what: &str) -> usize {
    log.iter().filter(|l| *l == what).count()
}

#[test]
fn arm_registers_once_press_aborts_and_eof_removes_the_component() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    assert!(!mock.press(), "nothing grabbed before arming");
    let mut w = bus.watch();
    w.expect_ready();
    let log = mock.log();
    assert_eq!(count(&log, "doRegister"), 1, "{log:?}");
    assert!(
        log.contains(&"setShortcutKeys flags=6".to_owned()),
        "{log:?}"
    );
    {
        let state = mock.state.lock().unwrap();
        let (friendly, actions) = &state.components[COMPONENT];
        assert_eq!(friendly, "Claude: stop Computer Use");
        let act = &actions[ACTION];
        assert_eq!(
            act.keys,
            vec![vec![ESC, 0, 0, 0]],
            "bare Escape, (ai) padded"
        );
        assert!(act.present);
    }

    assert!(mock.press(), "Escape is grabbed");
    w.expect(r#"{"event":"abort"}"#);
    assert!(mock.press());
    w.expect(r#"{"event":"abort"}"#);

    w.close();
    assert_eq!(w.wait(), Some(0));
    assert!(!mock.has_component(), "component removed");
    assert!(!mock.press(), "grab released");
}

#[test]
fn suspend_releases_and_resume_regrabs_after_the_settle_window() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();

    w.send("suspend");
    w.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    assert!(!mock.press(), "the model's Escape reaches the app");
    w.quiet(300);

    let sent = Instant::now();
    w.send("resume");
    // Inside the settle window: still released, still muted.
    std::thread::sleep(Duration::from_millis(40));
    assert!(!mock.press(), "a late injected Escape is not grabbed");
    w.expect(r#"{"event":"ack","cmd":"resume"}"#);
    assert!(
        sent.elapsed() >= Duration::from_millis(150),
        "resume acked after {:?}",
        sent.elapsed()
    );
    assert!(mock.log().contains(&"setShortcutKeys flags=2".to_owned()));
    w.quiet(200);

    assert!(mock.press(), "re-grabbed");
    w.expect(r#"{"event":"abort"}"#);
    w.close();
    assert_eq!(w.wait(), Some(0));
    assert!(!mock.has_component());
}

#[test]
fn a_foreign_escape_shortcut_refuses_to_arm_with_exit_2() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    mock.add_foreign_escape();
    let mut w = bus.watch();
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(
        line.starts_with(
            r#"{"event":"error","message":"Escape is already used by a global shortcut (Example: Cancel)"#
        ),
        "{line}"
    );
    assert_eq!(w.wait(), Some(2));
    let log = mock.log();
    assert_eq!(count(&log, "doRegister"), 0, "{log:?}");
    assert!(!mock.has_component());
}

#[test]
fn sigterm_unregisters_and_exits_0() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    assert!(mock.has_component());
    unsafe { libc::kill(w.child.id() as i32, libc::SIGTERM) };
    assert_eq!(w.wait(), Some(0));
    assert!(!mock.has_component());
    assert!(!mock.press());
}

#[test]
fn two_watchers_share_one_registration_and_the_last_one_removes_it() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut a = bus.watch();
    a.expect_ready();
    let mut b = bus.watch();
    b.expect_ready();
    assert_eq!(count(&mock.log(), "doRegister"), 1, "one registration");

    assert!(mock.press());
    a.expect(r#"{"event":"abort"}"#);
    b.expect(r#"{"event":"abort"}"#);

    a.close();
    assert_eq!(a.wait(), Some(0));
    assert!(mock.has_component(), "b still holds it");
    assert!(mock.press());
    b.expect(r#"{"event":"abort"}"#);

    b.close();
    assert_eq!(b.wait(), Some(0));
    assert!(!mock.has_component());
}

#[test]
fn a_sigkilled_watchers_registration_is_cleaned_by_the_next_one() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut a = bus.watch();
    a.expect_ready();
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(mock.press(), "a SIGKILLed watcher leaves its grab (known)");
    mock.log();

    let mut b = bus.watch();
    b.expect_ready();
    let log = mock.log();
    let cleanup = log.iter().position(|l| l == "cleanUp").expect("cleanUp");
    let register = log
        .iter()
        .position(|l| l == "doRegister")
        .expect("doRegister");
    assert!(log.contains(&"setInactive".to_owned()), "{log:?}");
    assert!(cleanup < register, "stale entry removed first: {log:?}");

    b.close();
    assert_eq!(b.wait(), Some(0));
    assert!(!mock.has_component());
    assert!(!mock.press());
}

#[test]
fn kglobalaccel_leaving_the_bus_fails_with_exit_1() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    mock.quit();
    w.expect(r#"{"event":"error","message":"kglobalaccel left the session bus"}"#);
    assert_eq!(w.wait(), Some(1));
}

#[test]
fn the_bus_going_away_fails_with_exit_1() {
    if !dbus_daemon_available() {
        return;
    }
    let mut bus = Bus::start();
    let _mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    bus.kill();
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(line.starts_with(r#"{"event":"error","message":"#), "{line}");
    assert_eq!(w.wait(), Some(1));
}

#[test]
fn without_kglobalaccel_arming_fails_with_exit_2_and_starts_nothing() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mut w = bus.watch();
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(
        line.contains("org.kde.kglobalaccel is not on the session bus"),
        "{line}"
    );
    assert_eq!(w.wait(), Some(2));
}

#[test]
fn a_foreign_sequence_containing_escape_refuses_to_arm() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    // `Esc, X` (a vi-style chord): a bare Escape grab would break it.
    mock.add_foreign(vec![ESC, 0x58, 0, 0]);
    let mut w = bus.watch();
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(line.contains("Example: Cancel"), "{line}");
    assert_eq!(w.wait(), Some(2));
    assert!(!mock.has_component());
}

#[test]
fn a_stale_grab_is_removed_even_when_a_foreign_shortcut_then_refuses() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut a = bus.watch();
    a.expect_ready();
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(mock.press(), "SIGKILL left the grab");
    mock.add_foreign_escape();
    let mut b = bus.watch();
    let line = b.line(Duration::from_secs(5)).unwrap();
    assert!(line.contains("Example: Cancel"), "{line}");
    assert_eq!(b.wait(), Some(2));
    assert!(!mock.has_component(), "stale grab removed");
}

#[test]
fn two_session_buses_of_one_user_keep_separate_registrations() {
    if !dbus_daemon_available() {
        return;
    }
    let bus1 = Bus::start();
    let bus2 = Bus::start();
    let mock1 = Mock::start(&bus1);
    let mock2 = Mock::start(&bus2);
    let runtime = bus1.runtime_dir();
    let mut a = bus1.watch_in(&runtime);
    a.expect_ready();
    let mut b = bus2.watch_in(&runtime);
    b.expect_ready();
    assert_eq!(count(&mock1.log(), "doRegister"), 1);
    assert_eq!(count(&mock2.log(), "doRegister"), 1);

    a.close();
    assert_eq!(a.wait(), Some(0));
    assert!(!mock1.has_component(), "last holder on bus 1 removed it");
    assert!(mock2.has_component());
    assert!(mock2.press());
    b.expect(r#"{"event":"abort"}"#);
    b.close();
    assert_eq!(b.wait(), Some(0));
    assert!(!mock2.has_component());
}

#[test]
fn a_failed_resume_ends_the_watcher_with_exit_1() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    w.send("suspend");
    w.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    mock.fail_regrab();
    w.send("resume");
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(line.starts_with(r#"{"event":"error""#), "{line}");
    assert!(line.contains("gone"), "{line}");
    assert_eq!(w.wait(), Some(1));
    assert!(!mock.has_component(), "disarmed after the failure");
}

#[test]
fn kglobalaccel_restarting_under_a_new_owner_fails_with_exit_1() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let _old = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    // A new kglobalaccel takes the name over (new unique name, empty state).
    let _new = Mock::start(&bus);
    w.expect(
        r#"{"event":"error","message":"kglobalaccel was restarted: the Escape shortcut is gone"}"#,
    );
    assert_eq!(w.wait(), Some(1));
}

#[test]
fn interleaved_suspends_of_two_watchers_regrab_only_after_the_last_resume() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut a = bus.watch();
    a.expect_ready();
    let mut b = bus.watch();
    b.expect_ready();

    a.send("suspend");
    a.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    b.send("suspend");
    b.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    a.send("resume");
    a.expect(r#"{"event":"ack","cmd":"resume"}"#);
    assert!(!mock.press(), "b still injects: Escape stays released");
    a.quiet(200);

    b.send("resume");
    b.expect(r#"{"event":"ack","cmd":"resume"}"#);
    assert!(mock.press(), "grabbed again after the last resume");
    a.expect(r#"{"event":"abort"}"#);
    b.expect(r#"{"event":"abort"}"#);
}

#[test]
fn a_suspended_watcher_that_dies_does_not_leave_escape_released() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let a = bus.watch();
    a.expect_ready();
    let mut b = bus.watch();
    b.expect_ready();
    b.send("suspend");
    b.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    assert!(!mock.press());
    b.child.kill().unwrap();
    b.child.wait().unwrap();

    // a's watchdog prunes the dead suspender within a tick or two.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !mock.press() {
        assert!(Instant::now() < deadline, "Escape never grabbed again");
        std::thread::sleep(Duration::from_millis(100));
    }
    a.expect(r#"{"event":"abort"}"#);
}

#[test]
fn a_suspend_without_resume_fails_after_the_limit() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let mut w = bus.watch();
    w.expect_ready();
    let sent = Instant::now();
    w.send("suspend");
    w.expect(r#"{"event":"ack","cmd":"suspend"}"#);
    let line = w.line(Duration::from_secs(8)).unwrap();
    assert!(
        line.contains("suspended for more than 5 s without resume"),
        "{line}"
    );
    assert!(sent.elapsed() >= Duration::from_secs(5));
    assert_eq!(w.wait(), Some(1));
    assert!(!mock.has_component());
}

/// Holder directories under `<runtime>/claude-cu-esc` (empty when absent).
fn holder_dir_names(runtime: &Path) -> Vec<String> {
    std::fs::read_dir(runtime.join("claude-cu-esc"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_failed_arm_leaves_no_holder_directories() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    // No kglobalaccel: refused before anything is created.
    let mut w = bus.watch();
    assert_eq!(w.wait(), Some(2));
    assert!(
        !bus.runtime_dir().join("claude-cu-esc").exists(),
        "{:?}",
        holder_dir_names(&bus.runtime_dir())
    );

    // A conflict is found under the lock: the directories are removed again.
    let mock = Mock::start(&bus);
    mock.add_foreign_escape();
    let mut w = bus.watch();
    assert_eq!(w.wait(), Some(2));
    assert_eq!(holder_dir_names(&bus.runtime_dir()), Vec::<String>::new());

    // A successful arm keeps them while running.
    mock.state.lock().unwrap().components.clear();
    let mut w = bus.watch();
    w.expect_ready();
    assert_eq!(holder_dir_names(&bus.runtime_dir()).len(), 2);
    w.close();
    assert_eq!(w.wait(), Some(0));
}

#[test]
fn a_symlinked_holder_parent_refuses_to_arm_with_exit_2() {
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let mock = Mock::start(&bus);
    let elsewhere = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), bus.runtime_dir().join("claude-cu-esc")).unwrap();
    let mut w = bus.watch();
    let line = w.line(Duration::from_secs(5)).unwrap();
    assert!(line.contains("is a symlink"), "{line}");
    assert_eq!(w.wait(), Some(2));
    assert_eq!(count(&mock.log(), "doRegister"), 0);
    assert!(!mock.has_component());
    assert!(
        std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing created through the symlink"
    );
}

#[test]
fn a_group_readable_holder_parent_is_tightened_to_0700() {
    use std::os::unix::fs::PermissionsExt;
    if !dbus_daemon_available() {
        return;
    }
    let bus = Bus::start();
    let _mock = Mock::start(&bus);
    let parent = bus.runtime_dir().join("claude-cu-esc");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut w = bus.watch();
    w.expect_ready();
    let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    w.close();
    assert_eq!(w.wait(), Some(0));
}
