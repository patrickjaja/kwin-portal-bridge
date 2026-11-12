//! `overlay`: Claude's Computer Use glow and drawn cursor through the shared
//! cu-overlay crate (protocol 1, JSON lines on stdin / stdout), plus this
//! bridge's KDE part: the overlay's own surfaces never reach a screen capture.
//!
//! cu-overlay owns stdin, stdout (protocol lines only), the parent-death
//! watch and the exit code. It runs on the main thread before any tokio
//! runtime exists. Next to it, a helper thread loads a KWin script for the
//! lifetime of this process that sets `excludeFromCapture` on this process's
//! layer surfaces, existing and newly added (the surface is created at `show`
//! and destroyed at `hide`). The helper never writes to stdout and never
//! fails the overlay: without KWin scripting it leaves one stderr line.
//!
//! How KWin shows the surface to scripts (KWin 6.7.2, measured live): caption
//! and desktopFileName empty, the layer-shell namespace not exposed at all,
//! resourceClass / resourceName = the client executable's basename (KWin's
//! `WaylandWindow::updateResourceName`), `pid` = this process, `layer` = 9
//! (overlay layer), `normalWindow` true and windowType 0. So the script
//! matches `window.pid` = this process AND (a bridge name in resourceClass /
//! resourceName / desktopFileName, or `layer` 9); see [`exclusion_script`].
//! The second condition keeps a script left over by a crashed run harmless
//! should its pid be reused by an unrelated program.
//!
//! Lifetime: the script is unloaded when cu-overlay returns. A process killed
//! by a signal (SIGTERM from the parent-death watch, SIGKILL) cannot unload
//! it; every start therefore sweeps the scripts whose process is gone, from
//! the records in `$XDG_RUNTIME_DIR/kwin-portal-bridge/cu-overlay/`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use dbus::blocking::Connection;

/// `window.layer` of a layer-shell surface on the overlay layer
/// (`KWin::Layer::OverlayLayer`).
const KWIN_OVERLAY_LAYER: u32 = 9;

/// Per D-Bus call; the whole helper is off the protocol path anyway.
const DBUS_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the exit waits for the helper to unload the script.
const UNLOAD_WAIT: Duration = Duration::from_millis(1500);

const PLUGIN_PREFIX: &str = "kwin-portal-bridge-cu-overlay-";

/// `kwin-portal-bridge overlay`.
pub fn run() -> ExitCode {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let helper = std::thread::Builder::new()
        .name("overlay-exclude".to_owned())
        .spawn(move || {
            let guard = match ExclusionGuard::install() {
                Ok(guard) => Some(guard),
                Err(error) => {
                    eprintln!(
                        "[kwin-portal-bridge overlay] capture exclusion unavailable: {error:#}"
                    );
                    None
                }
            };
            // Until the overlay returns (the sender is dropped).
            let _ = stop_rx.recv();
            if let Some(guard) = guard {
                guard.remove();
            }
            let _ = done_tx.send(());
        });
    if let Err(error) = &helper {
        eprintln!("[kwin-portal-bridge overlay] capture exclusion helper not started: {error}");
    }

    let code = cu_overlay::run(std::io::stdin(), std::io::stdout());

    drop(stop_tx);
    if helper.is_ok() {
        let _ = done_rx.recv_timeout(UNLOAD_WAIT);
    }
    code
}

/// A loaded exclusion script; [`ExclusionGuard::remove`] unloads it.
struct ExclusionGuard {
    conn: Connection,
    plugin: String,
    file: PathBuf,
}

impl ExclusionGuard {
    fn install() -> Result<Self> {
        let pid = std::process::id();
        let start = proc_start_time(pid).context("cannot read this process's start time")?;
        let dir = record_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create `{}`", dir.display()))?;
        let conn = Connection::new_session().context("no session bus")?;
        sweep_stale(&dir, |plugin| unload(&conn, plugin));

        let record = Record { pid, start };
        let file = dir.join(record.file_name());
        let script = exclusion_script(pid, crate::util::bridge_overlay_names());
        std::fs::write(&file, script)
            .with_context(|| format!("cannot write `{}`", file.display()))?;
        let plugin = record.plugin();
        let loaded = load_and_run(&conn, &file, &plugin);
        if let Err(error) = loaded {
            std::fs::remove_file(&file).ok();
            return Err(error);
        }
        Ok(Self { conn, plugin, file })
    }

