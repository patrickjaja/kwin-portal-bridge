use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use dbus::blocking::{Connection, SyncConnection};
use dbus::channel::MatchingReceiver;
use dbus::message::MatchRule;
use serde::de::DeserializeOwned;

use crate::model::{
    CursorPosition, DoctorReport, ExcludeUpdate, Rect, ScreenInfo, ToolPresence,
    WindowControlResult, WindowInfo,
};
use crate::util;

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const SCRIPT_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SCRIPT_OUTPUT_MAX_POLLS: usize = 20;
const BRIDGE_PATH: &str = "/Bridge";
const BRIDGE_INTERFACE: &str = "org.kde.KWinPortalBridge";

pub struct KWinBackend;

impl KWinBackend {
    pub fn new() -> Self {
        Self
    }

    pub fn doctor(&self) -> Result<DoctorReport> {
        let tools = ["qdbus6", "gdbus", "kwin_wayland", "xdg-desktop-portal"]
            .into_iter()
            .map(command_presence)
            .collect::<Result<Vec<_>>>()?;

        Ok(DoctorReport { tools })
    }

    pub fn list_screens(&self) -> Result<Vec<ScreenInfo>> {
        let payload = run_json_script("kwin-portal-bridge-screens", SCRIPTS.screens)?;
        parse_payload(payload)
    }

    pub fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let payload = run_json_script("kwin-portal-bridge-windows", SCRIPTS.windows)?;
        parse_payload(payload)
    }

    pub fn cursor_position(&self) -> Result<CursorPosition> {
        let payload = run_json_script("kwin-portal-bridge-cursor", SCRIPTS.cursor)?;
        parse_payload(payload)
    }

    pub fn set_exclude_from_capture(
        &self,
        windows: &[String],
        value: bool,
    ) -> Result<ExcludeUpdate> {
        if windows.is_empty() {
            bail!("at least one --window must be provided");
        }

        let args_json = serde_json::to_string(windows)?;
        let script = format!(
            "{}\nconst TARGET_WINDOWS = {args_json};\nconst TARGET_VALUE = {};\n{}",
            SCRIPT_HEADER, value, SCRIPT_SET_EXCLUDE
        );

        let payload = run_json_script("kwin-portal-bridge-exclude", &script)?;
        let updated: ExcludeUpdate = parse_payload(payload)?;

        if updated.windows.len() != windows.len() {
            bail!(
                "KWin updated {} window(s), but {} were requested",
                updated.windows.len(),
                windows.len()
            );
        }

        Ok(updated)
    }

    pub fn activate_window(&self, window_id: &str) -> Result<()> {
        let script = format!(
            "{}\nconst TARGET_WINDOW = {:?};\n{}",
            SCRIPT_HEADER, window_id, SCRIPT_ACTIVATE_WINDOW
        );

        run_script("kwin-portal-bridge-activate", &script, false)?;

        let activated = self
            .list_windows()?
            .into_iter()
            .find(|window| window.is_active)
            .ok_or_else(|| anyhow!("KWin did not report an active window after activation"))?;

        if activated.id != window_id && !activated.is_transient_for_window(window_id) {
            bail!(
                "KWin activated `{}`, but `{window_id}` was requested",
                activated.id
            );
        }

        Ok(())
    }

    pub fn set_window_geometry(
        &self,
        window_id: &str,
        geometry: &Rect,
    ) -> Result<WindowControlResult> {
        let geometry_json = serde_json::to_string(geometry)?;
        let script = format!(
            "{}\nconst TARGET_WINDOW = {:?};\nconst TARGET_GEOMETRY = {};\n{}",
            SCRIPT_HEADER, window_id, geometry_json, SCRIPTS.set_window_geometry
        );

        let payload = run_json_script("kwin-portal-bridge-set-window-geometry", &script)?;
        parse_payload(payload)
    }

    pub fn set_window_keep_above(
        &self,
        window_id: &str,
        value: bool,
    ) -> Result<WindowControlResult> {
        let script = format!(
            "{}\nconst TARGET_WINDOW = {:?};\nconst TARGET_VALUE = {};\n{}",
            SCRIPT_HEADER, window_id, value, SCRIPTS.set_window_keep_above
        );

        let payload = run_json_script("kwin-portal-bridge-set-window-keep-above", &script)?;
        parse_payload(payload)
    }
}

