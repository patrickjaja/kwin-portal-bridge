use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use wl_clipboard_rs::copy::{
    MimeType as CopyMimeType, Options as CopyOptions, Source as CopySource,
};
use wl_clipboard_rs::paste::{ClipboardType, MimeType as PasteMimeType, Seat, get_contents};

use crate::capture::{screenshot_result_from_frame, zoom_result_from_frame};
use crate::executor::ExecutorBackend;
use crate::kwin::KWinBackend;
use crate::model::{
    ClipboardReadResult, ClipboardWriteResult, PortalSessionInfo, ScreenInfo, ScreenshotCapture,
    ScreenshotResult,
};
use crate::portal::LivePortalSession;
use crate::session_overlay::SessionOverlayProcess;
use crate::teach_overlay::TeachOverlayProcess;

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionRequest {
    SessionInfo,
    Shutdown,
    MovePointerScreenPoint {
        screen: ScreenInfo,
        x: i32,
        y: i32,
    },
    LeftMouseDown,
    LeftMouseUp,
    TypeText {
        text: String,
        delay_ms: u64,
    },
    ClickScreenPoint {
        screen: ScreenInfo,
        x: i32,
        y: i32,
        button: i32,
        count: u32,
        keycodes: Vec<i32>,
    },
    ScrollScreenPoint {
        screen: ScreenInfo,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    },
    KeySequence {
        keycodes: Vec<i32>,
        repeat: u32,
    },
    HoldKeyCodes {
        keycodes: Vec<i32>,
        duration_ms: u64,
    },
    DragScreenPoints {
        from_screen: ScreenInfo,
        from_x: i32,
        from_y: i32,
        to_screen: ScreenInfo,
        to_x: i32,
        to_y: i32,
    },
    CaptureStillFrame {
        screen: ScreenInfo,
    },
    CaptureZoom {
        screen: ScreenInfo,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    SetOverlayDisplay {
        display: Option<String>,
    },
    ReadClipboard,
    WriteClipboard {
        text: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn start_session_daemon() -> Result<PortalSessionInfo> {
    if let Ok(info) = request::<PortalSessionInfo>(SessionRequest::SessionInfo).await {
        bail!("a portal session is already active: {}", info.session_id);
    }

    let socket = prepare_session_socket().await?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(current_exe);
    command
        .arg("serve-session")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().context("failed to spawn session daemon")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match request::<PortalSessionInfo>(SessionRequest::SessionInfo).await {
            Ok(info) => return Ok(info),
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(error).context("session daemon did not become ready in time");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

pub async fn stop_session_daemon() -> Result<()> {
    let _: Value = request(SessionRequest::Shutdown).await?;
    Ok(())
}

pub async fn prepare_session_socket() -> Result<PathBuf> {
    if let Ok(info) = request::<PortalSessionInfo>(SessionRequest::SessionInfo).await {
        bail!("a portal session is already active: {}", info.session_id);
    }

    let socket = socket_path()?;
    cleanup_stale_socket(&socket).await?;
    Ok(socket)
}

pub async fn open_session_daemon(
    socket: &Path,
) -> Result<(
    UnixListener,
    LivePortalSession,
    SessionOverlayProcess,
    TeachOverlayProcess,
)> {
    if socket.exists() {
        std::fs::remove_file(socket).with_context(|| {
            format!(
                "failed to remove stale session socket `{}`",
                socket.display()
            )
        })?;
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create session socket directory `{}`",
                parent.display()
            )
        })?;
    }

    let listener = UnixListener::bind(socket)
        .with_context(|| format!("failed to bind session socket `{}`", socket.display()))?;
    let session = match LivePortalSession::open().await {
        Ok(session) => session,
        Err(error) => {
            std::fs::remove_file(socket).ok();
            return Err(error);
        }
    };
    let overlay = match SessionOverlayProcess::spawn(None) {
        Ok(overlay) => overlay,
        Err(error) => {
            session.shutdown().await.ok();
            std::fs::remove_file(socket).ok();
            return Err(error);
        }
    };
    let teach_overlay = match TeachOverlayProcess::spawn() {
        Ok(teach_overlay) => teach_overlay,
        Err(error) => {
            let mut overlay = overlay;
            overlay.shutdown();
            session.shutdown().await.ok();
            std::fs::remove_file(socket).ok();
            return Err(error);
        }
    };
    Ok((listener, session, overlay, teach_overlay))
}

pub async fn serve_session_daemon(socket: PathBuf) -> Result<()> {
    let (listener, session, overlay, teach_overlay) = open_session_daemon(&socket).await?;
    serve_open_session(socket, listener, session, overlay, teach_overlay).await
}

pub async fn serve_open_session(
    socket: PathBuf,
    listener: UnixListener,
    mut session: LivePortalSession,
    mut overlay: SessionOverlayProcess,
    mut teach_overlay: TeachOverlayProcess,
) -> Result<()> {
    let serve_result = async {
        let mut capture_drain_tick = tokio::time::interval(Duration::from_millis(100));
        capture_drain_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        #[cfg(unix)]
        let mut terminate_signal =
            signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
        #[cfg(unix)]
        let terminate = async {
            terminate_signal.recv().await;
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        tokio::pin!(terminate);

        loop {
            tokio::select! {
                _ = capture_drain_tick.tick() => {
                    session.drain_capture_backlog();
                }
                _ = &mut terminate => {
                    break;
                }
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.context("failed to accept IPC client")?;
                    let request = read_request(&mut stream).await?;
                    let (response, should_shutdown) = handle_request(&mut session, &mut overlay, request).await;
                    write_response(&mut stream, response).await?;
                    if should_shutdown {
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    restore_prepare_state().ok();
    teach_overlay.shutdown();
    overlay.shutdown();
    session.shutdown().await.ok();
    std::fs::remove_file(&socket).ok();
    serve_result
}

pub async fn request<T: DeserializeOwned>(request: SessionRequest) -> Result<T> {
    let socket = socket_path()?;
    if !socket.exists() {
        bail!("no active portal session");
    }

    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("failed to connect to session socket `{}`", socket.display()))?;
    let payload = serde_json::to_vec(&request)?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write session IPC request")?;
    stream
        .shutdown()
        .await
        .context("failed to finish writing session IPC request")?;

    let mut response_bytes = Vec::new();
    stream
        .read_to_end(&mut response_bytes)
        .await
        .context("failed to read session IPC response")?;
    let response: SessionResponse =
        serde_json::from_slice(&response_bytes).context("failed to decode session IPC response")?;

    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "session IPC request failed".to_owned())
        );
    }

    let value = response.result.unwrap_or(Value::Null);
    serde_json::from_value(value).context("failed to decode session IPC payload")
}

pub fn socket_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    Ok(base.join("kwin-portal-bridge").join("session.sock"))
}

async fn cleanup_stale_socket(socket: &Path) -> Result<()> {
    if !socket.exists() {
        return Ok(());
    }

    match UnixStream::connect(socket).await {
        Ok(_) => {}
        Err(_) => {
            std::fs::remove_file(socket).with_context(|| {
                format!(
                    "failed to remove stale session socket `{}`",
                    socket.display()
                )
            })?;
        }
    }

    Ok(())
}

async fn read_request(stream: &mut UnixStream) -> Result<SessionRequest> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .context("failed to read session IPC request")?;
    serde_json::from_slice(&bytes).context("failed to decode session IPC request")
}

async fn write_response(stream: &mut UnixStream, response: SessionResponse) -> Result<()> {
    let bytes = serde_json::to_vec(&response)?;
    stream
        .write_all(&bytes)
        .await
        .context("failed to write session IPC response")
}

async fn handle_request(
    session: &mut LivePortalSession,
    overlay: &mut SessionOverlayProcess,
    request: SessionRequest,
) -> (SessionResponse, bool) {
    let result = match request {
        SessionRequest::SessionInfo => respond_ok(session.info()),
        SessionRequest::Shutdown => {
            return (
                SessionResponse {
                    ok: true,
                    result: Some(Value::Null),
                    error: None,
                },
                true,
            );
        }
        SessionRequest::MovePointerScreenPoint { screen, x, y } => {
            respond_async(session.move_pointer_screen_point(&screen, x, y).await)
        }
        SessionRequest::LeftMouseDown => respond_async(session.left_mouse_down().await),
        SessionRequest::LeftMouseUp => respond_async(session.left_mouse_up().await),
        SessionRequest::TypeText { text, delay_ms } => {
            respond_async(session.type_text(&text, delay_ms).await)
        }
        SessionRequest::ClickScreenPoint {
            screen,
            x,
            y,
            button,
            count,
            keycodes,
        } => respond_async(
            session
                .click_screen_point(&screen, x, y, button, count, &keycodes)
                .await,
        ),
        SessionRequest::ScrollScreenPoint {
            screen,
            x,
            y,
            dx,
            dy,
        } => respond_async(session.scroll_screen_point(&screen, x, y, dx, dy).await),
        SessionRequest::KeySequence { keycodes, repeat } => {
            respond_async(session.key_sequence(&keycodes, repeat).await)
        }
        SessionRequest::HoldKeyCodes {
            keycodes,
            duration_ms,
        } => respond_async(session.hold_key_codes(&keycodes, duration_ms).await),
        SessionRequest::DragScreenPoints {
            from_screen,
            from_x,
            from_y,
            to_screen,
            to_x,
            to_y,
        } => respond_async(
            session
                .drag_screen_points(&from_screen, from_x, from_y, &to_screen, to_x, to_y)
                .await,
        ),
        SessionRequest::CaptureStillFrame { screen } => {
            respond_async(capture_still(session, &screen).await)
        }
        SessionRequest::CaptureZoom { screen, x, y, w, h } => {
            respond_async(capture_zoom(session, &screen, x, y, w, h).await)
        }
        SessionRequest::SetOverlayDisplay { display } => {
            respond_async(set_overlay_display(overlay, display.as_deref()))
        }
        SessionRequest::ReadClipboard => respond_async(read_clipboard().await),
        SessionRequest::WriteClipboard { text } => respond_async(write_clipboard(text).await),
    };

    (result, false)
}

fn set_overlay_display(
    overlay: &mut SessionOverlayProcess,
    display: Option<&str>,
) -> Result<Value> {
    overlay.set_output(display)?;
    Ok(Value::Null)
}

async fn capture_still(
    session: &mut LivePortalSession,
    screen: &ScreenInfo,
) -> Result<ScreenshotResult> {
    let captured = session.capture_screen_frame(screen).await?;
    screenshot_result_from_frame(screen, &captured.frame)
}

async fn capture_zoom(
    session: &mut LivePortalSession,
    screen: &ScreenInfo,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<ScreenshotCapture> {
    let captured = session.capture_screen_frame(screen).await?;
    zoom_result_from_frame(screen, &captured.frame, x, y, w, h)
}

fn respond_ok<T: Serialize>(value: T) -> SessionResponse {
    match serde_json::to_value(value) {
        Ok(result) => SessionResponse {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => SessionResponse {
            ok: false,
            result: None,
            error: Some(format!("failed to serialize session response: {error}")),
        },
    }
}

fn restore_prepare_state() -> Result<()> {
    let executor = ExecutorBackend::new().context("failed to create executor backend")?;
    let kwin = KWinBackend::new();
    executor
        .restore_prepare_state(&kwin)
        .context("failed to restore prepare state")?;
    Ok(())
}

fn respond_async<T: Serialize>(result: Result<T>) -> SessionResponse {
    match result {
        Ok(value) => respond_ok(value),
        Err(error) => SessionResponse {
            ok: false,
            result: None,
            error: Some(format!("{error:#}")),
        },
    }
}

async fn read_clipboard() -> Result<ClipboardReadResult> {
    let text = tokio::task::spawn_blocking(read_clipboard_sync)
        .await
        .context("clipboard read worker task failed to join")??;

    Ok(ClipboardReadResult {
        action: "read-clipboard".to_owned(),
        text,
    })
}

async fn write_clipboard(text: String) -> Result<ClipboardWriteResult> {
    let text_for_write = text.clone();
    tokio::task::spawn_blocking(move || write_clipboard_sync(&text_for_write))
        .await
        .context("clipboard write worker task failed to join")??;

    for _ in 0..20 {
        let observed = tokio::task::spawn_blocking(read_clipboard_sync)
            .await
            .context("clipboard verification worker task failed to join")?;
        if let Ok(observed) = observed
            && observed == text
        {
            return Ok(ClipboardWriteResult {
                action: "write-clipboard".to_owned(),
                char_count: text.chars().count(),
                text,
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(ClipboardWriteResult {
        action: "write-clipboard".to_owned(),
        char_count: text.chars().count(),
        text,
    })
}

fn read_clipboard_sync() -> Result<String> {
    use std::io::Read;

    let (mut pipe, _) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        PasteMimeType::Text,
    )
    .context("failed to read clipboard through wl-clipboard-rs")?;

    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)
        .context("failed to read clipboard bytes")?;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }

    String::from_utf8(bytes).context("clipboard contents were not valid UTF-8")
}

fn write_clipboard_sync(text: &str) -> Result<()> {
    CopyOptions::new()
        .copy(
            CopySource::Bytes(text.as_bytes().to_vec().into()),
            CopyMimeType::Text,
        )
        .context("failed to write clipboard through wl-clipboard-rs")
}