    fn remove(self) {
        unload(&self.conn, &self.plugin);
        std::fs::remove_file(&self.file).ok();
    }
}

fn load_and_run(conn: &Connection, file: &Path, plugin: &str) -> Result<()> {
    let scripting = conn.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);
    // A script of this name can only be left over from a process that had
    // this pid and start time, i.e. never; unload defensively anyway.
    unload(conn, plugin);
    let (id,): (i32,) = scripting
        .method_call(
            "org.kde.kwin.Scripting",
            "loadScript",
            (
                file.to_str().context("script path is not valid UTF-8")?,
                plugin,
            ),
        )
        .context("KWin loadScript failed")?;
    if id < 0 {
        bail!("KWin refused to load `{plugin}`");
    }
    conn.with_proxy(
        "org.kde.KWin",
        format!("/Scripting/Script{id}"),
        DBUS_TIMEOUT,
    )
    .method_call::<(), _, _, _>("org.kde.kwin.Script", "run", ())
    .context("KWin Script.run failed")?;
    Ok(())
}

fn unload(conn: &Connection, plugin: &str) {
    let _: Result<(bool,), _> = conn
        .with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT)
        .method_call("org.kde.kwin.Scripting", "unloadScript", (plugin,));
}

/// Unload the scripts of overlay processes that are gone and drop their
/// records. A record whose process still runs (another profile's overlay)
/// is left alone.
fn sweep_stale(dir: &Path, unload: impl Fn(&str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(record) = name.to_str().and_then(Record::parse) else {
            continue;
        };
        if proc_start_time(record.pid) == Some(record.start) {
            continue;
        }
        unload(&record.plugin());
        std::fs::remove_file(entry.path()).ok();
    }
}

/// `$XDG_RUNTIME_DIR/kwin-portal-bridge/cu-overlay`.
fn record_dir() -> PathBuf {
    crate::util::runtime_base()
        .join("kwin-portal-bridge")
        .join("cu-overlay")
}

/// One overlay process: its pid and start time (pids get reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    pid: u32,
    start: u64,
}

impl Record {
    fn file_name(self) -> String {
        format!("{}-{}.js", self.pid, self.start)
    }

    fn plugin(self) -> String {
        format!("{PLUGIN_PREFIX}{}-{}", self.pid, self.start)
    }

    fn parse(file_name: &str) -> Option<Self> {
        let (pid, start) = file_name.strip_suffix(".js")?.split_once('-')?;
        let pid: u32 = pid.parse().ok()?;
        (pid > 0).then_some(())?;
        Some(Self {
            pid,
            start: start.parse().ok()?,
        })
    }
}