fn parse_payload<T: DeserializeOwned>(payload: String) -> Result<T> {
    serde_json::from_str(&payload).context("failed to decode KWin JSON payload")
}

fn command_presence(command: &str) -> Result<ToolPresence> {
    let output = Command::new("which")
        .arg(command)
        .output()
        .with_context(|| format!("failed to probe `{command}` with `which`"))?;

    let available = output.status.success();
    let path = if available {
        Some(String::from_utf8(output.stdout)?.trim().to_owned())
    } else {
        None
    };

    Ok(ToolPresence {
        command: command.to_owned(),
        available,
        path,
    })
}

fn run_json_script(script_name: &str, script_body: &str) -> Result<String> {
    let payload = run_script(script_name, script_body, true)?;
    payload.ok_or_else(|| anyhow!("KWin script finished without a result payload"))
}

fn run_script(
    script_name: &str,
    script_body: &str,
    require_result: bool,
) -> Result<Option<String>> {
    let kwin_conn =
        Connection::new_session().context("failed to connect to the session bus for KWin")?;
    let kwin_proxy = kwin_conn.with_proxy("org.kde.KWin", "/Scripting", DBUS_TIMEOUT);

    let receiver_conn =
        SyncConnection::new_session().context("failed to create a session bus receiver")?;
    let dbus_addr = receiver_conn.unique_name().to_string();
    let messages = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let message_sink = Arc::clone(&messages);

    let _receiver = receiver_conn.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |message, _connection| {
            if let Some(member) = message.member()
                && let Some(arg) = message.get1::<String>()
                && let Ok(mut guard) = message_sink.lock()
            {
                guard.push((member.to_string(), arg));
            }
            true
        }),
    );

    let mut script_file = tempfile::NamedTempFile::with_prefix("kwin-portal-bridge-")?;
    script_file.write_all(render_script(&dbus_addr, script_body).as_bytes())?;
    let script_path = script_file.into_temp_path();

    let unique_name = format!("{script_name}-{}", unique_suffix());
    let (script_id,): (i32,) = kwin_proxy
        .method_call(
            "org.kde.kwin.Scripting",
            "loadScript",
            (script_path.to_str().unwrap(), unique_name),
        )
        .context("failed to load the temporary KWin script")?;

    if script_id < 0 {
        bail!("KWin refused to load script `{script_name}`");
    }

    let script_proxy = kwin_conn.with_proxy(
        "org.kde.KWin",
        format!("/Scripting/Script{script_id}"),
        DBUS_TIMEOUT,
    );

    let _: () = script_proxy
        .method_call("org.kde.kwin.Script", "run", ())
        .context("failed to run the KWin script")?;

    for _ in 0..SCRIPT_OUTPUT_MAX_POLLS {
        receiver_conn
            .process(SCRIPT_OUTPUT_POLL_INTERVAL)
            .context("failed while waiting for KWin script output")?;

        if messages_include_terminal_event(&messages)? {
            break;
        }
    }

    if let Err(error) = script_proxy.method_call::<(), _, _, _>("org.kde.kwin.Script", "stop", ()) {
        let message = format!("{error:#}");
        if !message.contains("No such object path") {
            return Err(error).context("failed to stop the KWin script");
        }
    }

    let received = messages
        .lock()
        .map_err(|_| anyhow!("message receiver lock poisoned"))?;

    if let Some((_, error)) = received.iter().find(|(kind, _)| kind == "error") {
        bail!("KWin script error: {error}");
    }

    let payload = received
        .iter()
        .find(|(kind, _)| kind == "result")
        .map(|(_, payload)| payload.clone());

    if require_result && payload.is_none() {
        bail!("KWin script finished without a result payload");
    }

    Ok(payload)
}

fn messages_include_terminal_event(messages: &Arc<Mutex<Vec<(String, String)>>>) -> Result<bool> {
    let received = messages
        .lock()
        .map_err(|_| anyhow!("message receiver lock poisoned"))?;

    Ok(received
        .iter()
        .any(|(kind, _)| matches!(kind.as_str(), "result" | "error")))
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros())
        .unwrap_or_default()
}

fn render_script(dbus_addr: &str, script_body: &str) -> String {
    let overlay_names_json =
        serde_json::to_string(util::bridge_overlay_names()).unwrap_or_else(|_| "[]".to_owned());
    format!(
        "{SCRIPT_HEADER}\nconst DBUS_DESTINATION = {dbus_addr:?};\nconst BRIDGE_PATH = {BRIDGE_PATH:?};\nconst BRIDGE_INTERFACE = {BRIDGE_INTERFACE:?};\nconst BRIDGE_OVERLAY_NAMES = {overlay_names_json};\n{script_body}\n"
    )
}

