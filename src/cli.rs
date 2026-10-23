use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "kwin-portal-bridge",
    version,
    about = "KWin + portal support bridge for Linux computer-use tooling."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the standalone MCP server on stdio.
    #[cfg(feature = "mcp")]
    Mcp,
    /// Start a long-lived portal session daemon for this tool-use lock.
    SessionStart {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Report the bridge, backend, session and grant state as JSON (CLI contract v1).
    Doctor,
    /// End the long-lived portal session daemon for this tool-use lock.
    SessionEnd,
    /// Forget the remembered screen-sharing choice so the next session shows the consent dialog again.
    ForgetSession,
    /// Enumerate screens through a KWin script.
    Screens,
    /// Enumerate windows through a KWin script.
    Windows,
    /// Report the current global cursor position in logical coordinates.
    CursorPosition,
    /// Move and resize a window through a KWin script.
    SetWindowGeometry {
        #[arg(long)]
        window: String,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long)]
        width: i32,
        #[arg(long)]
        height: i32,
    },
    /// Set or clear a window's keep-above flag through a KWin script.
    SetWindowKeepAbove {
        #[arg(long)]
        window: String,
        #[arg(long, action = clap::ArgAction::Set)]
        value: bool,
    },
    /// Focus and raise a window through a KWin script.
    ActivateWindow {
        #[arg(long)]
        window: String,
    },
    /// Preview the set of apps that would be hidden from capture for an action.
    PreviewHideSet {
        #[arg(long = "allowed-bundle-id")]
        allowed_bundle_ids: Vec<String>,
        #[arg(long, default_value = "com.anthropic.claude-code.cli-no-window")]
        host_bundle_id: String,
        #[arg(long)]
        display: Option<String>,
    },
    /// Enumerate launchable desktop applications from installed desktop entries.
    ListInstalledApps,
    /// Resolve a desktop-entry icon and return it as a data URL.
    GetAppIcon {
        #[arg(long)]
        target: String,
    },
    /// Launch an installed desktop application by bundle id, desktop id, name, or path.
    OpenApp {
        #[arg(long)]
        app: String,
    },
    /// Report the currently active app.
    FrontmostApp,
    /// Report the topmost app at a global logical point.
    AppUnderPoint {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Move the pointer to a global logical point.
    PointerMove {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Click at a global logical point without executor-side allowlist checks.
    PointerClick {
        #[arg(long = "modifier")]
        modifiers: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Scroll at a global logical point without executor-side allowlist checks.
    PointerScroll {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, allow_hyphen_values = true, default_value_t = 0.0)]
        dx: f64,
        #[arg(long, allow_hyphen_values = true, default_value_t = 0.0)]
        dy: f64,
    },
    /// Drag between global logical points without executor-side allowlist checks.
    PointerDrag {
        #[arg(long, allow_hyphen_values = true)]
        from_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        from_y: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_y: i32,
    },
    /// Send an executor-style key sequence such as `ctrl+c`.
    KeySequence {
        #[arg(long)]
        keys: String,
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Type text through the portal using keysyms instead of keycode chords.
    Type {
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = 12)]
        delay_ms: u64,
    },
    /// Hold one or more keys for a fixed duration in milliseconds.
    HoldKey {
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        #[arg(long)]
        duration_ms: u64,
    },
    /// Read text from the local clipboard while the session lock is active.
    ReadClipboard,
    /// Write text to the local clipboard while the session lock is active.
    WriteClipboard {
        #[arg(long)]
        text: String,
    },
    /// Press and hold the left mouse button until explicitly released.
    LeftMouseDown,
    /// Release the left mouse button if it is currently held.
    LeftMouseUp,
    /// Persistently mark disallowed windows as excludeFromCapture until restored.
    PrepareForAction {
        #[arg(long = "allowed-bundle-id")]
        allowed_bundle_ids: Vec<String>,
        #[arg(long, default_value = "com.anthropic.claude-code.cli-no-window")]
        host_bundle_id: String,
        #[arg(long)]
        display: Option<String>,
    },
    /// Restore windows previously hidden by `prepare-for-action`.
    RestorePrepareState,
    /// Capture a still frame through the portal/PipeWire path.
    Screenshot {
        #[arg(long)]
        display: Option<String>,
    },
    /// Capture a logical region within a display and return an executor-style zoom image.
    Zoom {
        #[arg(long)]
        display: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long)]
        w: i32,
        #[arg(long)]
        h: i32,
    },
    /// Serve the session daemon on a Unix socket (internal; spawned by `session-start`).
    #[command(hide = true)]
    ServeSession {
        #[arg(long)]
        socket: String,
    },
    /// Run the layer-shell session indicator overlay (internal; spawned by the session daemon).
    #[command(hide = true)]
    SessionOverlay {
        #[arg(long)]
        output: Option<String>,
    },
    /// Move the session indicator overlay to a specific display.
    SetOverlayDisplay {
        #[arg(long)]
        display: Option<String>,
    },
    /// Serve the teach overlay controller on a Unix socket (internal; spawned on demand).
    #[command(hide = true)]
    ServeTeachOverlay {
        #[arg(long)]
        socket: String,
    },
    /// Show a teach step from a JSON payload and wait for the user's response.
    TeachStep {
        #[arg(long)]
        payload: String,
        #[arg(long)]
        display: Option<String>,
    },
    /// Switch the teach overlay to its working indicator.
    TeachWorking,
    /// Hide the teach overlay.
    TeachHide,
    /// Move the teach overlay to a specific display.
    TeachDisplay {
        #[arg(long)]
        display: String,
    },
    /// Block until the teach overlay reports the next user event.
    TeachWaitEvent,
    /// Render a teach overlay preview from a JSON payload without a running session.
    TeachOverlayPreview {
        #[arg(long)]
        payload: String,
        #[arg(long)]
        display: Option<String>,
        #[arg(long, default_value_t = false)]
        working: bool,
        #[arg(long)]
        auto_exit_ms: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        let mut argv = vec!["kwin-portal-bridge"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
            .unwrap_or_else(|error| panic!("{args:?} did not parse: {error}"))
            .command
    }

    #[test]
    fn pointer_flags_accept_negative_coordinates() {
        assert!(matches!(
            parse(&["pointer-move", "--x", "-5", "--y", "-1080"]),
            Command::PointerMove { x: -5, y: -1080 }
        ));
        assert!(matches!(
            parse(&["pointer-click", "--x", "-1", "--y", "-2"]),
            Command::PointerClick { x: -1, y: -2, .. }
        ));
        assert!(matches!(
            parse(&["app-under-point", "--x", "-3", "--y", "-4"]),
            Command::AppUnderPoint { x: -3, y: -4 }
        ));
    }

    #[test]
    fn drag_endpoints_accept_negative_coordinates() {
        assert!(matches!(
            parse(&[
                "pointer-drag",
                "--from-x",
                "-10",
                "--from-y",
                "-20",
                "--to-x",
                "-30",
                "--to-y",
                "-40",
            ]),
            Command::PointerDrag {
                from_x: -10,
                from_y: -20,
                to_x: -30,
                to_y: -40,
            }
        ));
    }

    #[test]
    fn scroll_position_and_deltas_accept_negative_values() {
        let Command::PointerScroll { x, y, dx, dy } = parse(&[
            "pointer-scroll",
            "--x",
            "-5",
            "--y",
            "-6",
            "--dx",
            "-1.5",
            "--dy",
            "-3",
        ]) else {
            panic!("expected pointer-scroll");
        };
        assert_eq!((x, y), (-5, -6));
        assert_eq!((dx, dy), (-1.5, -3.0));
    }

    #[test]
    fn region_and_window_origins_accept_negative_values() {
        assert!(matches!(
            parse(&["zoom", "--x", "-7", "--y", "-8", "--w", "10", "--h", "10"]),
            Command::Zoom { x: -7, y: -8, .. }
        ));
        assert!(matches!(
            parse(&[
                "set-window-geometry",
                "--window",
                "w",
                "--x",
                "-1920",
                "--y",
                "-1",
                "--width",
                "100",
                "--height",
                "100",
            ]),
            Command::SetWindowGeometry {
                x: -1920,
                y: -1,
                ..
            }
        ));
    }

    #[test]
    fn a_non_numeric_hyphen_value_is_still_a_usage_error() {
        let error = Cli::try_parse_from(["kwin-portal-bridge", "pointer-move", "--x", "--y", "5"])
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }
}