/// Start time (field 22 of `/proc/<pid>/stat`) of a running, non-zombie
/// process.
fn proc_start_time(pid: u32) -> Option<u64> {
    parse_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Fields counted after the last `)`: comm may contain spaces and
/// parentheses.
fn parse_start_time(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    // state is field 3; starttime is field 22.
    fields.nth(18)?.parse().ok()
}

/// The KWin script that keeps this process's surfaces out of captures.
/// Self-contained (no D-Bus callback): it runs until unloaded.
pub fn exclusion_script(pid: u32, names: &[String]) -> String {
    let names_json = serde_json::to_string(names)
        .unwrap_or_else(|_| "[]".to_owned())
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    format!(
        "const CU_OVERLAY_PID = {pid};\nconst CU_OVERLAY_NAMES = {names_json};\nconst CU_OVERLAY_LAYER = {KWIN_OVERLAY_LAYER};\n{EXCLUSION_SCRIPT_BODY}"
    )
}

const EXCLUSION_SCRIPT_BODY: &str = r#"
function cuOverlayIsOurs(window) {
    if (!window || Number(window.pid) !== CU_OVERLAY_PID) {
        return false;
    }
    if (Number(window.layer) === CU_OVERLAY_LAYER) {
        return true;
    }
    const candidates = [window.resourceClass, window.resourceName, window.desktopFileName];
    for (let i = 0; i < candidates.length; i++) {
        if (!candidates[i]) {
            continue;
        }
        let name = String(candidates[i]).trim().toLowerCase();
        if (name.endsWith(".desktop")) {
            name = name.slice(0, -8);
        }
        if (CU_OVERLAY_NAMES.indexOf(name) !== -1) {
            return true;
        }
    }
    return false;
}

function cuOverlayExclude(window) {
    try {
        if (cuOverlayIsOurs(window) && !window.excludeFromCapture) {
            window.excludeFromCapture = true;
        }
    } catch (_error) {
    }
}

workspace.windowList().forEach(cuOverlayExclude);
if (workspace.windowAdded) {
    workspace.windowAdded.connect(cuOverlayExclude);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_reject_foreign_files() {
        let record = Record {
            pid: 4242,
            start: 987_654,
        };
        assert_eq!(record.file_name(), "4242-987654.js");
        assert_eq!(record.plugin(), "kwin-portal-bridge-cu-overlay-4242-987654");
        assert_eq!(Record::parse(&record.file_name()), Some(record));
        for foreign in [
            "4242.js", "a-1.js", "1-b.js", "0-5.js", "1-2.txt", "-1-2.js", "",
        ] {
            assert_eq!(Record::parse(foreign), None, "{foreign}");
        }
    }

    #[test]
    fn start_time_parsing() {
        let stat = "900 (kwin (portal) bridge) S 500 900 500 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_start_time(stat), Some(123_456));
        assert_eq!(
            parse_start_time("7 (z) Z 3 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 99 0"),
            None
        );
        assert_eq!(parse_start_time("garbage"), None);
        let own = proc_start_time(std::process::id());
        assert!(own.is_some());
        assert_eq!(own, proc_start_time(std::process::id()));
    }

    #[test]
    fn script_matches_this_pid_and_carries_the_names() {
        let names = vec!["kwin-portal-bridge".to_owned(), "x\u{2028}y".to_owned()];
        let script = exclusion_script(31337, &names);
        assert!(script.starts_with("const CU_OVERLAY_PID = 31337;\n"));
        assert!(
            script.contains("const CU_OVERLAY_NAMES = [\"kwin-portal-bridge\",\"x\\u2028y\"];")
        );
        assert!(script.contains("const CU_OVERLAY_LAYER = 9;"));
        // The pid check comes first: no other process's window is touched.
        let body = &script[script.find("function cuOverlayIsOurs").unwrap()..];
        assert!(body.find("CU_OVERLAY_PID").unwrap() < body.find("CU_OVERLAY_LAYER").unwrap());
        assert!(script.contains("workspace.windowAdded.connect(cuOverlayExclude)"));
        assert!(!script.contains("callDBus"), "no reply channel needed");
    }

    #[test]
    fn sweep_keeps_live_records_and_drops_dead_ones() {
        let dir = tempfile::tempdir().unwrap();
        let me = Record {
            pid: std::process::id(),
            start: proc_start_time(std::process::id()).unwrap(),
        };
        let dead = Record {
            pid: me.pid,
            start: me.start + 1,
        };
        for record in [me, dead] {
            std::fs::write(dir.path().join(record.file_name()), "").unwrap();
        }
        std::fs::write(dir.path().join("unrelated.txt"), "").unwrap();
        let unloaded = std::cell::RefCell::new(Vec::new());
        sweep_stale(dir.path(), |plugin| {
            unloaded.borrow_mut().push(plugin.to_owned())
        });
        assert_eq!(unloaded.into_inner(), vec![dead.plugin()]);
        assert!(dir.path().join(me.file_name()).exists());
        assert!(!dir.path().join(dead.file_name()).exists());
        assert!(dir.path().join("unrelated.txt").exists());
    }
}