const SCRIPT_HEADER: &str = r#"
function bridgeRect(geometry) {
    return {
        x: Math.round(geometry.x),
        y: Math.round(geometry.y),
        width: Math.round(geometry.width),
        height: Math.round(geometry.height)
    };
}

function bridgeEmit(kind, payload) {
    callDBus(
        DBUS_DESTINATION,
        BRIDGE_PATH,
        BRIDGE_INTERFACE,
        kind,
        JSON.stringify(payload)
    );
}

function bridgeResult(payload) {
    bridgeEmit("result", payload);
}

function bridgeError(message) {
    bridgeEmit("error", { message: message });
}

function bridgeIsOverlayWindow(window) {
    if (!window) {
        return false;
    }
    const candidates = [window.resourceClass, window.resourceName, window.desktopFileName];
    for (let i = 0; i < candidates.length; i++) {
        const raw = candidates[i];
        if (!raw) {
            continue;
        }
        let normalized = String(raw).trim().toLowerCase();
        if (normalized.endsWith(".desktop")) {
            normalized = normalized.slice(0, -8);
        }
        if (BRIDGE_OVERLAY_NAMES.indexOf(normalized) !== -1) {
            return true;
        }
    }
    return false;
}

function bridgeWindowAppRef(window, seenIds, depth) {
    if (!window || depth <= 0) {
        return null;
    }

    const id = String(window.internalId || "");
    if (id && seenIds.includes(id)) {
        return null;
    }

    const nextSeenIds = id ? [...seenIds, id] : seenIds;
    return {
        id,
        desktop_file_name: window.desktopFileName || null,
        resource_class: window.resourceClass || null,
        resource_name: window.resourceName || null,
        transient: typeof window.transient === "boolean" ? window.transient : null,
        transient_for: bridgeWindowAppRef(window.transientFor, nextSeenIds, depth - 1)
    };
}
"#;

struct Scripts<'a> {
    screens: &'a str,
    windows: &'a str,
    cursor: &'a str,
    set_window_geometry: &'a str,
    set_window_keep_above: &'a str,
}

const SCRIPTS: Scripts<'static> = Scripts {
    screens: SCRIPT_SCREENS,
    windows: SCRIPT_WINDOWS,
    cursor: SCRIPT_CURSOR,
    set_window_geometry: SCRIPT_SET_WINDOW_GEOMETRY,
    set_window_keep_above: SCRIPT_SET_WINDOW_KEEP_ABOVE,
};

const SCRIPT_SCREENS: &str = r#"
try {
    function toArray(value) {
        if (!value) {
            return [];
        }

        if (Array.isArray(value)) {
            return value;
        }

        if (typeof value.length === "number") {
            try {
                return Array.from(value);
            } catch (_error) {
                const items = [];
                for (let i = 0; i < value.length; i++) {
                    items.push(value[i]);
                }
                return items;
            }
        }

        return [];
    }

    function sameScreen(a, b) {
        if (!a || !b) {
            return false;
        }

        if (a === b) {
            return true;
        }

        if (a.name && b.name && a.name === b.name) {
            return true;
        }

        const ag = a.geometry;
        const bg = b.geometry;
        return !!ag && !!bg &&
            ag.x === bg.x &&
            ag.y === bg.y &&
            ag.width === bg.width &&
            ag.height === bg.height;
    }

    const orderedScreens = toArray(workspace.screenOrder);
    const primaryScreen = orderedScreens.length > 0 ? orderedScreens[0] : null;
    const screens = workspace.screens.map((screen, index) => ({
        id: screen.name || `screen-${index}`,
        name: screen.name || `Screen ${index + 1}`,
        geometry: bridgeRect(screen.geometry),
        scale: typeof screen.devicePixelRatio === "number" ? screen.devicePixelRatio : screen.scale,
        refresh_millihz: screen.refreshRate,
        is_active: workspace.activeScreen === screen,
        is_primary: primaryScreen ? sameScreen(primaryScreen, screen) : index === 0
    }));
    bridgeResult(screens);
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_WINDOWS: &str = r#"
try {
    const windows = workspace.windowList().map((window, index) => {
        if (bridgeIsOverlayWindow(window) && !window.excludeFromCapture) {
            window.excludeFromCapture = true;
        }
        return {
        id: String(window.internalId),
        title: window.caption || "",
        geometry: bridgeRect(window.frameGeometry),
        pid: window.pid || null,
        desktop_file_name: window.desktopFileName || null,
        resource_class: window.resourceClass || null,
        resource_name: window.resourceName || null,
        window_role: window.windowRole || null,
        window_type: window.windowType ? String(window.windowType) : null,
        is_dock: typeof window.dock === "boolean" ? window.dock : null,
        is_desktop: typeof window.desktopWindow === "boolean" ? window.desktopWindow : null,
        is_visible: typeof window.visible === "boolean" ? window.visible : null,
        is_minimized: typeof window.minimized === "boolean" ? window.minimized : null,
        is_normal_window: typeof window.normalWindow === "boolean" ? window.normalWindow : null,
        is_dialog: typeof window.dialog === "boolean" ? window.dialog : null,
        transient: typeof window.transient === "boolean" ? window.transient : null,
        transient_for: bridgeWindowAppRef(window.transientFor, [String(window.internalId)], 8),
        output: window.output ? window.output.name : null,
        stacking_order: typeof window.stackingOrder === "number" ? window.stackingOrder : index,
        is_active: workspace.activeWindow === window,
        exclude_from_capture: !!window.excludeFromCapture,
        keep_above: typeof window.keepAbove === "boolean" ? window.keepAbove : null
        };
    });
    bridgeResult(windows);
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_CURSOR: &str = r#"
try {
    const pos =
        (typeof workspace.cursorPos === "object" && workspace.cursorPos) ||
        (typeof workspace.cursorPosition === "object" && workspace.cursorPosition);
    if (!pos) {
        throw new Error("KWin did not expose workspace.cursorPos");
    }
    bridgeResult({
        x: Math.round(pos.x),
        y: Math.round(pos.y)
    });
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_SET_EXCLUDE: &str = r#"
try {
    const changed = [];
    workspace.windowList().forEach((window) => {
        const id = String(window.internalId);
        if (TARGET_WINDOWS.indexOf(id) !== -1) {
            window.excludeFromCapture = TARGET_VALUE;
            changed.push(id);
        }
    });
    bridgeResult({
        windows: changed,
        value: TARGET_VALUE
    });
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_ACTIVATE_WINDOW: &str = r#"
try {
    let target = null;
    workspace.windowList().forEach((window) => {
        if (String(window.internalId) === TARGET_WINDOW) {
            target = window;
        }
    });

    if (!target) {
        throw new Error(`No window found for id ${TARGET_WINDOW}`);
    }

    workspace.activeWindow = target;
    bridgeResult({ activated: TARGET_WINDOW });
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_SET_WINDOW_GEOMETRY: &str = r#"
try {
    let target = null;
    workspace.windowList().forEach((window) => {
        if (String(window.internalId) === TARGET_WINDOW) {
            target = window;
        }
    });

    if (!target) {
        throw new Error(`No window found for id ${TARGET_WINDOW}`);
    }

    const nextGeometry = Object.assign({}, target.frameGeometry);
    nextGeometry.x = TARGET_GEOMETRY.x;
    nextGeometry.y = TARGET_GEOMETRY.y;
    nextGeometry.width = TARGET_GEOMETRY.width;
    nextGeometry.height = TARGET_GEOMETRY.height;
    target.frameGeometry = nextGeometry;

    bridgeResult({
        windowId: TARGET_WINDOW,
        geometry: bridgeRect(target.frameGeometry),
        keepAbove: !!target.keepAbove
    });
} catch (error) {
    bridgeError(String(error));
}
"#;

const SCRIPT_SET_WINDOW_KEEP_ABOVE: &str = r#"
try {
    let target = null;
    workspace.windowList().forEach((window) => {
        if (String(window.internalId) === TARGET_WINDOW) {
            target = window;
        }
    });

    if (!target) {
        throw new Error(`No window found for id ${TARGET_WINDOW}`);
    }

    target.keepAbove = TARGET_VALUE;

    bridgeResult({
        windowId: TARGET_WINDOW,
        geometry: bridgeRect(target.frameGeometry),
        keepAbove: !!target.keepAbove
    });
} catch (error) {
    bridgeError(String(error));
}
"#;
